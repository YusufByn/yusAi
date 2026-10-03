//! Ce que la vue « Lessons » du chat Prime montre d'un projet : les
//! propositions en attente (de tous les projets), les leçons du projet
//! (le sien, son type confirmé, le global) et les refines importées pour
//! lui, refine par refine. Et ce qu'on peut y faire : chaque action de
//! l'utilisateur est notée dans l'historique avec l'auteur `user` ; changer
//! le niveau d'une leçon soi-même vaut validation.

use std::collections::HashMap;
use std::path::Path;

use anyhow::{anyhow, bail, Context, Result};
use serde::Serialize;
use sinew_app::store::{
    AppStore, ImportedRefinement, Lesson, LessonEvent, LessonLevel, LessonOrigin, LessonProposal,
    LessonScope, LessonStatus, ProposalKind, ProposalStatus,
};

/// La vue d'un projet.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LessonsOverview {
    pub workspace_id: String,
    /// Le type confirmé du projet (seul il compte pour les leçons).
    pub project_type: Option<String>,
    pub proposals: Vec<ProposalView>,
    pub lessons: Vec<LessonView>,
    pub refines: Vec<RefineView>,
}

/// Une proposition en attente, avec sa leçon et son projet.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProposalView {
    #[serde(flatten)]
    pub proposal: LessonProposal,
    /// La leçon visée, telle qu'elle est maintenant.
    pub lesson: Option<Lesson>,
    /// Le projet d'origine (la proposition, sinon sa leçon).
    pub project: Option<String>,
    /// Le type confirmé de ce projet : une montée vers « type » le demande.
    pub project_type: Option<String>,
    pub conversation_title: Option<String>,
}

/// Une leçon du projet.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LessonView {
    #[serde(flatten)]
    pub lesson: Lesson,
    /// Elle tient dans le budget d'injection (4 000 caractères).
    pub injected: bool,
}

/// Une refine importée et ce qu'elle a fait.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RefineView {
    #[serde(flatten)]
    pub refinement: ImportedRefinement,
    pub conversation_title: Option<String>,
    pub created: usize,
    pub updated: usize,
    pub archived: usize,
    pub duplicates: usize,
    pub proposals: usize,
}

/// Le détail d'une refine : ses événements, avec la leçon de chacun telle
/// qu'elle est maintenant, et les propositions qu'elle a ouvertes.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RefineDetail {
    pub events: Vec<EventView>,
    pub proposals: Vec<LessonProposal>,
}

/// Un événement de l'historique des leçons.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EventView {
    #[serde(flatten)]
    pub event: LessonEvent,
    pub lesson: Option<Lesson>,
    pub conversation_title: Option<String>,
}

/// La vue du projet `workspace_id`.
pub fn lessons_overview(
    store: &AppStore,
    data_dir: &Path,
    workspace_id: &str,
) -> Result<LessonsOverview> {
    let mut titles = Titles::new(store);
    let project_type = store.confirmed_project_type(workspace_id)?;
    let injected = crate::prime_guidance::thread_guidance(store, data_dir, workspace_id)?.injected;
    let lessons = store
        .project_lessons(
            &LessonScope {
                workspace_id: workspace_id.to_string(),
                project_type: project_type.clone(),
            },
            true,
        )?
        .into_iter()
        .map(|lesson| LessonView {
            injected: injected.contains(&lesson.id),
            lesson,
        })
        .collect();

    let mut proposals = Vec::new();
    for proposal in store.pending_lesson_proposals()? {
        let lesson = match &proposal.lesson_id {
            Some(id) => store.lesson(id)?,
            None => None,
        };
        let project = proposal.workspace_id.clone().or_else(|| {
            lesson
                .as_ref()
                .and_then(|lesson| lesson.workspace_id.clone())
        });
        let project_type = match &project {
            Some(project) => store.confirmed_project_type(project)?,
            None => None,
        };
        proposals.push(ProposalView {
            conversation_title: titles.of(proposal.conversation_id.as_deref())?,
            proposal,
            lesson,
            project,
            project_type,
        });
    }

    let mut refines = Vec::new();
    for refinement in store.project_refinements(workspace_id)? {
        let events = store.refinement_events(&refinement.refinement_id)?;
        let count = |action: &str| events.iter().filter(|event| event.action == action).count();
        refines.push(RefineView {
            conversation_title: titles.of(refinement.conversation_id.as_deref())?,
            created: count("created"),
            updated: count("updated"),
            archived: count("archived"),
            duplicates: count("duplicate_skipped"),
            proposals: store.refinement_proposals(&refinement.refinement_id)?.len(),
            refinement,
        });
    }

    Ok(LessonsOverview {
        workspace_id: workspace_id.to_string(),
        project_type,
        proposals,
        lessons,
        refines,
    })
}

/// Le détail d'une refine.
pub fn refine_detail(store: &AppStore, refinement_id: &str) -> Result<RefineDetail> {
    Ok(RefineDetail {
        events: event_views(store, store.refinement_events(refinement_id)?)?,
        proposals: store.refinement_proposals(refinement_id)?,
    })
}

/// L'historique d'une leçon, du plus ancien au plus récent.
pub fn lesson_history(store: &AppStore, lesson_id: &str) -> Result<Vec<EventView>> {
    event_views(store, store.lesson_events(lesson_id)?)
}

fn event_views(store: &AppStore, events: Vec<LessonEvent>) -> Result<Vec<EventView>> {
    let mut titles = Titles::new(store);
    let mut lessons: HashMap<String, Option<Lesson>> = HashMap::new();
    let mut views = Vec::new();
    for event in events {
        let lesson = match lessons.get(&event.lesson_id) {
            Some(lesson) => lesson.clone(),
            None => {
                let lesson = store.lesson(&event.lesson_id)?;
                lessons.insert(event.lesson_id.clone(), lesson.clone());
                lesson
            }
        };
        views.push(EventView {
            conversation_title: titles.of(event.conversation_id.as_deref())?,
            lesson,
            event,
        });
    }
    Ok(views)
}

/// Accepte une proposition en attente et applique son effet. Pour une
/// montée, `target_level` remplace le niveau proposé ; le niveau `type`
/// demande le type confirmé du projet de la leçon. Les propositions de
/// skill attendent les dossiers de skills.
pub fn accept_proposal(
    store: &AppStore,
    proposal_id: &str,
    target_level: Option<LessonLevel>,
) -> Result<()> {
    let proposal = store
        .lesson_proposal(proposal_id)?
        .filter(|proposal| proposal.status == ProposalStatus::Pending)
        .ok_or_else(|| anyhow!("no pending proposal {proposal_id}"))?;
    let user = LessonOrigin::user();
    if proposal.kind == ProposalKind::Skill {
        bail!("skill proposals wait for the skills folders");
    }
    let lesson_id = proposal
        .lesson_id
        .as_deref()
        .ok_or_else(|| anyhow!("proposal {proposal_id} has no lesson"))?;
    let lesson = store
        .lesson(lesson_id)?
        .ok_or_else(|| anyhow!("the lesson of proposal {proposal_id} no longer exists"))?;
    // L'effet, puis la décision, puis les autres propositions de la leçon
    // que cet effet rend caduques.
    let stale: &[ProposalKind] = match proposal.kind {
        ProposalKind::Promote => {
            let level = target_level
                .or(proposal.target_level)
                .ok_or_else(|| anyhow!("proposal {proposal_id} has no target level"))?;
            move_lesson(store, &lesson, level, &user)?;
            &[ProposalKind::Promote]
        }
        ProposalKind::Change => {
            let field = |name: &str| {
                proposal
                    .payload
                    .get(name)
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_string)
            };
            let content = field("content")
                .ok_or_else(|| anyhow!("proposal {proposal_id} has no new text"))?;
            let title = field("title").unwrap_or_else(|| lesson.title.clone());
            store.update_lesson(&lesson.id, &title, &content, &user)?;
            &[]
        }
        ProposalKind::Archive => {
            store.archive_lesson(&lesson.id, &user)?;
            ALL_LESSON_PROPOSALS
        }
        ProposalKind::Skill => unreachable!("handled above"),
    };
    store.decide_lesson_proposal(proposal_id, ProposalStatus::Accepted)?;
    store.close_pending_proposals(&lesson.id, stale)?;
    Ok(())
}

const ALL_LESSON_PROPOSALS: &[ProposalKind] = &[
    ProposalKind::Promote,
    ProposalKind::Change,
    ProposalKind::Archive,
];

/// Refuse une proposition ; noté dans l'historique de sa leçon.
pub fn reject_proposal(store: &AppStore, proposal_id: &str) -> Result<()> {
    store.reject_lesson_proposal(proposal_id, &LessonOrigin::user())
}

/// Change le niveau d'une leçon (validation de l'utilisateur). Le niveau
/// `type` prend le type confirmé du projet de la leçon ; le niveau
/// `project` la garde à son projet d'origine. Ses propositions de montée
/// en attente deviennent caduques.
pub fn set_lesson_level(store: &AppStore, lesson_id: &str, level: LessonLevel) -> Result<Lesson> {
    let lesson = store
        .lesson(lesson_id)?
        .ok_or_else(|| anyhow!("unknown lesson {lesson_id}"))?;
    let moved = move_lesson(store, &lesson, level, &LessonOrigin::user())?;
    store.close_pending_proposals(lesson_id, &[ProposalKind::Promote])?;
    Ok(moved)
}

fn move_lesson(
    store: &AppStore,
    lesson: &Lesson,
    level: LessonLevel,
    origin: &LessonOrigin,
) -> Result<Lesson> {
    let project_type = match level {
        LessonLevel::Type => {
            let workspace = lesson
                .workspace_id
                .as_deref()
                .ok_or_else(|| anyhow!("lesson {} has no project", lesson.id))?;
            Some(
                store
                    .confirmed_project_type(workspace)?
                    .ok_or_else(|| anyhow!("choose the project's type first"))?,
            )
        }
        _ => None,
    };
    store.set_lesson_level(&lesson.id, level, project_type.as_deref(), origin)
}

/// Archive une leçon ; ses propositions en attente deviennent caduques.
pub fn archive_lesson(store: &AppStore, lesson_id: &str) -> Result<Lesson> {
    let archived = store.archive_lesson(lesson_id, &LessonOrigin::user())?;
    store.close_pending_proposals(lesson_id, ALL_LESSON_PROPOSALS)?;
    Ok(archived)
}

/// Ce que « Undo » a fait d'une refine.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UndoReport {
    /// Leçons créées par la refine, archivées.
    pub archived: Vec<String>,
    /// Leçons modifiées par la refine, remises à leur ancien texte.
    pub reverted: Vec<String>,
    /// Leçons archivées par la refine, restaurées.
    pub restored: Vec<String>,
    /// Propositions de la refine encore en attente, refusées.
    pub rejected_proposals: usize,
    /// Ce qui n'a pas pu être défait, avec la raison.
    pub skipped: Vec<String>,
}

/// Défait une refine : archive les leçons qu'elle a créées, remet l'ancien
/// texte des leçons qu'elle a modifiées (si personne ne les a retouchées
/// depuis), restaure celles qu'elle a archivées et refuse ses propositions
/// encore en attente. Tout est noté avec l'auteur `user`.
pub fn undo_refine(store: &AppStore, refinement_id: &str) -> Result<UndoReport> {
    let refinement = store
        .imported_refinement(refinement_id)?
        .ok_or_else(|| anyhow!("unknown refine {refinement_id}"))?;
    if refinement.undone_at_ms.is_some() {
        bail!("refine {refinement_id} is already undone");
    }
    let user = LessonOrigin::user();
    let mut report = UndoReport::default();
    // Du plus récent au plus ancien, comme on remonte le temps.
    for event in store.refinement_events(refinement_id)?.into_iter().rev() {
        let Some(lesson) = store.lesson(&event.lesson_id)? else {
            continue;
        };
        let snapshot = |value: &Option<serde_json::Value>| -> Result<Option<Lesson>> {
            value
                .clone()
                .map(serde_json::from_value)
                .transpose()
                .context("unable to read a lesson snapshot")
        };
        match event.action.as_str() {
            "created" if lesson.status == LessonStatus::Active => {
                match archive_lesson(store, &lesson.id) {
                    Ok(_) => report.archived.push(lesson.id),
                    Err(error) => report.skipped.push(format!("{}: {error:#}", lesson.id)),
                }
            }
            "created" => report
                .skipped
                .push(format!("{}: already archived", lesson.id)),
            "updated" => {
                let (Some(before), Some(after)) =
                    (snapshot(&event.before)?, snapshot(&event.after)?)
                else {
                    continue;
                };
                if lesson.title != after.title || lesson.content != after.content {
                    report
                        .skipped
                        .push(format!("{}: changed since the refine", lesson.id));
                    continue;
                }
                match store.update_lesson(&lesson.id, &before.title, &before.content, &user) {
                    Ok(_) => report.reverted.push(lesson.id),
                    Err(error) => report.skipped.push(format!("{}: {error:#}", lesson.id)),
                }
            }
            "archived" if lesson.status == LessonStatus::Archived => {
                match store.restore_lesson(&lesson.id, &user) {
                    Ok(_) => report.restored.push(lesson.id),
                    Err(error) => report.skipped.push(format!("{}: {error:#}", lesson.id)),
                }
            }
            _ => {}
        }
    }
    for proposal in store.refinement_proposals(refinement_id)? {
        if proposal.status == ProposalStatus::Pending {
            store.reject_lesson_proposal(&proposal.id, &user)?;
            report.rejected_proposals += 1;
        }
    }
    store.mark_refinement_undone(refinement_id)?;
    Ok(report)
}

/// Les titres des conversations, lus une fois chacun.
struct Titles<'a> {
    store: &'a AppStore,
    known: HashMap<String, Option<String>>,
}

impl<'a> Titles<'a> {
    fn new(store: &'a AppStore) -> Self {
        Self {
            store,
            known: HashMap::new(),
        }
    }

    fn of(&mut self, conversation_id: Option<&str>) -> Result<Option<String>> {
        let Some(id) = conversation_id else {
            return Ok(None);
        };
        if let Some(title) = self.known.get(id) {
            return Ok(title.clone());
        }
        let title = self.store.conversation_title(id)?;
        self.known.insert(id.to_string(), title.clone());
        Ok(title)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::prime_lessons::{import_refinement_outcome_as, ThreadContext};
    use serde_json::json;
    use sinew_app::store::{InsertLessonOutcome, LessonKind, LessonLevel, LessonOrigin, NewLesson};

    #[test]
    fn the_overview_shows_proposals_lessons_and_refines_of_the_project() {
        let root = std::env::temp_dir().join(format!(
            "yusai-review-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |elapsed| elapsed.as_nanos())
        ));
        let agent_dir = root.join("agent");
        std::fs::create_dir_all(&agent_dir).unwrap();
        let store = AppStore::open_at(root.join("state.sqlite3")).unwrap();
        let conversation = store
            .create_conversation(
                "/work/a",
                &sinew_core::ModelRef::new("test", "model"),
                "system",
            )
            .unwrap();
        let thread = ThreadContext {
            conversation_id: conversation.id.clone(),
            workspace_id: "/work/a".to_string(),
        };
        // Une leçon d'un autre projet : hors de la vue de /work/a.
        let elsewhere = NewLesson {
            level: LessonLevel::Project,
            workspace_id: Some("/work/b".to_string()),
            project_type: None,
            kind: LessonKind::Memory,
            title: "B".to_string(),
            content: "Leçon de B.".to_string(),
        };
        assert!(matches!(
            store
                .insert_lesson(&elsewhere, &LessonOrigin::user())
                .unwrap(),
            InsertLessonOutcome::Created(_)
        ));
        // Une refine globale de /work/a : une leçon et une proposition.
        let details = json!({
            "refinementId": "refine_1",
            "summary": "Tests du projet",
            "scope": "global",
            "edits": [{
                "action": "create", "kind": "memory", "id": "tests",
                "title": "Tests", "content": "Lancer cargo test.", "applied": true,
            }],
        });
        let report =
            import_refinement_outcome_as(&store, &agent_dir, &thread, &details, "refine:retain")
                .unwrap()
                .unwrap();

        let overview = lessons_overview(&store, &root, "/work/a").unwrap();
        assert_eq!(overview.lessons.len(), 1);
        assert_eq!(overview.lessons[0].lesson.id, report.created[0]);
        assert!(overview.lessons[0].injected);
        assert_eq!(overview.proposals.len(), 1);
        let proposal = &overview.proposals[0];
        assert_eq!(proposal.project.as_deref(), Some("/work/a"));
        assert_eq!(
            proposal.conversation_title.as_deref(),
            Some(conversation.title.as_str())
        );
        assert_eq!(
            proposal
                .lesson
                .as_ref()
                .map(|lesson| lesson.content.as_str()),
            Some("Lancer cargo test.")
        );
        assert_eq!(overview.refines.len(), 1);
        let refine = &overview.refines[0];
        assert_eq!(
            refine.refinement.summary.as_deref(),
            Some("Tests du projet")
        );
        assert_eq!(refine.refinement.actor.as_deref(), Some("refine:global"));
        assert_eq!((refine.created, refine.proposals), (1, 1));

        let detail = refine_detail(&store, "refine_1").unwrap();
        assert_eq!(detail.events.len(), 1);
        assert_eq!(detail.events[0].event.action, "created");
        assert_eq!(
            detail.events[0]
                .lesson
                .as_ref()
                .map(|lesson| lesson.id.as_str()),
            Some(report.created[0].as_str())
        );
        assert_eq!(detail.proposals.len(), 1);
        let history = lesson_history(&store, &report.created[0]).unwrap();
        assert_eq!(history.len(), 1);
        assert_eq!(history[0].event.actor, "refine:global");

        // La vue d'un autre projet voit la proposition (Review de tous les
        // projets), pas la leçon ni la refine.
        let other = lessons_overview(&store, &root, "/work/b").unwrap();
        assert_eq!(other.proposals.len(), 1);
        assert_eq!(other.lessons.len(), 1);
        assert!(other.refines.is_empty());
        let _ = std::fs::remove_dir_all(root);
    }

    fn fixture() -> (AppStore, std::path::PathBuf, ThreadContext) {
        let root = std::env::temp_dir().join(format!(
            "yusai-review-actions-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |elapsed| elapsed.as_nanos())
        ));
        std::fs::create_dir_all(root.join("agent")).unwrap();
        let store = AppStore::open_at(root.join("state.sqlite3")).unwrap();
        let thread = ThreadContext {
            conversation_id: "conv-a".to_string(),
            workspace_id: "/work/a".to_string(),
        };
        (store, root, thread)
    }

    fn import(store: &AppStore, root: &Path, thread: &ThreadContext, details: serde_json::Value) {
        import_refinement_outcome_as(
            store,
            &root.join("agent"),
            thread,
            &details,
            "refine:retain",
        )
        .unwrap()
        .unwrap();
    }

    fn create(id: &str, content: &str) -> serde_json::Value {
        json!({
            "action": "create", "kind": "memory", "id": id,
            "title": "T", "content": content, "applied": true,
        })
    }

    fn user_lesson(store: &AppStore, content: &str) -> Lesson {
        let new = NewLesson {
            level: LessonLevel::Project,
            workspace_id: Some("/work/a".to_string()),
            project_type: None,
            kind: LessonKind::Memory,
            title: "T".to_string(),
            content: content.to_string(),
        };
        match store.insert_lesson(&new, &LessonOrigin::user()).unwrap() {
            InsertLessonOutcome::Created(lesson) => lesson,
            other => panic!("not created: {other:?}"),
        }
    }

    #[test]
    fn accepting_a_proposal_applies_it_and_rejecting_leaves_the_lesson() {
        let (store, root, thread) = fixture();
        // Une refine globale : une leçon projet, proposée pour le global.
        import(
            &store,
            &root,
            &thread,
            json!({ "refinementId": "r1", "scope": "global", "edits": [create("a", "Lancer cargo test.")] }),
        );
        let promote = store.pending_lesson_proposals().unwrap().remove(0);
        let lesson_id = promote.lesson_id.clone().unwrap();
        // Vers le type : il faut un type confirmé.
        assert!(accept_proposal(&store, &promote.id, Some(LessonLevel::Type)).is_err());
        store
            .set_project_type(
                "/work/a",
                Some("rust"),
                sinew_app::store::ProjectTypeSource::User,
            )
            .unwrap();
        accept_proposal(&store, &promote.id, Some(LessonLevel::Type)).unwrap();
        let moved = store.lesson(&lesson_id).unwrap().unwrap();
        assert_eq!(moved.level, LessonLevel::Type);
        assert_eq!(moved.project_type.as_deref(), Some("rust"));
        assert_eq!(
            store.lesson_proposal(&promote.id).unwrap().unwrap().status,
            ProposalStatus::Accepted
        );
        let last = store.lesson_events(&lesson_id).unwrap().pop().unwrap();
        assert_eq!(
            (last.action.as_str(), last.actor.as_str()),
            ("promoted", "user")
        );

        // Une refine propose de changer la leçon (niveau type) : refus, puis
        // une autre acceptée.
        let change = |id: &str, content: &str| {
            json!({ "refinementId": id, "edits": [{
                "action": "update", "kind": "memory", "id": lesson_id,
                "title": "T", "content": content, "applied": true,
            }] })
        };
        import(
            &store,
            &root,
            &thread,
            change("r2", "Lancer cargo nextest."),
        );
        let rejected = store.pending_lesson_proposals().unwrap().remove(0);
        reject_proposal(&store, &rejected.id).unwrap();
        assert_eq!(
            store.lesson(&lesson_id).unwrap().unwrap().content,
            "Lancer cargo test."
        );
        assert_eq!(
            store
                .lesson_events(&lesson_id)
                .unwrap()
                .pop()
                .unwrap()
                .action,
            "rejected"
        );
        import(
            &store,
            &root,
            &thread,
            change("r3", "Lancer cargo test --workspace."),
        );
        let accepted = store.pending_lesson_proposals().unwrap().remove(0);
        accept_proposal(&store, &accepted.id, None).unwrap();
        assert_eq!(
            store.lesson(&lesson_id).unwrap().unwrap().content,
            "Lancer cargo test --workspace."
        );

        // Une refine propose de l'archiver : accepté.
        import(
            &store,
            &root,
            &thread,
            json!({ "refinementId": "r4", "edits": [{
                "action": "delete", "kind": "memory", "id": lesson_id, "applied": true,
            }] }),
        );
        let archive = store.pending_lesson_proposals().unwrap().remove(0);
        accept_proposal(&store, &archive.id, None).unwrap();
        assert_eq!(
            store.lesson(&lesson_id).unwrap().unwrap().status,
            LessonStatus::Archived
        );
        assert!(store.pending_lesson_proposals().unwrap().is_empty());
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn skill_proposals_wait_and_user_moves_close_promotions() {
        let (store, root, thread) = fixture();
        import(
            &store,
            &root,
            &thread,
            json!({ "refinementId": "r1", "scope": "global", "edits": [
                create("a", "Lancer cargo test."),
                {
                    "action": "create", "kind": "skill", "id": "fmt", "title": "Format",
                    "content": "Formater.", "applied": true,
                    "reference": { "type": "python", "import": "fmt", "callable": "run" },
                    "arguments": {},
                },
            ] }),
        );
        let pending = store.pending_lesson_proposals().unwrap();
        let skill = pending
            .iter()
            .find(|proposal| proposal.kind == ProposalKind::Skill)
            .unwrap();
        assert!(accept_proposal(&store, &skill.id, None).is_err());
        let promote = pending
            .iter()
            .find(|proposal| proposal.kind == ProposalKind::Promote)
            .unwrap();
        // L'utilisateur monte la leçon lui-même : la proposition est caduque.
        let moved = set_lesson_level(
            &store,
            promote.lesson_id.as_deref().unwrap(),
            LessonLevel::Global,
        )
        .unwrap();
        assert_eq!(moved.level, LessonLevel::Global);
        let left = store.pending_lesson_proposals().unwrap();
        assert_eq!(left.len(), 1);
        assert_eq!(left[0].kind, ProposalKind::Skill);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn undo_archives_the_refine_lessons_and_brings_old_texts_back() {
        let (store, root, thread) = fixture();
        let kept = user_lesson(&store, "Un commit par point.");
        let touched = user_lesson(&store, "Répondre en français.");
        let update = |lesson: &Lesson, content: &str| {
            json!({
                "action": "update", "kind": "memory", "id": lesson.id,
                "title": "T", "content": content, "applied": true,
            })
        };
        import(
            &store,
            &root,
            &thread,
            json!({ "refinementId": "r1", "scope": "global", "edits": [
                create("a", "Lancer cargo test."),
                update(&kept, "Un commit par point, sans pousser."),
                update(&touched, "Répondre en anglais."),
            ] }),
        );
        let created = store.refinement_events("r1").unwrap()[0].lesson_id.clone();
        // Retouchée par l'utilisateur après la refine : Undo n'y touche pas.
        store
            .update_lesson(
                &touched.id,
                "T",
                "Répondre en allemand.",
                &LessonOrigin::user(),
            )
            .unwrap();
        assert_eq!(store.pending_lesson_proposals().unwrap().len(), 1);

        let report = undo_refine(&store, "r1").unwrap();
        assert_eq!(report.archived, vec![created.clone()]);
        assert_eq!(report.reverted, vec![kept.id.clone()]);
        assert_eq!(report.skipped.len(), 1, "report: {report:?}");
        assert!(report.skipped[0].contains("changed since the refine"));
        assert_eq!(
            store.lesson(&created).unwrap().unwrap().status,
            LessonStatus::Archived
        );
        assert_eq!(
            store.lesson(&kept.id).unwrap().unwrap().content,
            "Un commit par point."
        );
        assert_eq!(
            store.lesson(&touched.id).unwrap().unwrap().content,
            "Répondre en allemand."
        );
        // La proposition de montée de la leçon créée tombe avec elle.
        assert!(store.pending_lesson_proposals().unwrap().is_empty());
        for id in [&created, &kept.id] {
            let last = store.lesson_events(id).unwrap().pop().unwrap();
            assert_eq!(last.actor, "user");
        }
        assert!(store
            .imported_refinement("r1")
            .unwrap()
            .unwrap()
            .undone_at_ms
            .is_some());
        assert!(undo_refine(&store, "r1").is_err());
        let _ = std::fs::remove_dir_all(root);
    }
}
