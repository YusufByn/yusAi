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
use serde::Serialize;
use serde_json::{json, Value};
use sinew_app::store::{
    AppStore, InsertLessonOutcome, LessonKind, LessonLevel, LessonOrigin, LessonStatus, NewLesson,
    NewProposal, ProposalKind,
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
        return Err(format!("{label}: not applied"));
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
    pub skipped: Vec<String>,
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

/// Les imports passent un par un : magasin et fichier du harness partagé.
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
/// relirait). Les erreurs de ce retrait ne font pas échouer l'import.
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
    let mut touched: Vec<(&'static str, String)> = Vec::new();
    let mut to_promote: Vec<String> = Vec::new();
    for op in import.ops {
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
                match store.insert_lesson(&lesson, &origin) {
                    Ok(InsertLessonOutcome::Created(created)) => {
                        touched.push((harness_kind(kind), entry_id));
                        to_promote.push(created.id.clone());
                        report.created.push(created.id);
                    }
                    Ok(InsertLessonOutcome::Duplicate { existing_id }) => {
                        touched.push((harness_kind(kind), entry_id));
                        to_promote.push(existing_id.clone());
                        report.duplicates.push(existing_id);
                    }
                    Err(error) => report.skipped.push(format!("create {entry_id}: {error:#}")),
                }
            }
            LessonOp::Update {
                lesson_id,
                title,
                content,
            } => match store.update_lesson(&lesson_id, &title, &content, &origin) {
                Ok(_) => report.updated.push(lesson_id),
                Err(error) => report
                    .skipped
                    .push(format!("update {lesson_id}: {error:#}")),
            },
            LessonOp::Archive { lesson_id } => match store.archive_lesson(&lesson_id, &origin) {
                Ok(_) => report.archived.push(lesson_id),
                Err(error) => report
                    .skipped
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
                Err(error) => report.skipped.push(format!("proposal: {error:#}")),
            },
        }
    }
    if global {
        for lesson_id in to_promote {
            match propose_global(store, &lesson_id, &origin) {
                Ok(Some(proposal)) => report.proposals.push(proposal),
                Ok(None) => {}
                Err(error) => report
                    .skipped
                    .push(format!("promote {lesson_id}: {error:#}")),
            }
        }
    }
    store.mark_refinement_imported(&report.refinement_id, Some(&thread.conversation_id))?;

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
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(error) => {
            return Err(error).with_context(|| format!("read {}", path.display()));
        }
    };
    let mut state: Value =
        serde_json::from_str(&text).with_context(|| format!("parse {}", path.display()))?;
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
    if removed == 0 {
        return Ok(0);
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
    Ok(removed)
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
                "create memory:a: not applied".to_string(),
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
}
