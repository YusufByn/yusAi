//! Les refines de Prime deviennent des leçons yusAi : conversion des edits
//! en opérations (`refinement_ops`, fonction pure), puis import dans le
//! magasin (`import_refinement_outcome`), en direct depuis le relais
//! d'événements ou en rattrapage depuis l'historique d'un fil
//! (`import_thread_outcomes`).
//!
//! Une refine arrive en ligne `custom` `refinement_outcome`, dont les
//! `details` portent `refinementId`, `summary`, `scope`, `edits` et, pour un
//! retour arrière, `rollbackOf` (pa-core/src/session_engine/refine.rs:125-145).
//! Chaque edit est un `AppliedRefinementEdit` : `action`
//! (`create|update|delete`), `kind` (`prompt|memory|skill|subagent`), `id`,
//! `title`/`content` prévus, `before`/`after` (l'entrée du harness avant et
//! après), `applied` (pa-core/src/refinement/mod.rs:286-330).
//!
//! Règles (décisions dans CONTEXT.md) :
//! - seules les edits appliquées comptent ;
//! - `memory`, `prompt`, `subagent` deviennent des leçons ; `skill` (une
//!   simple référence Python) devient une proposition de skill ;
//! - une création arrive toujours au niveau projet (le niveau demandé à
//!   Prime ne compte pas) ;
//! - une mise à jour ou une suppression visant une de nos leçons (id
//!   `yl_…`, amorcée dans le harness avant la refine) s'applique si la leçon
//!   est au niveau projet ; au niveau type ou global, elle devient une
//!   proposition en attente de validation ;
//! - une mise à jour d'une entrée qui n'est pas à nous devient une création
//!   (le magasin écarte le doublon) ; sa suppression est ignorée.
//!
//! La conversion est pure : le niveau de nos leçons vient de l'appelant.

use std::path::{Path, PathBuf};
use std::sync::{Mutex, PoisonError};

use anyhow::{anyhow, Context, Result};
use pa_core::refinement::{HarnessEntry, HarnessScope, RefinementKind};
use pa_core::session::manager::format_iso;
use serde::Serialize;
use serde_json::{json, Value};
use sinew_app::store::{
    AppStore, InsertLessonOutcome, Lesson, LessonKind, LessonLevel, LessonOrigin, LessonScope,
    LessonStatus, NewLesson, NewProposal, ProposalKind,
};

/// Préfixe des ids de nos leçons, tels qu'amorcés dans le harness de Prime.
pub const LESSON_ID_PREFIX: &str = "yl_";

/// Une opération sur le magasin des leçons.
#[derive(Debug, Clone, PartialEq)]
pub enum LessonOp {
    /// Nouvelle leçon au niveau projet. `entry_id` est l'entrée du harness
    /// de Prime qui l'a portée, retirée du harness une fois importée.
    Create {
        entry_id: String,
        kind: LessonKind,
        title: String,
        content: String,
    },
    /// Nouvelle version d'une leçon de niveau projet.
    Update {
        lesson_id: String,
        title: String,
        content: String,
    },
    /// Archivage d'une leçon de niveau projet.
    Archive { lesson_id: String },
    /// Proposition soumise à validation.
    Propose {
        lesson_id: Option<String>,
        kind: ProposalKind,
        target_level: Option<LessonLevel>,
        payload: Value,
    },
}

/// Ce qu'une refine demande au magasin.
#[derive(Debug, Clone, PartialEq)]
pub struct RefinementImport {
    pub refinement_id: String,
    pub summary: String,
    pub rollback_of: Option<String>,
    pub ops: Vec<LessonOp>,
    /// Les edits écartées, avec la raison (journal, tests).
    pub skipped: Vec<String>,
}

/// Les opérations d'une ligne `refinement_outcome` (ses `details`).
/// `level_of` donne le niveau d'une de nos leçons encore active, ou `None`.
pub fn refinement_ops(
    details: &Value,
    level_of: impl Fn(&str) -> Option<LessonLevel>,
) -> Result<RefinementImport> {
    let refinement_id = details
        .get("refinementId")
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())
        .ok_or_else(|| anyhow!("refinement outcome without refinementId: {details}"))?
        .to_string();
    let mut import = RefinementImport {
        refinement_id,
        summary: text(details.get("summary")).unwrap_or_default(),
        rollback_of: text(details.get("rollbackOf")),
        ops: Vec::new(),
        skipped: Vec::new(),
    };
    for edit in details
        .get("edits")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        match edit_op(edit, &import, &level_of) {
            Ok(op) => import.ops.push(op),
            Err(reason) => import.skipped.push(reason),
        }
    }
    Ok(import)
}

fn edit_op(
    edit: &Value,
    import: &RefinementImport,
    level_of: &impl Fn(&str) -> Option<LessonLevel>,
) -> std::result::Result<LessonOp, String> {
    let action = edit.get("action").and_then(Value::as_str).unwrap_or("");
    let kind = edit.get("kind").and_then(Value::as_str).unwrap_or("");
    let id = edit.get("id").and_then(Value::as_str).unwrap_or("");
    let label = format!("{action} {kind}:{id}");
    if edit.get("applied").and_then(Value::as_bool) != Some(true) {
        // Prime dit pourquoi dans `error` (pa-core/src/refinement/mod.rs:314-315).
        return Err(match text(edit.get("error")) {
            Some(error) => format!("{label}: not applied ({error})"),
            None => format!("{label}: not applied"),
        });
    }
    if !matches!(action, "create" | "update" | "delete") {
        return Err(format!("{label}: unknown action"));
    }

    if kind == "skill" {
        return Ok(LessonOp::Propose {
            lesson_id: None,
            kind: ProposalKind::Skill,
            target_level: None,
            payload: json!({
                "action": action,
                "id": id,
                "entry": edit.get("after").or_else(|| edit.get("before")).cloned().unwrap_or(Value::Null),
                "reference": edit.get("reference").cloned().unwrap_or(Value::Null),
                "arguments": edit.get("arguments").cloned().unwrap_or(Value::Null),
                "refinementSummary": import.summary,
            }),
        });
    }
    let lesson_kind = match kind {
        "memory" => LessonKind::Memory,
        "prompt" => LessonKind::Prompt,
        "subagent" => LessonKind::Subagent,
        _ => return Err(format!("{label}: unknown kind")),
    };

    let ours = id.starts_with(LESSON_ID_PREFIX).then_some(id);
    let current_level = ours.and_then(level_of);
    if action == "delete" {
        let Some(lesson_id) = ours.filter(|_| current_level.is_some()) else {
            return Err(format!("{label}: not one of our active lessons"));
        };
        return Ok(match current_level {
            Some(LessonLevel::Project) => LessonOp::Archive {
                lesson_id: lesson_id.to_string(),
            },
            level => LessonOp::Propose {
                lesson_id: Some(lesson_id.to_string()),
                kind: ProposalKind::Archive,
                target_level: level,
                payload: json!({ "refinementSummary": import.summary }),
            },
        });
    }

    let (title, content) = entry_text(edit).ok_or_else(|| format!("{label}: empty entry"))?;
    match (action, ours, current_level) {
        ("update", Some(lesson_id), Some(LessonLevel::Project)) => Ok(LessonOp::Update {
            lesson_id: lesson_id.to_string(),
            title,
            content,
        }),
        ("update", Some(lesson_id), Some(level)) => Ok(LessonOp::Propose {
            lesson_id: Some(lesson_id.to_string()),
            kind: ProposalKind::Change,
            target_level: Some(level),
            payload: json!({
                "title": title,
                "content": content,
                "refinementSummary": import.summary,
            }),
        }),
        // Création, ou mise à jour d'une entrée qui n'est pas (ou plus) une
        // de nos leçons actives : une leçon projet de plus.
        _ => Ok(LessonOp::Create {
            entry_id: id.to_string(),
            kind: lesson_kind,
            title,
            content,
        }),
    }
}

/// Le titre et le contenu après l'edit : l'entrée `after` de Prime, sinon
/// les champs prévus de l'edit. `None` si le contenu est vide.
fn entry_text(edit: &Value) -> Option<(String, String)> {
    let after = edit.get("after");
    let pick = |field: &str| {
        after
            .and_then(|entry| text(entry.get(field)))
            .or_else(|| text(edit.get(field)))
    };
    let content = pick("content")?;
    let title = pick("title").unwrap_or_else(|| first_words(&content));
    Some((title, content))
}

/// La conversation d'où vient une refine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThreadContext {
    pub conversation_id: String,
    /// Le projet : `workspace_id` des conversations (son chemin).
    pub workspace_id: String,
}

/// Ce qu'un import a fait dans le magasin.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ImportReport {
    pub refinement_id: String,
    pub created: Vec<String>,
    /// Leçons existantes qui portaient déjà le texte d'une création.
    pub duplicates: Vec<String>,
    pub updated: Vec<String>,
    pub archived: Vec<String>,
    pub proposals: Vec<String>,
    /// Edits écartées dès la conversion (non appliquées par Prime…).
    pub skipped: Vec<String>,
    /// Opérations qui ont échoué dans le magasin, gardées avec la refine
    /// (`ImportedRefinement::failures`) ; leurs entrées restent dans le
    /// harness.
    pub failed: Vec<String>,
    /// Entrées retirées du harness de Prime après l'import.
    pub removed_entries: usize,
}

/// Une refine locale du worker écrit dans `harness/` à côté des fichiers
/// de fil (`<agent_dir>/yusai-threads/harness/`), une refine globale dans
/// `agent_dir` lui-même : chemins figés par le test e2e
/// `refine_runs_scripted_and_append_system_prompt_survives_a_worker_restart`
/// (pa-core/src/session_engine/refine.rs:237-240,
/// pa-daemon/src/agent_engine/session_engine_impl.rs:1289).
pub fn refine_harness_file(agent_dir: &Path, global: bool) -> PathBuf {
    if global {
        agent_dir.join(HARNESS_STATE_FILE)
    } else {
        agent_dir
            .join(crate::prime_session::THREADS_DIR)
            .join("harness")
            .join(HARNESS_STATE_FILE)
    }
}

const HARNESS_STATE_FILE: &str = "harness_state.json";

/// Les imports, l'amorçage et le retrait de nos leçons passent un par un :
/// magasin et fichiers du harness partagés.
static IMPORT_LOCK: Mutex<()> = Mutex::new(());

/// Importe une ligne `refinement_outcome` dans le magasin, une seule fois
/// par `refinementId` (`None` si elle l'était déjà).
///
/// Une refine de portée globale (`refine.run(global_=True)` par le modèle)
/// arrive au niveau projet comme les autres, avec une proposition de montée
/// vers global pour chaque leçon créée (ou déjà présente sous un autre
/// niveau).
///
/// Une fois l'import noté en base, et seulement alors, les entrées que
/// cette refine a créées dans le harness de Prime en sont retirées : elles
/// vivent désormais chez nous, et le fichier local est partagé par tous nos
/// fils (sinon toute refine suivante, de n'importe quel projet, les
/// relirait). Une création qui échoue garde son entrée dans le fichier ;
/// chaque échec est noté avec la refine. Les erreurs du retrait ne font pas
/// échouer l'import.
pub fn import_refinement_outcome(
    store: &AppStore,
    agent_dir: &Path,
    thread: &ThreadContext,
    details: &Value,
) -> Result<Option<ImportReport>> {
    let _guard = IMPORT_LOCK.lock().unwrap_or_else(PoisonError::into_inner);
    let refinement_id = details
        .get("refinementId")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if !refinement_id.is_empty() && store.is_refinement_imported(refinement_id)? {
        return Ok(None);
    }
    let import = refinement_ops(details, |id| {
        store
            .lesson(id)
            .ok()
            .flatten()
            .filter(|lesson| lesson.status == LessonStatus::Active)
            .map(|lesson| lesson.level)
    })?;
    let global = details.get("scope").and_then(Value::as_str) == Some("global");
    let origin = LessonOrigin {
        actor: if global { "refine:global" } else { "refine" }.to_string(),
        conversation_id: Some(thread.conversation_id.clone()),
        refinement_id: Some(import.refinement_id.clone()),
    };
    let mut report = ImportReport {
        refinement_id: import.refinement_id.clone(),
        skipped: import.skipped.clone(),
        ..ImportReport::default()
    };
    let touched = apply_ops(store, thread, import.ops, &origin, global, &mut report);
    store.mark_refinement_imported(
        &report.refinement_id,
        Some(&thread.conversation_id),
        &report.failed,
    )?;

    let harness = refine_harness_file(agent_dir, global);
    match remove_harness_entries(&harness, &touched) {
        Ok(removed) => report.removed_entries = removed,
        Err(error) => tracing::warn!(
            error = %error,
            file = %harness.display(),
            "imported prime refine entries stay in the harness"
        ),
    }
    Ok(Some(report))
}

/// Applique des opérations au magasin pour une conversation. Les créations
/// arrivent au niveau projet ; avec `promote`, chaque leçon créée (ou déjà
/// présente) reçoit une proposition de montée vers global. Les échecs vont
/// dans `report.failed`. Renvoie les entrées du harness (`kind`, `id`) des
/// créations réussies, les seules à retirer du harness.
fn apply_ops(
    store: &AppStore,
    thread: &ThreadContext,
    ops: Vec<LessonOp>,
    origin: &LessonOrigin,
    promote: bool,
    report: &mut ImportReport,
) -> Vec<(&'static str, String)> {
    let mut touched = Vec::new();
    for op in ops {
        match op {
            LessonOp::Create {
                entry_id,
                kind,
                title,
                content,
            } => {
                let lesson = NewLesson {
                    level: LessonLevel::Project,
                    workspace_id: Some(thread.workspace_id.clone()),
                    project_type: None,
                    kind,
                    title,
                    content,
                };
                let lesson_id = match store.insert_lesson(&lesson, origin) {
                    Ok(InsertLessonOutcome::Created(created)) => {
                        report.created.push(created.id.clone());
                        created.id
                    }
                    Ok(InsertLessonOutcome::Duplicate { existing_id }) => {
                        report.duplicates.push(existing_id.clone());
                        existing_id
                    }
                    // L'entrée reste dans le harness : elle n'est pas chez nous.
                    Err(error) => {
                        report.failed.push(format!("create {entry_id}: {error:#}"));
                        continue;
                    }
                };
                if promote {
                    match propose_global(store, &lesson_id, origin) {
                        Ok(Some(proposal)) => report.proposals.push(proposal),
                        Ok(None) => {}
                        Err(error) => {
                            report
                                .failed
                                .push(format!("promote {lesson_id}: {error:#}"));
                            continue;
                        }
                    }
                }
                touched.push((harness_kind(kind), entry_id));
            }
            LessonOp::Update {
                lesson_id,
                title,
                content,
            } => match store.update_lesson(&lesson_id, &title, &content, origin) {
                Ok(_) => report.updated.push(lesson_id),
                Err(error) => report.failed.push(format!("update {lesson_id}: {error:#}")),
            },
            LessonOp::Archive { lesson_id } => match store.archive_lesson(&lesson_id, origin) {
                Ok(_) => report.archived.push(lesson_id),
                Err(error) => report
                    .failed
                    .push(format!("archive {lesson_id}: {error:#}")),
            },
            LessonOp::Propose {
                lesson_id,
                kind,
                target_level,
                payload,
            } => match store.create_lesson_proposal(&NewProposal {
                lesson_id,
                kind,
                target_level,
                payload,
                conversation_id: origin.conversation_id.clone(),
                refinement_id: origin.refinement_id.clone(),
            }) {
                Ok(proposal) => report.proposals.push(proposal.id),
                Err(error) => report.failed.push(format!("proposal: {error:#}")),
            },
        }
    }
    touched
}

/// Le harness global que lisent le noyau (`rlm.harness.*(…, global_=True)`,
/// prime-agent-runtime/src/rlm/harness.py:153-166) et le digest du prompt
/// de toutes les sessions (pa-core/src/session_engine/engine.rs:548).
pub fn global_harness_file(agent_dir: &Path) -> PathBuf {
    agent_dir.join("harness").join(HARNESS_STATE_FILE)
}

/// Garde contre les écritures globales directes du modèle : chaque entrée
/// du harness global devient une leçon de niveau projet de la conversation
/// donnée (celle dont une cellule vient de finir), avec une proposition de
/// montée vers global ; une entrée `skill` devient une proposition de
/// skill. Une entrée ne quitte le fichier qu'une fois importée sans échec :
/// sinon Prime la réinjecterait dans toutes les sessions, sans validation.
///
/// L'import est noté comme une refine (`harness-global-<ms>`, avec ses
/// échecs). `None` si aucune entrée n'a été importée, proposée ou n'a
/// échoué.
pub fn import_global_harness_writes(
    store: &AppStore,
    agent_dir: &Path,
    thread: &ThreadContext,
) -> Result<Option<ImportReport>> {
    let _guard = IMPORT_LOCK.lock().unwrap_or_else(PoisonError::into_inner);
    let file = global_harness_file(agent_dir);
    let text = match std::fs::read_to_string(&file) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).with_context(|| format!("read {}", file.display())),
    };
    let state: Value =
        serde_json::from_str(&text).with_context(|| format!("parse {}", file.display()))?;
    let mut entries: Vec<(&'static str, String, Value)> = Vec::new();
    for kind in ["memory", "prompt", "subagent", "skill"] {
        if let Some(records) = state
            .get("entries")
            .and_then(|entries| entries.get(kind))
            .and_then(Value::as_object)
        {
            for (id, entry) in records {
                entries.push((kind, id.clone(), entry.clone()));
            }
        }
    }
    if entries.is_empty() {
        return Ok(None);
    }
    let refinement_id = format!(
        "harness-global-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |elapsed| elapsed.as_millis())
    );
    let origin = LessonOrigin {
        actor: "harness:global".to_string(),
        conversation_id: Some(thread.conversation_id.clone()),
        refinement_id: Some(refinement_id.clone()),
    };
    let mut report = ImportReport {
        refinement_id: refinement_id.clone(),
        ..ImportReport::default()
    };
    let mut touched = Vec::new();
    for (kind, id, entry) in entries {
        // Une entrée = une création appliquée, convertie comme une edit de
        // refine.
        let details = json!({
            "refinementId": refinement_id,
            "summary": "written by the model to Prime's global harness",
            "edits": [{
                "action": "create",
                "kind": kind,
                "id": id,
                "reference": entry.get("reference").cloned().unwrap_or(Value::Null),
                "arguments": entry.get("arguments").cloned().unwrap_or(Value::Null),
                "after": entry,
                "applied": true,
            }],
        });
        let import = refinement_ops(&details, |_| None)?;
        report.skipped.extend(import.skipped);
        let failures = report.failed.len();
        let is_skill = import.ops.iter().any(|op| {
            matches!(
                op,
                LessonOp::Propose {
                    kind: ProposalKind::Skill,
                    ..
                }
            )
        });
        let created = apply_ops(store, thread, import.ops, &origin, true, &mut report);
        if report.failed.len() == failures {
            touched.extend(created);
            if is_skill {
                touched.push(("skill", id));
            }
        }
    }
    let nothing_done = report.created.is_empty()
        && report.duplicates.is_empty()
        && report.proposals.is_empty()
        && report.failed.is_empty();
    if nothing_done {
        // Seulement des entrées écartées (vides, inconnues) : pas de trace.
        return Ok(None);
    }
    store.mark_refinement_imported(
        &refinement_id,
        Some(&thread.conversation_id),
        &report.failed,
    )?;
    match remove_harness_entries(&file, &touched) {
        Ok(removed) => report.removed_entries = removed,
        Err(error) => tracing::warn!(
            error = %error,
            file = %file.display(),
            "imported global harness entries stay in the file"
        ),
    }
    Ok(Some(report))
}

/// Une proposition de montée vers global, sauf si la leçon y est déjà ou
/// si une proposition identique attend.
fn propose_global(
    store: &AppStore,
    lesson_id: &str,
    origin: &LessonOrigin,
) -> Result<Option<String>> {
    let Some(lesson) = store.lesson(lesson_id)? else {
        return Ok(None);
    };
    if lesson.level == LessonLevel::Global {
        return Ok(None);
    }
    let already = store
        .pending_lesson_proposals()?
        .into_iter()
        .any(|proposal| {
            proposal.lesson_id.as_deref() == Some(lesson_id)
                && proposal.kind == ProposalKind::Promote
                && proposal.target_level == Some(LessonLevel::Global)
        });
    if already {
        return Ok(None);
    }
    let proposal = store.create_lesson_proposal(&NewProposal {
        lesson_id: Some(lesson_id.to_string()),
        kind: ProposalKind::Promote,
        target_level: Some(LessonLevel::Global),
        payload: json!({ "reason": "global refine requested in the conversation" }),
        conversation_id: origin.conversation_id.clone(),
        refinement_id: origin.refinement_id.clone(),
    })?;
    Ok(Some(proposal.id))
}

fn harness_kind(kind: LessonKind) -> &'static str {
    match kind {
        LessonKind::Memory => "memory",
        LessonKind::Prompt => "prompt",
        LessonKind::Subagent => "subagent",
    }
}

/// Retire des entrées (`kind`, `id`) d'un fichier d'état du harness de
/// Prime (`{"schema":1,"entries":{kind:{id:entry}},"refinements":[…]}`,
/// pa-core/src/refinement/mod.rs:93-101), le reste intact. Écriture
/// atomique, seulement si quelque chose change. Renvoie le nombre
/// d'entrées retirées.
pub fn remove_harness_entries(path: &Path, entries: &[(&str, String)]) -> Result<usize> {
    if entries.is_empty() {
        return Ok(0);
    }
    edit_harness_file(path, false, |state| {
        let mut removed = 0;
        for (kind, id) in entries {
            if let Some(records) = state
                .get_mut("entries")
                .and_then(|entries| entries.get_mut(*kind))
                .and_then(Value::as_object_mut)
            {
                if records.remove(id).is_some() {
                    removed += 1;
                }
            }
        }
        removed
    })
}

/// Amorce nos leçons dans le harness local partagé avant une refine, pour
/// que le planificateur de Prime les voie et puisse les modifier ou les
/// supprimer (une edit sur une entrée absente échoue,
/// pa-core/src/refinement/planner.rs:347-372). Les entrées `yl_…` déjà
/// présentes (refine interrompue) sont d'abord retirées ; les autres
/// restent. Le planificateur affiche le `path` de chaque entrée
/// (pa-core/src/refinement/executor.rs:76-79) : il y lit le niveau de la
/// leçon. Renvoie le nombre de leçons amorcées.
pub fn seed_lessons(path: &Path, lessons: &[Lesson]) -> Result<usize> {
    let mut seeded = Vec::with_capacity(lessons.len());
    for lesson in lessons {
        let entry = HarnessEntry {
            id: lesson.id.clone(),
            kind: refinement_kind(lesson.kind),
            title: lesson.title.clone(),
            content: lesson.content.clone(),
            path: seeded_path(lesson),
            scope: Some(HarnessScope::Local),
            reference: serde_json::Map::new(),
            arguments: serde_json::Map::new(),
            metadata: serde_json::Map::new(),
            source: "yusai".to_string(),
            created_at: format_iso(lesson.created_at_ms),
            updated_at: format_iso(lesson.updated_at_ms),
            version: 1,
        };
        seeded.push((harness_kind(lesson.kind), serde_json::to_value(entry)?));
    }
    let changes = edit_harness_file(path, !seeded.is_empty(), |state| {
        if !state.is_object() {
            return 0;
        }
        let mut changes = drop_seeded(state);
        if !state.get("entries").is_some_and(Value::is_object) {
            state["entries"] = json!({});
        }
        for (kind, entry) in &seeded {
            let id = entry["id"].as_str().unwrap_or_default().to_string();
            let records = &mut state["entries"][*kind];
            if !records.is_object() {
                *records = json!({});
            }
            records[id.as_str()] = entry.clone();
            changes += 1;
        }
        changes
    })?;
    if changes == 0 && !seeded.is_empty() {
        return Err(anyhow!("{} is not a harness state", path.display()));
    }
    Ok(seeded.len())
}

/// Retire du harness local toutes nos entrées `yl_…`, après une refine ;
/// le reste du fichier ne bouge pas. Renvoie le nombre d'entrées retirées.
pub fn remove_seeded_lessons(path: &Path) -> Result<usize> {
    edit_harness_file(path, false, drop_seeded)
}

/// Amorce, sous le verrou des imports, les leçons qui s'appliquent à la
/// conversation (son projet, le type du projet, le global).
pub fn seed_thread_lessons(
    store: &AppStore,
    agent_dir: &Path,
    thread: &ThreadContext,
) -> Result<usize> {
    let _guard = IMPORT_LOCK.lock().unwrap_or_else(PoisonError::into_inner);
    let project_type = store
        .project_type(&thread.workspace_id)?
        .and_then(|setting| setting.project_type);
    let lessons = store.applicable_lessons(&LessonScope {
        workspace_id: thread.workspace_id.clone(),
        project_type,
    })?;
    seed_lessons(&refine_harness_file(agent_dir, false), &lessons)
}

/// [`remove_seeded_lessons`] sous le verrou des imports.
pub fn unseed_thread_lessons(agent_dir: &Path) -> Result<usize> {
    let _guard = IMPORT_LOCK.lock().unwrap_or_else(PoisonError::into_inner);
    remove_seeded_lessons(&refine_harness_file(agent_dir, false))
}

fn drop_seeded(state: &mut Value) -> usize {
    let mut removed = 0;
    if let Some(kinds) = state.get_mut("entries").and_then(Value::as_object_mut) {
        for records in kinds.values_mut().filter_map(Value::as_object_mut) {
            let before = records.len();
            records.retain(|id, _| !id.starts_with(LESSON_ID_PREFIX));
            removed += before - records.len();
        }
    }
    removed
}

/// `yusai/project`, `yusai/type/<type>` ou `yusai/global`.
fn seeded_path(lesson: &Lesson) -> String {
    match lesson.level {
        LessonLevel::Project => "yusai/project".to_string(),
        LessonLevel::Type => format!(
            "yusai/type/{}",
            lesson.project_type.as_deref().unwrap_or_default()
        ),
        LessonLevel::Global => "yusai/global".to_string(),
    }
}

fn refinement_kind(kind: LessonKind) -> RefinementKind {
    match kind {
        LessonKind::Memory => RefinementKind::Memory,
        LessonKind::Prompt => RefinementKind::Prompt,
        LessonKind::Subagent => RefinementKind::Subagent,
    }
}

/// Lit un fichier d'état du harness, le modifie et le réécrit (atomique,
/// 0o600) si `edit` signale un changement. Fichier absent : rien à faire,
/// sauf avec `create`, où l'on part d'un état vide.
fn edit_harness_file(
    path: &Path,
    create: bool,
    edit: impl FnOnce(&mut Value) -> usize,
) -> Result<usize> {
    let mut state = match std::fs::read_to_string(path) {
        Ok(text) => {
            serde_json::from_str(&text).with_context(|| format!("parse {}", path.display()))?
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound && create => {
            json!({ "schema": 1, "entries": {}, "refinements": [] })
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(error) => {
            return Err(error).with_context(|| format!("read {}", path.display()));
        }
    };
    let changes = edit(&mut state);
    if changes == 0 {
        return Ok(0);
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    }
    let content = format!("{}\n", serde_json::to_string_pretty(&state)?);
    let temp = path.with_extension(format!("json.yusai-{}.tmp", std::process::id()));
    {
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create(true).truncate(true);
        #[cfg(unix)]
        std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
        let mut file = options
            .open(&temp)
            .with_context(|| format!("write {}", temp.display()))?;
        std::io::Write::write_all(&mut file, content.as_bytes())?;
    }
    std::fs::rename(&temp, path).with_context(|| format!("replace {}", path.display()))?;
    Ok(changes)
}

/// Rattrapage à l'ouverture d'un fil : les lignes `refinement_outcome` de
/// son historique (`snapshot.messages`) pas encore importées (refines
/// faites quand rien n'écoutait : auto-refine de Prime, app fermée…).
pub fn import_thread_outcomes(
    store: &AppStore,
    agent_dir: &Path,
    thread: &ThreadContext,
    messages: &[Value],
) -> Vec<ImportReport> {
    let mut reports = Vec::new();
    for details in messages.iter().filter_map(refinement_outcome_details) {
        match import_refinement_outcome(store, agent_dir, thread, details) {
            Ok(Some(report)) => reports.push(report),
            Ok(None) => {}
            Err(error) => tracing::warn!(error = %error, "prime refine import failed"),
        }
    }
    reports
}

/// Les `details` d'un message `custom` `refinement_outcome`.
pub fn refinement_outcome_details(message: &Value) -> Option<&Value> {
    (message.get("customType").and_then(Value::as_str) == Some(REFINEMENT_OUTCOME))
        .then(|| message.get("details"))
        .flatten()
}

const REFINEMENT_OUTCOME: &str = "refinement_outcome";

fn text(value: Option<&Value>) -> Option<String> {
    value
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .map(str::to_string)
}

fn first_words(content: &str) -> String {
    content
        .split_whitespace()
        .take(8)
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(id: &str, kind: &str, title: &str, content: &str) -> Value {
        json!({
            "id": id, "kind": kind, "title": title, "content": content, "path": "general",
            "scope": "local", "reference": {}, "arguments": {}, "metadata": {},
            "source": "refine", "created_at": "t", "updated_at": "t", "version": 1,
        })
    }

    fn outcome(edits: Vec<Value>) -> Value {
        json!({
            "refinementId": "refine_1",
            "summary": "lessons from the session",
            "scope": "local",
            "edits": edits,
        })
    }

    fn levels(id: &str) -> Option<LessonLevel> {
        match id {
            "yl_project" => Some(LessonLevel::Project),
            "yl_type" => Some(LessonLevel::Type),
            "yl_global" => Some(LessonLevel::Global),
            _ => None,
        }
    }

    fn create_edit(kind: &str, id: &str, content: &str) -> Value {
        json!({
            "action": "create", "kind": kind, "id": id,
            "title": "Planned title", "content": content,
            "after": entry(id, kind, "Stored title", content),
            "applied": true,
        })
    }

    #[test]
    fn creations_of_each_kept_kind_become_project_lessons() {
        let import = refinement_ops(
            &outcome(vec![
                create_edit("memory", "tests", "Toujours lancer cargo test."),
                create_edit("prompt", "style", "Répondre brièvement."),
                create_edit("subagent", "reviewer", "Relire les diffs."),
            ]),
            levels,
        )
        .unwrap();
        assert_eq!(import.refinement_id, "refine_1");
        assert_eq!(import.summary, "lessons from the session");
        assert_eq!(
            import.ops,
            vec![
                LessonOp::Create {
                    entry_id: "tests".to_string(),
                    kind: LessonKind::Memory,
                    title: "Stored title".to_string(),
                    content: "Toujours lancer cargo test.".to_string(),
                },
                LessonOp::Create {
                    entry_id: "style".to_string(),
                    kind: LessonKind::Prompt,
                    title: "Stored title".to_string(),
                    content: "Répondre brièvement.".to_string(),
                },
                LessonOp::Create {
                    entry_id: "reviewer".to_string(),
                    kind: LessonKind::Subagent,
                    title: "Stored title".to_string(),
                    content: "Relire les diffs.".to_string(),
                },
            ]
        );
        assert!(import.skipped.is_empty());
    }

    #[test]
    fn a_global_scope_still_lands_at_project_level() {
        let mut details = outcome(vec![create_edit("memory", "g1", "Leçon globale.")]);
        details["scope"] = json!("global");
        let import = refinement_ops(&details, levels).unwrap();
        assert!(matches!(import.ops[0], LessonOp::Create { .. }));
    }

    #[test]
    fn skills_become_proposals() {
        let import = refinement_ops(
            &outcome(vec![json!({
                "action": "create", "kind": "skill", "id": "fmt",
                "title": "Format", "content": "Formater le code.",
                "reference": { "type": "python", "import": "fmt", "callable": "run" },
                "arguments": {},
                "after": entry("fmt", "skill", "Format", "Formater le code."),
                "applied": true,
            })]),
            levels,
        )
        .unwrap();
        let [LessonOp::Propose {
            lesson_id: None,
            kind: ProposalKind::Skill,
            target_level: None,
            payload,
        }] = import.ops.as_slice()
        else {
            panic!("expected a skill proposal: {:?}", import.ops);
        };
        assert_eq!(payload["action"], "create");
        assert_eq!(payload["reference"]["import"], "fmt");
        assert_eq!(payload["entry"]["content"], "Formater le code.");
    }

    #[test]
    fn updates_apply_to_project_lessons_and_are_proposed_above() {
        let update = |id: &str| {
            json!({
                "action": "update", "kind": "memory", "id": id,
                "title": "New", "content": "Nouvelle version.",
                "before": entry(id, "memory", "Old", "Ancienne version."),
                "after": entry(id, "memory", "New", "Nouvelle version."),
                "applied": true,
            })
        };
        let import = refinement_ops(
            &outcome(vec![
                update("yl_project"),
                update("yl_type"),
                update("yl_global"),
                update("yl_gone"),
                update("prime_entry"),
            ]),
            levels,
        )
        .unwrap();
        let change = |id: &str, level| LessonOp::Propose {
            lesson_id: Some(id.to_string()),
            kind: ProposalKind::Change,
            target_level: Some(level),
            payload: json!({
                "title": "New",
                "content": "Nouvelle version.",
                "refinementSummary": "lessons from the session",
            }),
        };
        let create = |entry_id: &str| LessonOp::Create {
            entry_id: entry_id.to_string(),
            kind: LessonKind::Memory,
            title: "New".to_string(),
            content: "Nouvelle version.".to_string(),
        };
        assert_eq!(
            import.ops,
            vec![
                LessonOp::Update {
                    lesson_id: "yl_project".to_string(),
                    title: "New".to_string(),
                    content: "Nouvelle version.".to_string(),
                },
                change("yl_type", LessonLevel::Type),
                change("yl_global", LessonLevel::Global),
                // Leçon archivée ou supprimée entre-temps, entrée de Prime :
                // créations (le magasin écarte un doublon).
                create("yl_gone"),
                create("prime_entry"),
            ]
        );
    }

    #[test]
    fn deletions_archive_project_lessons_and_are_proposed_above() {
        let delete = |id: &str| {
            json!({
                "action": "delete", "kind": "prompt", "id": id,
                "before": entry(id, "prompt", "Old", "Ancienne consigne."),
                "applied": true,
            })
        };
        let import = refinement_ops(
            &outcome(vec![
                delete("yl_project"),
                delete("yl_type"),
                delete("yl_global"),
                delete("yl_gone"),
                delete("prime_entry"),
            ]),
            levels,
        )
        .unwrap();
        let archive = |id: &str, level| LessonOp::Propose {
            lesson_id: Some(id.to_string()),
            kind: ProposalKind::Archive,
            target_level: Some(level),
            payload: json!({ "refinementSummary": "lessons from the session" }),
        };
        assert_eq!(
            import.ops,
            vec![
                LessonOp::Archive {
                    lesson_id: "yl_project".to_string()
                },
                archive("yl_type", LessonLevel::Type),
                archive("yl_global", LessonLevel::Global),
            ]
        );
        assert_eq!(
            import.skipped,
            vec![
                "delete prompt:yl_gone: not one of our active lessons".to_string(),
                "delete prompt:prime_entry: not one of our active lessons".to_string(),
            ]
        );
    }

    #[test]
    fn unapplied_unknown_and_empty_edits_are_skipped() {
        let import = refinement_ops(
            &outcome(vec![
                json!({ "action": "create", "kind": "memory", "id": "a", "content": "x", "applied": false, "error": "stale" }),
                json!({ "action": "create", "kind": "plan", "id": "b", "content": "x", "applied": true }),
                json!({ "action": "rename", "kind": "memory", "id": "c", "content": "x", "applied": true }),
                json!({ "action": "create", "kind": "memory", "id": "d", "content": "  ", "applied": true }),
            ]),
            levels,
        )
        .unwrap();
        assert!(import.ops.is_empty());
        assert_eq!(
            import.skipped,
            vec![
                "create memory:a: not applied (stale)".to_string(),
                "create plan:b: unknown kind".to_string(),
                "rename memory:c: unknown action".to_string(),
                "create memory:d: empty entry".to_string(),
            ]
        );
    }

    #[test]
    fn planned_fields_fill_in_without_an_after_entry() {
        let import = refinement_ops(
            &outcome(vec![json!({
                "action": "create", "kind": "memory", "id": "e",
                "content": "Un contenu sans titre qui sert de titre court ici et là.",
                "applied": true,
            })]),
            levels,
        )
        .unwrap();
        assert_eq!(
            import.ops,
            vec![LessonOp::Create {
                entry_id: "e".to_string(),
                kind: LessonKind::Memory,
                title: "Un contenu sans titre qui sert de titre".to_string(),
                content: "Un contenu sans titre qui sert de titre court ici et là.".to_string(),
            }]
        );
    }

    #[test]
    fn rollbacks_are_ordinary_edits() {
        let mut details = outcome(vec![json!({
            "action": "update", "kind": "memory", "id": "yl_project",
            "content": "Ancienne version.",
            "after": entry("yl_project", "memory", "Old", "Ancienne version."),
            "applied": true,
        })]);
        details["rollbackOf"] = json!("refine_0");
        let import = refinement_ops(&details, levels).unwrap();
        assert_eq!(import.rollback_of.as_deref(), Some("refine_0"));
        assert!(matches!(import.ops[0], LessonOp::Update { .. }));
    }

    #[test]
    fn an_outcome_needs_its_refinement_id() {
        assert!(refinement_ops(&json!({ "edits": [] }), levels).is_err());
    }

    /// Un magasin et un dossier agent jetables, avec un harness local qui
    /// porte les entrées données (`kind`, `id`, `content`).
    fn import_fixture(entries: &[(&str, &str, &str)]) -> (AppStore, PathBuf) {
        static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let root = std::env::temp_dir().join(format!(
            "yusai-lessons-import-test-{}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |elapsed| elapsed.as_nanos())
        ));
        let agent_dir = root.join("agent");
        let harness = refine_harness_file(&agent_dir, false);
        std::fs::create_dir_all(harness.parent().unwrap()).unwrap();
        let mut state = json!({ "schema": 1, "entries": {}, "refinements": [] });
        for (kind, id, content) in entries {
            state["entries"][*kind][*id] = entry(id, kind, "Title", content);
        }
        std::fs::write(&harness, state.to_string()).unwrap();
        let store = AppStore::open_at(root.join("state.sqlite3")).unwrap();
        (store, agent_dir)
    }

    fn harness_ids(agent_dir: &Path, kind: &str) -> Vec<String> {
        let text = std::fs::read_to_string(refine_harness_file(agent_dir, false)).unwrap();
        let state: Value = serde_json::from_str(&text).unwrap();
        let mut ids: Vec<String> = state["entries"][kind]
            .as_object()
            .map(|entries| entries.keys().cloned().collect())
            .unwrap_or_default();
        ids.sort();
        ids
    }

    #[test]
    fn a_failed_creation_keeps_its_entry_and_is_recorded_with_the_refine() {
        let (store, agent_dir) = import_fixture(&[("memory", "tests", "Lancer cargo test.")]);
        // Sans projet, la création échoue dans le magasin.
        let thread = ThreadContext {
            conversation_id: "conv-1".to_string(),
            workspace_id: String::new(),
        };
        let report = import_refinement_outcome(
            &store,
            &agent_dir,
            &thread,
            &outcome(vec![create_edit("memory", "tests", "Lancer cargo test.")]),
        )
        .unwrap()
        .unwrap();
        assert!(report.created.is_empty());
        assert_eq!(report.failed.len(), 1, "report: {report:?}");
        assert!(report.failed[0].starts_with("create tests:"));
        assert_eq!(report.removed_entries, 0);
        assert_eq!(harness_ids(&agent_dir, "memory"), vec!["tests"]);
        let imported = store.imported_refinement("refine_1").unwrap().unwrap();
        assert_eq!(imported.failures, report.failed);
        assert_eq!(imported.conversation_id.as_deref(), Some("conv-1"));
        let _ = std::fs::remove_dir_all(agent_dir.parent().unwrap());
    }

    #[test]
    fn only_the_entries_of_successful_creations_leave_the_harness() {
        let (store, agent_dir) = import_fixture(&[
            ("memory", "tests", "Lancer cargo test."),
            ("memory", "other", "Une entrée d'une autre refine."),
        ]);
        let thread = ThreadContext {
            conversation_id: "conv-1".to_string(),
            workspace_id: "/work/a".to_string(),
        };
        // Deux leçons projet existantes : la mise à jour de la seconde vers
        // le texte de la première échoue (même texte, même niveau).
        let insert = |content: &str| match store
            .insert_lesson(
                &NewLesson {
                    level: LessonLevel::Project,
                    workspace_id: Some("/work/a".to_string()),
                    project_type: None,
                    kind: LessonKind::Memory,
                    title: "Old".to_string(),
                    content: content.to_string(),
                },
                &LessonOrigin::user(),
            )
            .unwrap()
        {
            InsertLessonOutcome::Created(lesson) => lesson.id,
            other => panic!("{other:?}"),
        };
        let first = insert("Première.");
        let second = insert("Seconde.");
        let collide = json!({
            "action": "update", "kind": "memory", "id": second,
            "content": "Première.",
            "after": entry(&second, "memory", "Old", "Première."),
            "applied": true,
        });
        let report = import_refinement_outcome(
            &store,
            &agent_dir,
            &thread,
            &outcome(vec![
                collide,
                create_edit("memory", "tests", "Lancer cargo test."),
            ]),
        )
        .unwrap()
        .unwrap();
        assert_eq!(report.created.len(), 1, "report: {report:?}");
        assert_eq!(report.failed.len(), 1, "report: {report:?}");
        assert!(report.failed[0].starts_with(&format!("update {second}:")));
        assert_eq!(report.removed_entries, 1);
        assert_eq!(harness_ids(&agent_dir, "memory"), vec!["other"]);
        assert_eq!(store.lesson(&second).unwrap().unwrap().content, "Seconde.");
        assert_eq!(store.lesson(&first).unwrap().unwrap().content, "Première.");
        assert_eq!(
            store
                .imported_refinement("refine_1")
                .unwrap()
                .unwrap()
                .failures,
            report.failed
        );
        let _ = std::fs::remove_dir_all(agent_dir.parent().unwrap());
    }

    #[test]
    fn global_harness_writes_become_project_lessons_proposed_for_global() {
        let (store, agent_dir) = import_fixture(&[]);
        let thread = ThreadContext {
            conversation_id: "conv-1".to_string(),
            workspace_id: "/work/a".to_string(),
        };
        // Déjà là au niveau projet : doublon, mais proposé quand même.
        let InsertLessonOutcome::Created(existing) = store
            .insert_lesson(
                &NewLesson {
                    level: LessonLevel::Project,
                    workspace_id: Some("/work/a".to_string()),
                    project_type: None,
                    kind: LessonKind::Memory,
                    title: "Tests".to_string(),
                    content: "Lancer cargo test.".to_string(),
                },
                &LessonOrigin::user(),
            )
            .unwrap()
        else {
            panic!("existing lesson");
        };
        let file = global_harness_file(&agent_dir);
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        let mut skill = entry("fmt", "skill", "Format", "Formater le code.");
        skill["reference"] = json!({ "type": "python", "import": "fmt", "callable": "run" });
        std::fs::write(
            &file,
            json!({
                "schema": 1,
                "entries": {
                    "memory": {
                        "langue": entry("langue", "memory", "Langue", "Répondre en français."),
                        "tests": entry("tests", "memory", "Tests", "Lancer cargo test."),
                    },
                    "skill": { "fmt": skill },
                    "subagent": { "vide": entry("vide", "subagent", "Vide", "  ") },
                },
                "refinements": [],
            })
            .to_string(),
        )
        .unwrap();

        let report = import_global_harness_writes(&store, &agent_dir, &thread)
            .unwrap()
            .expect("entries imported");
        assert_eq!(report.created.len(), 1, "report: {report:?}");
        assert_eq!(report.duplicates, vec![existing.id.clone()]);
        assert!(report.failed.is_empty(), "report: {report:?}");
        assert_eq!(report.removed_entries, 3, "report: {report:?}");
        let created = store.lesson(&report.created[0]).unwrap().unwrap();
        assert_eq!(created.level, LessonLevel::Project);
        assert_eq!(created.workspace_id.as_deref(), Some("/work/a"));
        assert_eq!(created.content, "Répondre en français.");
        assert_eq!(
            store.lesson_events(&created.id).unwrap()[0].actor,
            "harness:global"
        );

        let proposals = store.pending_lesson_proposals().unwrap();
        let promoted: Vec<&str> = proposals
            .iter()
            .filter(|proposal| proposal.kind == ProposalKind::Promote)
            .filter(|proposal| proposal.target_level == Some(LessonLevel::Global))
            .filter_map(|proposal| proposal.lesson_id.as_deref())
            .collect();
        assert_eq!(promoted.len(), 2);
        assert!(promoted.contains(&created.id.as_str()));
        assert!(promoted.contains(&existing.id.as_str()));
        assert_eq!(
            proposals
                .iter()
                .filter(|proposal| proposal.kind == ProposalKind::Skill)
                .count(),
            1
        );

        // Ne reste que l'entrée vide, jamais importée ; un second passage
        // ne fait rien et ne laisse pas de trace.
        let state: Value = serde_json::from_str(&std::fs::read_to_string(&file).unwrap()).unwrap();
        assert!(state["entries"]["memory"].as_object().unwrap().is_empty());
        assert!(state["entries"]["skill"].as_object().unwrap().is_empty());
        assert!(state["entries"]["subagent"]["vide"].is_object());
        assert_eq!(
            import_global_harness_writes(&store, &agent_dir, &thread).unwrap(),
            None
        );
        assert!(store
            .imported_refinement(&report.refinement_id)
            .unwrap()
            .is_some());
        // Une écriture à nouveau : pas de seconde proposition identique.
        let mut state = state;
        state["entries"]["memory"]["langue"] =
            entry("langue", "memory", "Langue", "Répondre en français.");
        std::fs::write(&file, state.to_string()).unwrap();
        let again = import_global_harness_writes(&store, &agent_dir, &thread)
            .unwrap()
            .unwrap();
        assert_eq!(again.duplicates, vec![created.id.clone()]);
        assert!(again.proposals.is_empty(), "report: {again:?}");
        let _ = std::fs::remove_dir_all(agent_dir.parent().unwrap());
    }

    #[test]
    fn no_global_harness_file_means_nothing_to_guard() {
        let (store, agent_dir) = import_fixture(&[]);
        let thread = ThreadContext {
            conversation_id: "conv-1".to_string(),
            workspace_id: "/work/a".to_string(),
        };
        assert_eq!(
            import_global_harness_writes(&store, &agent_dir, &thread).unwrap(),
            None
        );
        let _ = std::fs::remove_dir_all(agent_dir.parent().unwrap());
    }

    fn lesson(store: &AppStore, level: LessonLevel, workspace: &str, content: &str) -> Lesson {
        let new = NewLesson {
            level,
            workspace_id: Some(workspace.to_string()),
            project_type: (level == LessonLevel::Type).then(|| "rust".to_string()),
            kind: LessonKind::Memory,
            title: content.to_string(),
            content: content.to_string(),
        };
        match store.insert_lesson(&new, &LessonOrigin::user()).unwrap() {
            InsertLessonOutcome::Created(lesson) => lesson,
            other => panic!("not created: {other:?}"),
        }
    }

    #[test]
    fn seeding_shows_prime_the_applicable_lessons_and_unseeding_leaves_the_rest() {
        let (store, agent_dir) = import_fixture(&[
            ("memory", "theirs", "Une entrée de Prime."),
            ("memory", "yl_stale", "Reste d'une refine interrompue."),
        ]);
        store
            .set_project_type(
                "/work/a",
                Some("rust"),
                sinew_app::store::ProjectTypeSource::User,
            )
            .unwrap();
        let project = lesson(
            &store,
            LessonLevel::Project,
            "/work/a",
            "Lancer cargo test.",
        );
        let typed = lesson(&store, LessonLevel::Type, "/work/a", "Préférer anyhow.");
        let global = lesson(
            &store,
            LessonLevel::Global,
            "/work/a",
            "Répondre en français.",
        );
        let elsewhere = lesson(&store, LessonLevel::Project, "/work/b", "Autre projet.");
        let thread = ThreadContext {
            conversation_id: "conv-1".to_string(),
            workspace_id: "/work/a".to_string(),
        };

        assert_eq!(seed_thread_lessons(&store, &agent_dir, &thread).unwrap(), 3);
        let mut expected = vec![
            project.id.clone(),
            typed.id.clone(),
            global.id.clone(),
            "theirs".to_string(),
        ];
        expected.sort();
        assert_eq!(harness_ids(&agent_dir, "memory"), expected);
        assert!(!harness_ids(&agent_dir, "memory").contains(&elsewhere.id));
        // Prime relit nos entrées telles quelles, niveau compris dans le
        // `path`.
        let file = refine_harness_file(&agent_dir, false);
        let state =
            pa_core::refinement::load_harness_state(file.parent().unwrap(), HarnessScope::Local);
        let memories = &state.entries[&RefinementKind::Memory];
        assert_eq!(memories.len(), 4);
        assert_eq!(memories[&project.id].content, "Lancer cargo test.");
        assert_eq!(memories[&project.id].path, "yusai/project");
        assert_eq!(memories[&typed.id].path, "yusai/type/rust");
        assert_eq!(memories[&global.id].path, "yusai/global");
        assert_eq!(memories[&global.id].source, "yusai");

        assert_eq!(unseed_thread_lessons(&agent_dir).unwrap(), 3);
        assert_eq!(harness_ids(&agent_dir, "memory"), vec!["theirs"]);
        let text = std::fs::read_to_string(&file).unwrap();
        assert!(serde_json::from_str::<Value>(&text).unwrap()["refinements"].is_array());
        assert_eq!(unseed_thread_lessons(&agent_dir).unwrap(), 0);
        let _ = std::fs::remove_dir_all(agent_dir.parent().unwrap());
    }

    #[test]
    fn seeding_creates_the_harness_only_when_there_is_something_to_seed() {
        let (store, agent_dir) = import_fixture(&[]);
        let file = refine_harness_file(&agent_dir, false);
        std::fs::remove_file(&file).unwrap();
        let thread = ThreadContext {
            conversation_id: "conv-1".to_string(),
            workspace_id: "/work/a".to_string(),
        };
        assert_eq!(seed_thread_lessons(&store, &agent_dir, &thread).unwrap(), 0);
        assert!(!file.exists());
        let project = lesson(
            &store,
            LessonLevel::Project,
            "/work/a",
            "Lancer cargo test.",
        );
        assert_eq!(seed_thread_lessons(&store, &agent_dir, &thread).unwrap(), 1);
        assert_eq!(harness_ids(&agent_dir, "memory"), vec![project.id]);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&file).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        let _ = std::fs::remove_dir_all(agent_dir.parent().unwrap());
    }

    #[test]
    fn an_unreadable_harness_is_left_alone() {
        let (store, agent_dir) = import_fixture(&[]);
        let file = refine_harness_file(&agent_dir, false);
        std::fs::write(&file, "pas du json").unwrap();
        lesson(
            &store,
            LessonLevel::Project,
            "/work/a",
            "Lancer cargo test.",
        );
        let thread = ThreadContext {
            conversation_id: "conv-1".to_string(),
            workspace_id: "/work/a".to_string(),
        };
        assert!(seed_thread_lessons(&store, &agent_dir, &thread).is_err());
        assert!(unseed_thread_lessons(&agent_dir).is_err());
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "pas du json");
        let _ = std::fs::remove_dir_all(agent_dir.parent().unwrap());
    }
}
