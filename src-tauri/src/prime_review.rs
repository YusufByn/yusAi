//! Ce que la vue « Lessons » du chat Prime montre d'un projet : les
//! propositions en attente (de tous les projets), les leçons du projet
//! (le sien, son type confirmé, le global) et les refines importées pour
//! lui, refine par refine.

use std::collections::HashMap;
use std::path::Path;

use anyhow::Result;
use serde::Serialize;
use sinew_app::store::{
    AppStore, ImportedRefinement, Lesson, LessonEvent, LessonProposal, LessonScope,
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
}
