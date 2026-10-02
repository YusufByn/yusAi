//! Leçons retenues des conversations Prime (couche de rétention yusAi).
//!
//! Prime apprend par `/refine` mais n'a que deux portées (session, global)
//! et ses écritures ne tombent pas là où le modèle relit (voir CONTEXT.md,
//! « Harness de Prime »). yusAi range donc lui-même les leçons, par niveau :
//! - `project` : un projet (son `workspace_id`, c'est-à-dire son chemin) ;
//! - `type` : tous les projets d'un même type (nom libre, choisi par projet) ;
//! - `global` : tous les projets.
//!
//! Une leçon arrive au niveau projet ; elle ne monte qu'après validation
//! (table `lesson_proposals`). Chaque changement laisse une trace dans
//! `lesson_events`, qui n'est jamais effacée.

use anyhow::{anyhow, bail, Context, Result};
use rusqlite::{params, Connection, OptionalExtension, Row};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use uuid::Uuid;

use super::{now_ms, AppStore};

pub(super) fn ensure_lessons_tables(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        r#"
        create table if not exists lessons (
            id text primary key,
            level text not null check (level in ('project', 'type', 'global')),
            scope_key text not null,
            workspace_id text,
            project_type text,
            kind text not null check (kind in ('memory', 'prompt', 'subagent')),
            title text not null,
            content text not null,
            content_hash text not null,
            status text not null check (status in ('active', 'archived')),
            pinned integer not null default 0,
            created_at_ms integer not null,
            updated_at_ms integer not null
        );
        create unique index if not exists idx_lessons_active_content
            on lessons(scope_key, content_hash) where status = 'active';
        create index if not exists idx_lessons_scope
            on lessons(scope_key, status);

        create table if not exists lesson_events (
            id integer primary key autoincrement,
            lesson_id text not null references lessons(id),
            at_ms integer not null,
            action text not null,
            actor text not null,
            conversation_id text,
            refinement_id text,
            before_json text,
            after_json text
        );
        create index if not exists idx_lesson_events_lesson
            on lesson_events(lesson_id, id);

        create table if not exists lesson_proposals (
            id text primary key,
            lesson_id text references lessons(id),
            kind text not null check (kind in ('promote', 'change', 'archive', 'skill')),
            target_level text check (target_level in ('project', 'type', 'global')),
            payload_json text not null,
            status text not null check (status in ('pending', 'accepted', 'rejected')),
            conversation_id text,
            refinement_id text,
            created_at_ms integer not null,
            decided_at_ms integer
        );
        create index if not exists idx_lesson_proposals_status
            on lesson_proposals(status, created_at_ms);

        create table if not exists prime_imported_refinements (
            refinement_id text primary key,
            conversation_id text,
            imported_at_ms integer not null
        );

        create table if not exists prime_projects (
            workspace_id text primary key,
            project_type text,
            type_source text not null check (type_source in ('suggested', 'user')),
            updated_at_ms integer not null
        );

        create table if not exists prime_refine_state (
            conversation_id text primary key,
            user_turns_since_refine integer not null default 0,
            last_refined_at_ms integer,
            pending integer not null default 0
        );
        "#,
    )
    .context("unable to create lesson tables")?;
    ensure_refinement_failures_column(conn)
}

/// v11 : les opérations d'une refine qui ont échoué à l'import.
fn ensure_refinement_failures_column(conn: &Connection) -> Result<()> {
    let mut statement = conn
        .prepare("pragma table_info(prime_imported_refinements)")
        .context("unable to inspect imported refinements")?;
    let has_column = statement
        .query_map([], |row| row.get::<_, String>(1))
        .context("unable to inspect imported refinements")?
        .collect::<rusqlite::Result<Vec<String>>>()
        .context("unable to inspect imported refinements")?
        .iter()
        .any(|name| name == "failures_json");
    if !has_column {
        conn.execute_batch(
            "alter table prime_imported_refinements
                add column failures_json text not null default '[]';",
        )
        .context("unable to add refinement failures column")?;
    }
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LessonLevel {
    Project,
    Type,
    Global,
}

impl LessonLevel {
    fn as_str(self) -> &'static str {
        match self {
            Self::Project => "project",
            Self::Type => "type",
            Self::Global => "global",
        }
    }

    fn parse(text: &str) -> Result<Self> {
        match text {
            "project" => Ok(Self::Project),
            "type" => Ok(Self::Type),
            "global" => Ok(Self::Global),
            other => Err(anyhow!("unknown lesson level {other:?}")),
        }
    }
}

/// Le `kind` d'une entrée du harness de Prime gardé comme leçon : `memory`
/// (fait), `prompt` (consigne), `subagent` (rôle de délégation). Les
/// entrées `skill` deviennent des propositions, pas des leçons.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LessonKind {
    Memory,
    Prompt,
    Subagent,
}

impl LessonKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::Memory => "memory",
            Self::Prompt => "prompt",
            Self::Subagent => "subagent",
        }
    }

    fn parse(text: &str) -> Result<Self> {
        match text {
            "memory" => Ok(Self::Memory),
            "prompt" => Ok(Self::Prompt),
            "subagent" => Ok(Self::Subagent),
            other => Err(anyhow!("unknown lesson kind {other:?}")),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LessonStatus {
    Active,
    Archived,
}

impl LessonStatus {
    fn as_str(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Archived => "archived",
        }
    }

    fn parse(text: &str) -> Result<Self> {
        match text {
            "active" => Ok(Self::Active),
            "archived" => Ok(Self::Archived),
            other => Err(anyhow!("unknown lesson status {other:?}")),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Lesson {
    pub id: String,
    pub level: LessonLevel,
    /// Le projet d'origine (toujours renseigné quand on le connaît) ; il ne
    /// décide de l'application que pour le niveau `project`.
    pub workspace_id: Option<String>,
    /// Le type visé, pour le niveau `type`.
    pub project_type: Option<String>,
    pub kind: LessonKind,
    pub title: String,
    pub content: String,
    pub status: LessonStatus,
    pub pinned: bool,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
}

/// Une leçon à créer. Le niveau `project` exige `workspace_id`, le niveau
/// `type` exige `project_type`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewLesson {
    pub level: LessonLevel,
    pub workspace_id: Option<String>,
    pub project_type: Option<String>,
    pub kind: LessonKind,
    pub title: String,
    pub content: String,
}

/// Qui a causé un changement : `refine:close`, `refine:retain`,
/// `refine:compaction`, `refine:agent`, `harness:global`, `user`…
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LessonOrigin {
    pub actor: String,
    pub conversation_id: Option<String>,
    pub refinement_id: Option<String>,
}

impl LessonOrigin {
    pub fn user() -> Self {
        Self {
            actor: "user".to_string(),
            ..Self::default()
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LessonEvent {
    pub id: i64,
    pub lesson_id: String,
    pub at_ms: i64,
    pub action: String,
    pub actor: String,
    pub conversation_id: Option<String>,
    pub refinement_id: Option<String>,
    pub before: Option<Value>,
    pub after: Option<Value>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InsertLessonOutcome {
    Created(Lesson),
    /// Une leçon active applicable porte déjà ce texte (normalisé).
    Duplicate {
        existing_id: String,
    },
}

/// Un projet et son type : les leçons qui s'y appliquent sont celles du
/// projet, de son type et les globales.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LessonScope {
    pub workspace_id: String,
    pub project_type: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ProposalKind {
    Promote,
    Change,
    Archive,
    Skill,
}

impl ProposalKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::Promote => "promote",
            Self::Change => "change",
            Self::Archive => "archive",
            Self::Skill => "skill",
        }
    }

    fn parse(text: &str) -> Result<Self> {
        match text {
            "promote" => Ok(Self::Promote),
            "change" => Ok(Self::Change),
            "archive" => Ok(Self::Archive),
            "skill" => Ok(Self::Skill),
            other => Err(anyhow!("unknown proposal kind {other:?}")),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ProposalStatus {
    Pending,
    Accepted,
    Rejected,
}

impl ProposalStatus {
    fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Accepted => "accepted",
            Self::Rejected => "rejected",
        }
    }

    fn parse(text: &str) -> Result<Self> {
        match text {
            "pending" => Ok(Self::Pending),
            "accepted" => Ok(Self::Accepted),
            "rejected" => Ok(Self::Rejected),
            other => Err(anyhow!("unknown proposal status {other:?}")),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LessonProposal {
    pub id: String,
    pub lesson_id: Option<String>,
    pub kind: ProposalKind,
    pub target_level: Option<LessonLevel>,
    pub payload: Value,
    pub status: ProposalStatus,
    pub conversation_id: Option<String>,
    pub refinement_id: Option<String>,
    pub created_at_ms: i64,
    pub decided_at_ms: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewProposal {
    pub lesson_id: Option<String>,
    pub kind: ProposalKind,
    pub target_level: Option<LessonLevel>,
    pub payload: Value,
    pub conversation_id: Option<String>,
    pub refinement_id: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ProjectTypeSource {
    /// Tiré des fichiers du projet, pas encore confirmé.
    Suggested,
    /// Choisi dans le sélecteur.
    User,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProjectTypeSetting {
    pub project_type: Option<String>,
    pub source: ProjectTypeSource,
}

/// Une refine de Prime déjà importée, et ce qui a échoué à l'import.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ImportedRefinement {
    pub refinement_id: String,
    pub conversation_id: Option<String>,
    pub imported_at_ms: i64,
    pub failures: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RefineState {
    pub user_turns_since_refine: i64,
    pub last_refined_at_ms: Option<i64>,
    pub pending: bool,
}

/// Le texte comparé pour les doublons : minuscules, lettres et chiffres
/// seulement, espaces réduits.
pub fn normalize_lesson_text(text: &str) -> String {
    let mut normalized = String::with_capacity(text.len());
    let mut space = false;
    for ch in text.chars().flat_map(char::to_lowercase) {
        if ch.is_alphanumeric() {
            if space && !normalized.is_empty() {
                normalized.push(' ');
            }
            space = false;
            normalized.push(ch);
        } else {
            space = true;
        }
    }
    normalized
}

/// Un type de projet comparé sans casse ni espaces de bord.
pub fn normalize_project_type(name: &str) -> String {
    name.trim().to_lowercase()
}

fn content_hash(content: &str) -> String {
    let digest = Sha256::digest(normalize_lesson_text(content).as_bytes());
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn scope_key(
    level: LessonLevel,
    workspace_id: Option<&str>,
    project_type: Option<&str>,
) -> Result<String> {
    match level {
        LessonLevel::Project => {
            let workspace = workspace_id
                .filter(|id| !id.is_empty())
                .ok_or_else(|| anyhow!("a project lesson needs a workspace id"))?;
            Ok(format!("project:{workspace}"))
        }
        LessonLevel::Type => {
            let project_type = project_type
                .map(normalize_project_type)
                .filter(|name| !name.is_empty())
                .ok_or_else(|| anyhow!("a type lesson needs a project type"))?;
            Ok(format!("type:{project_type}"))
        }
        LessonLevel::Global => Ok("global".to_string()),
    }
}

/// Les clés des niveaux qui s'appliquent à un projet : le projet, son
/// type, le global.
fn applicable_scope_keys(scope: &LessonScope) -> Vec<String> {
    let mut keys = vec![format!("project:{}", scope.workspace_id)];
    if let Some(project_type) = scope
        .project_type
        .as_deref()
        .map(normalize_project_type)
        .filter(|name| !name.is_empty())
    {
        keys.push(format!("type:{project_type}"));
    }
    keys.push("global".to_string());
    keys
}

/// Les clés qui couvrent une clé donnée (pour les doublons) : une leçon
/// projet est couverte par son projet, le type du projet et le global ; une
/// leçon de type par son type et le global ; une globale par le global.
fn covering_scope_keys(conn: &Connection, level: LessonLevel, key: &str) -> Result<Vec<String>> {
    Ok(match level {
        LessonLevel::Project => {
            let workspace_id = key.trim_start_matches("project:");
            let project_type = project_type_of(conn, workspace_id)?.and_then(|s| s.project_type);
            applicable_scope_keys(&LessonScope {
                workspace_id: workspace_id.to_string(),
                project_type,
            })
        }
        LessonLevel::Type => vec![key.to_string(), "global".to_string()],
        LessonLevel::Global => vec!["global".to_string()],
    })
}

const LESSON_COLUMNS: &str = "id, level, workspace_id, project_type, kind, title, content, \
     status, pinned, created_at_ms, updated_at_ms";

fn lesson_from_row(row: &Row<'_>) -> rusqlite::Result<(Lesson, [String; 3])> {
    let level: String = row.get(1)?;
    let kind: String = row.get(4)?;
    let status: String = row.get(7)?;
    Ok((
        Lesson {
            id: row.get(0)?,
            // Remplacés juste après par les valeurs parsées.
            level: LessonLevel::Global,
            workspace_id: row.get(2)?,
            project_type: row.get(3)?,
            kind: LessonKind::Memory,
            title: row.get(5)?,
            content: row.get(6)?,
            status: LessonStatus::Active,
            pinned: row.get::<_, i64>(8)? != 0,
            created_at_ms: row.get(9)?,
            updated_at_ms: row.get(10)?,
        },
        [level, kind, status],
    ))
}

fn finish_lesson((mut lesson, [level, kind, status]): (Lesson, [String; 3])) -> Result<Lesson> {
    lesson.level = LessonLevel::parse(&level)?;
    lesson.kind = LessonKind::parse(&kind)?;
    lesson.status = LessonStatus::parse(&status)?;
    Ok(lesson)
}

fn lesson_by_id(conn: &Connection, id: &str) -> Result<Option<Lesson>> {
    conn.query_row(
        &format!("select {LESSON_COLUMNS} from lessons where id = ?1"),
        params![id],
        lesson_from_row,
    )
    .optional()
    .context("unable to read lesson")?
    .map(finish_lesson)
    .transpose()
}

fn active_lesson_with_hash(
    conn: &Connection,
    keys: &[String],
    hash: &str,
) -> Result<Option<String>> {
    for key in keys {
        let found: Option<String> = conn
            .query_row(
                "select id from lessons
                 where scope_key = ?1 and content_hash = ?2 and status = 'active'",
                params![key, hash],
                |row| row.get(0),
            )
            .optional()
            .context("unable to look up duplicate lessons")?;
        if found.is_some() {
            return Ok(found);
        }
    }
    Ok(None)
}

fn record_event(
    conn: &Connection,
    lesson_id: &str,
    action: &str,
    origin: &LessonOrigin,
    before: Option<&Lesson>,
    after: Option<&Lesson>,
) -> Result<()> {
    let to_json = |lesson: Option<&Lesson>| -> Result<Option<String>> {
        lesson
            .map(serde_json::to_string)
            .transpose()
            .context("unable to serialize lesson snapshot")
    };
    conn.execute(
        "insert into lesson_events
            (lesson_id, at_ms, action, actor, conversation_id, refinement_id, before_json, after_json)
         values (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        params![
            lesson_id,
            now_ms(),
            action,
            origin.actor,
            origin.conversation_id,
            origin.refinement_id,
            to_json(before)?,
            to_json(after)?,
        ],
    )
    .context("unable to record lesson event")?;
    Ok(())
}

fn project_type_of(conn: &Connection, workspace_id: &str) -> Result<Option<ProjectTypeSetting>> {
    conn.query_row(
        "select project_type, type_source from prime_projects where workspace_id = ?1",
        params![workspace_id],
        |row| Ok((row.get::<_, Option<String>>(0)?, row.get::<_, String>(1)?)),
    )
    .optional()
    .context("unable to read project type")?
    .map(|(project_type, source)| {
        Ok(ProjectTypeSetting {
            project_type,
            source: match source.as_str() {
                "suggested" => ProjectTypeSource::Suggested,
                "user" => ProjectTypeSource::User,
                other => bail!("unknown project type source {other:?}"),
            },
        })
    })
    .transpose()
}

impl AppStore {
    /// Crée une leçon, sauf si une leçon active qui la couvre porte déjà le
    /// même texte : le doublon est alors noté dans l'historique de la leçon
    /// existante.
    pub fn insert_lesson(
        &self,
        lesson: &NewLesson,
        origin: &LessonOrigin,
    ) -> Result<InsertLessonOutcome> {
        let mut conn = self.connection()?;
        let tx = conn
            .transaction()
            .context("unable to start lesson transaction")?;
        let key = scope_key(
            lesson.level,
            lesson.workspace_id.as_deref(),
            lesson.project_type.as_deref(),
        )?;
        let hash = content_hash(&lesson.content);
        let covering = covering_scope_keys(&tx, lesson.level, &key)?;
        if let Some(existing_id) = active_lesson_with_hash(&tx, &covering, &hash)? {
            record_event(&tx, &existing_id, "duplicate_skipped", origin, None, None)?;
            tx.commit().context("unable to commit lesson transaction")?;
            return Ok(InsertLessonOutcome::Duplicate { existing_id });
        }
        let now = now_ms();
        let created = Lesson {
            id: format!("yl_{}", Uuid::new_v4().simple()),
            level: lesson.level,
            workspace_id: lesson.workspace_id.clone(),
            project_type: match lesson.level {
                LessonLevel::Type => lesson.project_type.as_deref().map(normalize_project_type),
                _ => None,
            },
            kind: lesson.kind,
            title: lesson.title.trim().to_string(),
            content: lesson.content.trim().to_string(),
            status: LessonStatus::Active,
            pinned: false,
            created_at_ms: now,
            updated_at_ms: now,
        };
        tx.execute(
            "insert into lessons
                (id, level, scope_key, workspace_id, project_type, kind, title, content,
                 content_hash, status, pinned, created_at_ms, updated_at_ms)
             values (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, 'active', 0, ?10, ?10)",
            params![
                created.id,
                created.level.as_str(),
                key,
                created.workspace_id,
                created.project_type,
                created.kind.as_str(),
                created.title,
                created.content,
                hash,
                now,
            ],
        )
        .context("unable to insert lesson")?;
        record_event(&tx, &created.id, "created", origin, None, Some(&created))?;
        tx.commit().context("unable to commit lesson transaction")?;
        Ok(InsertLessonOutcome::Created(created))
    }

    pub fn lesson(&self, id: &str) -> Result<Option<Lesson>> {
        lesson_by_id(&self.connection()?, id)
    }

    /// Les leçons actives qui s'appliquent à un projet : épinglées d'abord,
    /// puis projet, type, global ; les plus récentes d'abord dans un niveau.
    pub fn applicable_lessons(&self, scope: &LessonScope) -> Result<Vec<Lesson>> {
        let conn = self.connection()?;
        let keys = applicable_scope_keys(scope);
        let mut lessons = Vec::new();
        let mut statement = conn
            .prepare(&format!(
                "select {LESSON_COLUMNS} from lessons
                 where scope_key = ?1 and status = 'active'
                 order by updated_at_ms desc, id"
            ))
            .context("unable to prepare lesson query")?;
        for key in &keys {
            let rows = statement
                .query_map(params![key], lesson_from_row)
                .context("unable to query lessons")?;
            for row in rows {
                lessons.push(finish_lesson(row.context("unable to read lesson row")?)?);
            }
        }
        // Tri stable : l'ordre projet, type, global reste sous l'épinglage.
        lessons.sort_by_key(|lesson| !lesson.pinned);
        Ok(lessons)
    }

    /// Nouvelle version d'une leçon (titre et contenu), tracée avec l'avant
    /// et l'après.
    pub fn update_lesson(
        &self,
        id: &str,
        title: &str,
        content: &str,
        origin: &LessonOrigin,
    ) -> Result<Lesson> {
        let mut conn = self.connection()?;
        let tx = conn
            .transaction()
            .context("unable to start lesson transaction")?;
        let before = lesson_by_id(&tx, id)?.ok_or_else(|| anyhow!("unknown lesson {id}"))?;
        let mut after = before.clone();
        after.title = title.trim().to_string();
        after.content = content.trim().to_string();
        after.updated_at_ms = now_ms();
        tx.execute(
            "update lessons set title = ?2, content = ?3, content_hash = ?4, updated_at_ms = ?5
             where id = ?1",
            params![
                id,
                after.title,
                after.content,
                content_hash(&after.content),
                after.updated_at_ms
            ],
        )
        .with_context(|| format!("unable to update lesson {id} (same text as another lesson?)"))?;
        record_event(&tx, id, "updated", origin, Some(&before), Some(&after))?;
        tx.commit().context("unable to commit lesson transaction")?;
        Ok(after)
    }

    pub fn archive_lesson(&self, id: &str, origin: &LessonOrigin) -> Result<Lesson> {
        self.set_lesson_status(id, LessonStatus::Archived, "archived", origin)
    }

    /// Réactive une leçon archivée (refusé si une leçon active porte déjà
    /// le même texte au même niveau).
    pub fn restore_lesson(&self, id: &str, origin: &LessonOrigin) -> Result<Lesson> {
        self.set_lesson_status(id, LessonStatus::Active, "restored", origin)
    }

    fn set_lesson_status(
        &self,
        id: &str,
        status: LessonStatus,
        action: &str,
        origin: &LessonOrigin,
    ) -> Result<Lesson> {
        let mut conn = self.connection()?;
        let tx = conn
            .transaction()
            .context("unable to start lesson transaction")?;
        let before = lesson_by_id(&tx, id)?.ok_or_else(|| anyhow!("unknown lesson {id}"))?;
        let mut after = before.clone();
        after.status = status;
        after.updated_at_ms = now_ms();
        tx.execute(
            "update lessons set status = ?2, updated_at_ms = ?3 where id = ?1",
            params![id, status.as_str(), after.updated_at_ms],
        )
        .with_context(|| format!("unable to change the status of lesson {id}"))?;
        record_event(&tx, id, action, origin, Some(&before), Some(&after))?;
        tx.commit().context("unable to commit lesson transaction")?;
        Ok(after)
    }

    /// Change le niveau d'une leçon (montée validée, ou descente). Le
    /// niveau `type` demande le type visé.
    pub fn set_lesson_level(
        &self,
        id: &str,
        level: LessonLevel,
        project_type: Option<&str>,
        origin: &LessonOrigin,
    ) -> Result<Lesson> {
        let mut conn = self.connection()?;
        let tx = conn
            .transaction()
            .context("unable to start lesson transaction")?;
        let before = lesson_by_id(&tx, id)?.ok_or_else(|| anyhow!("unknown lesson {id}"))?;
        let key = scope_key(level, before.workspace_id.as_deref(), project_type)?;
        let mut after = before.clone();
        after.level = level;
        after.project_type = match level {
            LessonLevel::Type => project_type.map(normalize_project_type),
            _ => None,
        };
        after.updated_at_ms = now_ms();
        tx.execute(
            "update lessons set level = ?2, scope_key = ?3, project_type = ?4, updated_at_ms = ?5
             where id = ?1",
            params![
                id,
                level.as_str(),
                key,
                after.project_type,
                after.updated_at_ms
            ],
        )
        .with_context(|| {
            format!("unable to move lesson {id} (same text already at that level?)")
        })?;
        let rank = |level: LessonLevel| match level {
            LessonLevel::Project => 0,
            LessonLevel::Type => 1,
            LessonLevel::Global => 2,
        };
        let action = if rank(level) >= rank(before.level) {
            "promoted"
        } else {
            "demoted"
        };
        record_event(&tx, id, action, origin, Some(&before), Some(&after))?;
        tx.commit().context("unable to commit lesson transaction")?;
        Ok(after)
    }

    pub fn set_lesson_pinned(
        &self,
        id: &str,
        pinned: bool,
        origin: &LessonOrigin,
    ) -> Result<Lesson> {
        let mut conn = self.connection()?;
        let tx = conn
            .transaction()
            .context("unable to start lesson transaction")?;
        let before = lesson_by_id(&tx, id)?.ok_or_else(|| anyhow!("unknown lesson {id}"))?;
        let mut after = before.clone();
        after.pinned = pinned;
        after.updated_at_ms = now_ms();
        tx.execute(
            "update lessons set pinned = ?2, updated_at_ms = ?3 where id = ?1",
            params![id, i64::from(pinned), after.updated_at_ms],
        )
        .context("unable to pin lesson")?;
        let action = if pinned { "pinned" } else { "unpinned" };
        record_event(&tx, id, action, origin, Some(&before), Some(&after))?;
        tx.commit().context("unable to commit lesson transaction")?;
        Ok(after)
    }

    /// L'historique d'une leçon, du plus ancien au plus récent.
    pub fn lesson_events(&self, lesson_id: &str) -> Result<Vec<LessonEvent>> {
        let conn = self.connection()?;
        let mut statement = conn
            .prepare(
                "select id, lesson_id, at_ms, action, actor, conversation_id, refinement_id,
                        before_json, after_json
                 from lesson_events where lesson_id = ?1 order by id",
            )
            .context("unable to prepare lesson history query")?;
        let rows = statement
            .query_map(params![lesson_id], |row| {
                Ok((
                    LessonEvent {
                        id: row.get(0)?,
                        lesson_id: row.get(1)?,
                        at_ms: row.get(2)?,
                        action: row.get(3)?,
                        actor: row.get(4)?,
                        conversation_id: row.get(5)?,
                        refinement_id: row.get(6)?,
                        before: None,
                        after: None,
                    },
                    row.get::<_, Option<String>>(7)?,
                    row.get::<_, Option<String>>(8)?,
                ))
            })
            .context("unable to query lesson history")?;
        let parse = |text: Option<String>| -> Result<Option<Value>> {
            text.map(|text| serde_json::from_str(&text))
                .transpose()
                .context("unable to parse lesson snapshot")
        };
        let mut events = Vec::new();
        for row in rows {
            let (mut event, before, after) = row.context("unable to read lesson event")?;
            event.before = parse(before)?;
            event.after = parse(after)?;
            events.push(event);
        }
        Ok(events)
    }

    pub fn create_lesson_proposal(&self, proposal: &NewProposal) -> Result<LessonProposal> {
        let conn = self.connection()?;
        let created = LessonProposal {
            id: format!("yp_{}", Uuid::new_v4().simple()),
            lesson_id: proposal.lesson_id.clone(),
            kind: proposal.kind,
            target_level: proposal.target_level,
            payload: proposal.payload.clone(),
            status: ProposalStatus::Pending,
            conversation_id: proposal.conversation_id.clone(),
            refinement_id: proposal.refinement_id.clone(),
            created_at_ms: now_ms(),
            decided_at_ms: None,
        };
        conn.execute(
            "insert into lesson_proposals
                (id, lesson_id, kind, target_level, payload_json, status, conversation_id,
                 refinement_id, created_at_ms, decided_at_ms)
             values (?1, ?2, ?3, ?4, ?5, 'pending', ?6, ?7, ?8, null)",
            params![
                created.id,
                created.lesson_id,
                created.kind.as_str(),
                created.target_level.map(LessonLevel::as_str),
                serde_json::to_string(&created.payload).context("unable to serialize proposal")?,
                created.conversation_id,
                created.refinement_id,
                created.created_at_ms,
            ],
        )
        .context("unable to insert lesson proposal")?;
        Ok(created)
    }

    /// Les propositions en attente, les plus anciennes d'abord.
    pub fn pending_lesson_proposals(&self) -> Result<Vec<LessonProposal>> {
        let conn = self.connection()?;
        let mut statement = conn
            .prepare(
                "select id, lesson_id, kind, target_level, payload_json, status, conversation_id,
                        refinement_id, created_at_ms, decided_at_ms
                 from lesson_proposals where status = 'pending' order by created_at_ms, id",
            )
            .context("unable to prepare proposal query")?;
        let rows = statement
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, Option<String>>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, Option<String>>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, String>(5)?,
                    row.get::<_, Option<String>>(6)?,
                    row.get::<_, Option<String>>(7)?,
                    row.get::<_, i64>(8)?,
                    row.get::<_, Option<i64>>(9)?,
                ))
            })
            .context("unable to query proposals")?;
        let mut proposals = Vec::new();
        for row in rows {
            let (
                id,
                lesson_id,
                kind,
                target,
                payload,
                status,
                conversation,
                refinement,
                at,
                decided,
            ) = row.context("unable to read proposal")?;
            proposals.push(LessonProposal {
                id,
                lesson_id,
                kind: ProposalKind::parse(&kind)?,
                target_level: target.as_deref().map(LessonLevel::parse).transpose()?,
                payload: serde_json::from_str(&payload).context("unable to parse proposal")?,
                status: ProposalStatus::parse(&status)?,
                conversation_id: conversation,
                refinement_id: refinement,
                created_at_ms: at,
                decided_at_ms: decided,
            });
        }
        Ok(proposals)
    }

    /// Clôt une proposition en attente (son effet sur la leçon est appliqué
    /// par l'appelant).
    pub fn decide_lesson_proposal(&self, id: &str, status: ProposalStatus) -> Result<()> {
        if status == ProposalStatus::Pending {
            bail!("a decision is accepted or rejected");
        }
        let conn = self.connection()?;
        let changed = conn
            .execute(
                "update lesson_proposals set status = ?2, decided_at_ms = ?3
                 where id = ?1 and status = 'pending'",
                params![id, status.as_str(), now_ms()],
            )
            .context("unable to decide lesson proposal")?;
        if changed == 0 {
            bail!("no pending proposal {id}");
        }
        Ok(())
    }

    pub fn is_refinement_imported(&self, refinement_id: &str) -> Result<bool> {
        let conn = self.connection()?;
        conn.query_row(
            "select 1 from prime_imported_refinements where refinement_id = ?1",
            params![refinement_id],
            |_| Ok(()),
        )
        .optional()
        .context("unable to read imported refinements")
        .map(|found| found.is_some())
    }

    /// Note qu'une refine de Prime a été importée, avec les opérations qui
    /// ont échoué ; `false` si elle l'était déjà (l'import est idempotent
    /// par `refinementId`).
    pub fn mark_refinement_imported(
        &self,
        refinement_id: &str,
        conversation_id: Option<&str>,
        failures: &[String],
    ) -> Result<bool> {
        let conn = self.connection()?;
        let inserted = conn
            .execute(
                "insert or ignore into prime_imported_refinements
                    (refinement_id, conversation_id, imported_at_ms, failures_json)
                 values (?1, ?2, ?3, ?4)",
                params![
                    refinement_id,
                    conversation_id,
                    now_ms(),
                    serde_json::to_string(failures).context("unable to serialize failures")?
                ],
            )
            .context("unable to record imported refinement")?;
        Ok(inserted == 1)
    }

    pub fn imported_refinement(&self, refinement_id: &str) -> Result<Option<ImportedRefinement>> {
        let conn = self.connection()?;
        conn.query_row(
            "select conversation_id, imported_at_ms, failures_json
             from prime_imported_refinements where refinement_id = ?1",
            params![refinement_id],
            |row| {
                Ok((
                    row.get::<_, Option<String>>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, String>(2)?,
                ))
            },
        )
        .optional()
        .context("unable to read imported refinement")?
        .map(|(conversation_id, imported_at_ms, failures)| {
            Ok(ImportedRefinement {
                refinement_id: refinement_id.to_string(),
                conversation_id,
                imported_at_ms,
                failures: serde_json::from_str(&failures)
                    .context("unable to parse refinement failures")?,
            })
        })
        .transpose()
    }

    pub fn project_type(&self, workspace_id: &str) -> Result<Option<ProjectTypeSetting>> {
        project_type_of(&self.connection()?, workspace_id)
    }

    /// Enregistre le type d'un projet. Une suggestion ne remplace jamais un
    /// choix de l'utilisateur.
    pub fn set_project_type(
        &self,
        workspace_id: &str,
        project_type: Option<&str>,
        source: ProjectTypeSource,
    ) -> Result<ProjectTypeSetting> {
        let conn = self.connection()?;
        if source == ProjectTypeSource::Suggested {
            if let Some(current) = project_type_of(&conn, workspace_id)? {
                if current.source == ProjectTypeSource::User {
                    return Ok(current);
                }
            }
        }
        let project_type = project_type
            .map(str::trim)
            .filter(|name| !name.is_empty())
            .map(str::to_string);
        conn.execute(
            "insert into prime_projects (workspace_id, project_type, type_source, updated_at_ms)
             values (?1, ?2, ?3, ?4)
             on conflict(workspace_id) do update set
                project_type = excluded.project_type,
                type_source = excluded.type_source,
                updated_at_ms = excluded.updated_at_ms",
            params![
                workspace_id,
                project_type,
                match source {
                    ProjectTypeSource::Suggested => "suggested",
                    ProjectTypeSource::User => "user",
                },
                now_ms()
            ],
        )
        .context("unable to save project type")?;
        Ok(ProjectTypeSetting {
            project_type,
            source,
        })
    }

    pub fn refine_state(&self, conversation_id: &str) -> Result<RefineState> {
        let conn = self.connection()?;
        Ok(conn
            .query_row(
                "select user_turns_since_refine, last_refined_at_ms, pending
                 from prime_refine_state where conversation_id = ?1",
                params![conversation_id],
                |row| {
                    Ok(RefineState {
                        user_turns_since_refine: row.get(0)?,
                        last_refined_at_ms: row.get(1)?,
                        pending: row.get::<_, i64>(2)? != 0,
                    })
                },
            )
            .optional()
            .context("unable to read refine state")?
            .unwrap_or_default())
    }

    /// Un tour utilisateur de plus depuis la dernière refine.
    pub fn note_user_turn(&self, conversation_id: &str) -> Result<RefineState> {
        let conn = self.connection()?;
        conn.execute(
            "insert into prime_refine_state (conversation_id, user_turns_since_refine)
             values (?1, 1)
             on conflict(conversation_id) do update set
                user_turns_since_refine = user_turns_since_refine + 1",
            params![conversation_id],
        )
        .context("unable to count user turn")?;
        drop(conn);
        self.refine_state(conversation_id)
    }

    /// Une refine a eu lieu : le compteur repart de zéro, plus rien en
    /// attente.
    pub fn mark_refined(&self, conversation_id: &str) -> Result<()> {
        let conn = self.connection()?;
        conn.execute(
            "insert into prime_refine_state
                (conversation_id, user_turns_since_refine, last_refined_at_ms, pending)
             values (?1, 0, ?2, 0)
             on conflict(conversation_id) do update set
                user_turns_since_refine = 0,
                last_refined_at_ms = excluded.last_refined_at_ms,
                pending = 0",
            params![conversation_id, now_ms()],
        )
        .context("unable to record refine")?;
        Ok(())
    }

    pub fn set_refine_pending(&self, conversation_id: &str, pending: bool) -> Result<()> {
        let conn = self.connection()?;
        conn.execute(
            "insert into prime_refine_state (conversation_id, pending) values (?1, ?2)
             on conflict(conversation_id) do update set pending = excluded.pending",
            params![conversation_id, i64::from(pending)],
        )
        .context("unable to mark refine pending")?;
        Ok(())
    }

    /// À la sortie (Cmd+Q) : la refine d'une conversation qui a de nouveaux
    /// tours attend le prochain démarrage. Vrai si elle est mise en attente.
    pub fn defer_refine_if_unrefined(&self, conversation_id: &str) -> Result<bool> {
        let conn = self.connection()?;
        let changed = conn
            .execute(
                "update prime_refine_state set pending = 1
                 where conversation_id = ?1 and user_turns_since_refine > 0",
                params![conversation_id],
            )
            .context("unable to defer refine")?;
        Ok(changed > 0)
    }

    /// Les refines à faire au démarrage : celles mises en attente, et toute
    /// conversation qui a au moins un tour non retenu (sortie sans
    /// `on_exit` : Ctrl+C en dev, plantage).
    pub fn refines_due_at_start(&self) -> Result<Vec<String>> {
        let conn = self.connection()?;
        let mut statement = conn
            .prepare(
                "select conversation_id from prime_refine_state
                 where pending = 1 or user_turns_since_refine > 0
                 order by conversation_id",
            )
            .context("unable to prepare due refine query")?;
        let rows = statement
            .query_map([], |row| row.get(0))
            .context("unable to query due refines")?;
        rows.collect::<rusqlite::Result<Vec<String>>>()
            .context("unable to read due refines")
    }

    /// Oublie l'état de refine d'une conversation qui n'existe plus.
    pub fn forget_refine_state(&self, conversation_id: &str) -> Result<()> {
        let conn = self.connection()?;
        conn.execute(
            "delete from prime_refine_state where conversation_id = ?1",
            params![conversation_id],
        )
        .context("unable to forget refine state")?;
        Ok(())
    }

    /// Les conversations dont la refine attend le prochain démarrage.
    pub fn pending_refines(&self) -> Result<Vec<String>> {
        let conn = self.connection()?;
        let mut statement = conn
            .prepare(
                "select conversation_id from prime_refine_state
                 where pending = 1 order by conversation_id",
            )
            .context("unable to prepare pending refine query")?;
        let rows = statement
            .query_map([], |row| row.get(0))
            .context("unable to query pending refines")?;
        rows.collect::<rusqlite::Result<Vec<String>>>()
            .context("unable to read pending refines")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_store() -> (AppStore, std::path::PathBuf) {
        let path = std::env::temp_dir().join(format!(
            "sinew-store-lessons-test-{}.sqlite3",
            Uuid::new_v4()
        ));
        let store = AppStore { path: path.clone() };
        store.migrate().unwrap();
        (store, path)
    }

    fn project_lesson(workspace: &str, content: &str) -> NewLesson {
        NewLesson {
            level: LessonLevel::Project,
            workspace_id: Some(workspace.to_string()),
            project_type: None,
            kind: LessonKind::Memory,
            title: "Tests".to_string(),
            content: content.to_string(),
        }
    }

    fn origin(refinement: &str) -> LessonOrigin {
        LessonOrigin {
            actor: "refine:retain".to_string(),
            conversation_id: Some("conv-1".to_string()),
            refinement_id: Some(refinement.to_string()),
        }
    }

    fn created(outcome: InsertLessonOutcome) -> Lesson {
        match outcome {
            InsertLessonOutcome::Created(lesson) => lesson,
            other => panic!("expected a new lesson, got {other:?}"),
        }
    }

    #[test]
    fn migration_is_idempotent_and_sets_version_11() {
        let (store, path) = temp_store();
        store.migrate().unwrap();
        let conn = store.connection().unwrap();
        let version: i64 = conn
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .unwrap();
        assert_eq!(version, 11);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn lessons_apply_to_their_project_type_and_everyone() {
        let (store, path) = temp_store();
        let own = created(
            store
                .insert_lesson(
                    &project_lesson("/work/a", "Lancer cargo test."),
                    &origin("r1"),
                )
                .unwrap(),
        );
        created(
            store
                .insert_lesson(&project_lesson("/work/b", "Autre projet."), &origin("r1"))
                .unwrap(),
        );
        let typed = created(
            store
                .insert_lesson(
                    &NewLesson {
                        level: LessonLevel::Type,
                        workspace_id: Some("/work/b".to_string()),
                        project_type: Some(" Tauri ".to_string()),
                        kind: LessonKind::Prompt,
                        title: "Tauri".to_string(),
                        content: "Pas de Tokio sur le thread principal.".to_string(),
                    },
                    &origin("r2"),
                )
                .unwrap(),
        );
        assert_eq!(typed.project_type.as_deref(), Some("tauri"));
        let global = created(
            store
                .insert_lesson(
                    &NewLesson {
                        level: LessonLevel::Global,
                        workspace_id: None,
                        project_type: None,
                        kind: LessonKind::Memory,
                        title: "Langue".to_string(),
                        content: "Répondre en français.".to_string(),
                    },
                    &origin("r3"),
                )
                .unwrap(),
        );

        let ids = |scope: LessonScope| -> Vec<String> {
            store
                .applicable_lessons(&scope)
                .unwrap()
                .into_iter()
                .map(|lesson| lesson.id)
                .collect()
        };
        assert_eq!(
            ids(LessonScope {
                workspace_id: "/work/a".to_string(),
                project_type: Some("TAURI".to_string()),
            }),
            vec![own.id.clone(), typed.id.clone(), global.id.clone()]
        );
        assert_eq!(
            ids(LessonScope {
                workspace_id: "/work/a".to_string(),
                project_type: None,
            }),
            vec![own.id.clone(), global.id.clone()]
        );

        // Une leçon épinglée passe devant, sans changer l'ordre du reste.
        store
            .set_lesson_pinned(&global.id, true, &LessonOrigin::user())
            .unwrap();
        assert_eq!(
            ids(LessonScope {
                workspace_id: "/work/a".to_string(),
                project_type: Some("tauri".to_string()),
            }),
            vec![global.id, own.id, typed.id]
        );
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn duplicates_are_skipped_against_covering_levels() {
        let (store, path) = temp_store();
        store
            .set_project_type("/work/a", Some("Tauri"), ProjectTypeSource::User)
            .unwrap();
        let typed = created(
            store
                .insert_lesson(
                    &NewLesson {
                        level: LessonLevel::Type,
                        workspace_id: None,
                        project_type: Some("tauri".to_string()),
                        kind: LessonKind::Memory,
                        title: "Tests".to_string(),
                        content: "Toujours lancer cargo test.".to_string(),
                    },
                    &origin("r1"),
                )
                .unwrap(),
        );
        // Même texte à la ponctuation et à la casse près, dans un projet de
        // ce type : doublon.
        let outcome = store
            .insert_lesson(
                &project_lesson("/work/a", "toujours lancer   CARGO TEST !"),
                &origin("r2"),
            )
            .unwrap();
        assert_eq!(
            outcome,
            InsertLessonOutcome::Duplicate {
                existing_id: typed.id.clone()
            }
        );
        let history = store.lesson_events(&typed.id).unwrap();
        assert_eq!(
            history
                .iter()
                .map(|event| event.action.as_str())
                .collect::<Vec<_>>(),
            vec!["created", "duplicate_skipped"]
        );
        assert_eq!(history[1].refinement_id.as_deref(), Some("r2"));

        // Dans un projet d'un autre type, ce n'est pas un doublon.
        created(
            store
                .insert_lesson(
                    &project_lesson("/work/b", "Toujours lancer cargo test."),
                    &origin("r3"),
                )
                .unwrap(),
        );
        // Une leçon archivée ne bloque plus.
        store
            .archive_lesson(&typed.id, &LessonOrigin::user())
            .unwrap();
        created(
            store
                .insert_lesson(
                    &project_lesson("/work/a", "Toujours lancer cargo test."),
                    &origin("r4"),
                )
                .unwrap(),
        );
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn every_change_is_kept_in_history() {
        let (store, path) = temp_store();
        let lesson = created(
            store
                .insert_lesson(&project_lesson("/work/a", "Version 1."), &origin("r1"))
                .unwrap(),
        );
        let updated = store
            .update_lesson(&lesson.id, "Tests", "Version 2.", &origin("r2"))
            .unwrap();
        assert_eq!(updated.content, "Version 2.");
        store
            .set_lesson_level(
                &lesson.id,
                LessonLevel::Type,
                Some("Tauri"),
                &LessonOrigin::user(),
            )
            .unwrap();
        store
            .set_lesson_level(
                &lesson.id,
                LessonLevel::Project,
                None,
                &LessonOrigin::user(),
            )
            .unwrap();
        store
            .archive_lesson(&lesson.id, &LessonOrigin::user())
            .unwrap();
        store
            .restore_lesson(&lesson.id, &LessonOrigin::user())
            .unwrap();

        let history = store.lesson_events(&lesson.id).unwrap();
        let actions: Vec<&str> = history.iter().map(|event| event.action.as_str()).collect();
        assert_eq!(
            actions,
            vec!["created", "updated", "promoted", "demoted", "archived", "restored"]
        );
        assert_eq!(history[1].before.as_ref().unwrap()["content"], "Version 1.");
        assert_eq!(history[1].after.as_ref().unwrap()["content"], "Version 2.");
        assert_eq!(history[2].after.as_ref().unwrap()["projectType"], "tauri");
        let current = store.lesson(&lesson.id).unwrap().unwrap();
        assert_eq!(current.level, LessonLevel::Project);
        assert_eq!(current.status, LessonStatus::Active);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn a_change_cannot_collide_with_another_active_lesson() {
        let (store, path) = temp_store();
        created(
            store
                .insert_lesson(&project_lesson("/work/a", "Première."), &origin("r1"))
                .unwrap(),
        );
        let second = created(
            store
                .insert_lesson(&project_lesson("/work/a", "Seconde."), &origin("r1"))
                .unwrap(),
        );
        assert!(store
            .update_lesson(&second.id, "Tests", "première", &origin("r2"))
            .is_err());
        assert_eq!(
            store.lesson(&second.id).unwrap().unwrap().content,
            "Seconde."
        );
        assert_eq!(store.lesson_events(&second.id).unwrap().len(), 1);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn levels_need_their_scope() {
        let (store, path) = temp_store();
        let mut lesson = project_lesson("", "Sans projet.");
        assert!(store.insert_lesson(&lesson, &origin("r1")).is_err());
        lesson.level = LessonLevel::Type;
        lesson.workspace_id = Some("/work/a".to_string());
        assert!(store.insert_lesson(&lesson, &origin("r1")).is_err());
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn proposals_wait_for_a_decision() {
        let (store, path) = temp_store();
        let lesson = created(
            store
                .insert_lesson(&project_lesson("/work/a", "À promouvoir."), &origin("r1"))
                .unwrap(),
        );
        let proposal = store
            .create_lesson_proposal(&NewProposal {
                lesson_id: Some(lesson.id.clone()),
                kind: ProposalKind::Promote,
                target_level: Some(LessonLevel::Global),
                payload: serde_json::json!({ "reason": "écrit en global par le modèle" }),
                conversation_id: Some("conv-1".to_string()),
                refinement_id: None,
            })
            .unwrap();
        let pending = store.pending_lesson_proposals().unwrap();
        assert_eq!(pending, vec![proposal.clone()]);
        assert!(store
            .decide_lesson_proposal(&proposal.id, ProposalStatus::Pending)
            .is_err());
        store
            .decide_lesson_proposal(&proposal.id, ProposalStatus::Accepted)
            .unwrap();
        assert!(store.pending_lesson_proposals().unwrap().is_empty());
        assert!(store
            .decide_lesson_proposal(&proposal.id, ProposalStatus::Rejected)
            .is_err());
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn refinements_import_once() {
        let (store, path) = temp_store();
        assert!(!store.is_refinement_imported("refine_1").unwrap());
        assert!(store
            .mark_refinement_imported("refine_1", Some("conv-1"), &[])
            .unwrap());
        assert!(store.is_refinement_imported("refine_1").unwrap());
        assert!(store
            .mark_refinement_imported("refine_2", None, &["update yl_x: collision".to_string()])
            .unwrap());
        let imported = store.imported_refinement("refine_2").unwrap().unwrap();
        assert_eq!(imported.failures, vec!["update yl_x: collision"]);
        assert_eq!(
            store
                .imported_refinement("refine_1")
                .unwrap()
                .unwrap()
                .failures,
            Vec::<String>::new()
        );
        assert_eq!(store.imported_refinement("refine_3").unwrap(), None);
        assert!(!store
            .mark_refinement_imported("refine_1", Some("conv-1"), &[])
            .unwrap());
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn a_suggested_type_never_overrides_the_user_choice() {
        let (store, path) = temp_store();
        assert_eq!(store.project_type("/work/a").unwrap(), None);
        store
            .set_project_type("/work/a", Some("Rust"), ProjectTypeSource::Suggested)
            .unwrap();
        store
            .set_project_type("/work/a", Some(" Tauri "), ProjectTypeSource::User)
            .unwrap();
        let kept = store
            .set_project_type("/work/a", Some("Node"), ProjectTypeSource::Suggested)
            .unwrap();
        assert_eq!(kept.project_type.as_deref(), Some("Tauri"));
        assert_eq!(
            store.project_type("/work/a").unwrap(),
            Some(ProjectTypeSetting {
                project_type: Some("Tauri".to_string()),
                source: ProjectTypeSource::User,
            })
        );
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn refine_state_counts_turns_and_pending_refines() {
        let (store, path) = temp_store();
        assert_eq!(
            store.refine_state("conv-1").unwrap(),
            RefineState::default()
        );
        store.note_user_turn("conv-1").unwrap();
        store.note_user_turn("conv-1").unwrap();
        assert_eq!(
            store
                .note_user_turn("conv-1")
                .unwrap()
                .user_turns_since_refine,
            3
        );
        store.set_refine_pending("conv-1", true).unwrap();
        store.set_refine_pending("conv-2", true).unwrap();
        assert_eq!(store.pending_refines().unwrap(), vec!["conv-1", "conv-2"]);
        store.mark_refined("conv-1").unwrap();
        let state = store.refine_state("conv-1").unwrap();
        assert_eq!(state.user_turns_since_refine, 0);
        assert!(!state.pending);
        assert!(state.last_refined_at_ms.is_some());
        assert_eq!(store.pending_refines().unwrap(), vec!["conv-2"]);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn only_conversations_with_new_turns_wait_for_the_next_start() {
        let (store, path) = temp_store();
        assert!(!store.defer_refine_if_unrefined("never-seen").unwrap());
        store.note_user_turn("conv-1").unwrap();
        store.note_user_turn("conv-2").unwrap();
        store.mark_refined("conv-2").unwrap();
        assert!(store.defer_refine_if_unrefined("conv-1").unwrap());
        assert!(!store.defer_refine_if_unrefined("conv-2").unwrap());
        assert_eq!(store.pending_refines().unwrap(), vec!["conv-1"]);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn unrefined_turns_are_due_at_start_even_without_a_clean_exit() {
        let (store, path) = temp_store();
        // Un tour, puis Ctrl+C : pas d'`on_exit`, pas d'attente.
        store.note_user_turn("conv-crash").unwrap();
        // Refinée depuis son dernier tour : rien à faire.
        store.note_user_turn("conv-done").unwrap();
        store.mark_refined("conv-done").unwrap();
        // Mise en attente (refine ratée), compteur déjà remis à zéro.
        store.set_refine_pending("conv-failed", true).unwrap();
        assert_eq!(
            store.refines_due_at_start().unwrap(),
            vec!["conv-crash", "conv-failed"]
        );
        store.forget_refine_state("conv-crash").unwrap();
        assert_eq!(
            store.refine_state("conv-crash").unwrap(),
            RefineState::default()
        );
        assert_eq!(store.refines_due_at_start().unwrap(), vec!["conv-failed"]);
        let _ = std::fs::remove_file(path);
    }
}
