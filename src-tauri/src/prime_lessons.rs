//! Conversion d'une refine de Prime en opérations sur nos leçons.
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
//! Fonction pure : le niveau de nos leçons vient de l'appelant.

use anyhow::{anyhow, Result};
use serde_json::{json, Value};
use sinew_app::store::{LessonKind, LessonLevel, ProposalKind};

/// Préfixe des ids de nos leçons, tels qu'amorcés dans le harness de Prime.
pub const LESSON_ID_PREFIX: &str = "yl_";

/// Une opération sur le magasin des leçons.
#[derive(Debug, Clone, PartialEq)]
pub enum LessonOp {
    /// Nouvelle leçon au niveau projet.
    Create {
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
                    kind: LessonKind::Memory,
                    title: "Stored title".to_string(),
                    content: "Toujours lancer cargo test.".to_string(),
                },
                LessonOp::Create {
                    kind: LessonKind::Prompt,
                    title: "Stored title".to_string(),
                    content: "Répondre brièvement.".to_string(),
                },
                LessonOp::Create {
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
        let create = LessonOp::Create {
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
                create.clone(),
                create,
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
