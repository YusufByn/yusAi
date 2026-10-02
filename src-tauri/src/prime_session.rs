//! Sessions Prime vues du chat : commandes Tauri (créer, envoyer un prompt,
//! annuler, fermer) sur le protocole natif du daemon, et relais des
//! `DaemonClientEvent` vers le front (événement Tauri `prime-event`).
//!
//! Le daemon n'est lancé qu'à la première création de session.
//!
//! Nettoyage des workers (une déconnexion ne les arrête pas,
//! pa-daemon/src/supervisor/clients.rs:354-373) :
//! - à la sortie de l'IDE ([`on_exit`]) : `Kill` des sessions créées par ce
//!   processus, puis `Shutdown` du daemon s'il n'en reste aucune ;
//! - après un crash ou une fermeture forcée, au démarrage suivant
//!   ([`reap_orphans_at_startup`]) : chaque session porte dans son
//!   `runtimeMetadata` le pid et l'identité de démarrage de l'IDE qui l'a
//!   créée ([`ide_runtime_metadata`]) ; celles dont l'IDE n'existe plus sont
//!   tuées ([`reap_orphaned_sessions`]), jamais celles d'une autre instance
//!   ouverte.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;

use anyhow::{anyhow, Result};
use pa_tui::daemon_client::{DaemonClient, DaemonClientEvent};
use pa_types::daemon::{DaemonCommand, DaemonErrorInfo, PromptInput, StreamingBehavior};
use serde::Serialize;
use serde_json::{json, Map, Value};
use sinew_app::tool_run::FileChange;
use tauri::{AppHandle, Emitter, Manager, State};

use crate::prime_diffs::PrimeDiffs;
use crate::prime_guidance::{with_guidance, Guidance};
use crate::prime_lessons::{
    import_global_harness_writes, import_refinement_outcome, import_thread_outcomes,
    refinement_outcome_details, ImportReport, ThreadContext,
};
use tokio::sync::{mpsc::UnboundedReceiver, watch, Mutex};

pub const PRIME_EVENT_NAME: &str = "prime-event";

/// Client du daemon partagé par toutes les fenêtres, connecté à la demande,
/// sessions créées par ce processus (tuées à la sortie) avec leur
/// conversation, et fichiers modifiés par leurs appels d'outils.
#[derive(Default)]
pub struct PrimeState {
    client: Mutex<Option<DaemonClient>>,
    sessions: StdMutex<HashMap<String, ThreadContext>>,
    diffs: StdMutex<PrimeDiffs>,
}

impl PrimeState {
    fn track(&self, active_session_id: &str, thread: ThreadContext) {
        if let Ok(mut sessions) = self.sessions.lock() {
            sessions.insert(active_session_id.to_string(), thread);
        }
    }

    fn thread(&self, active_session_id: &str) -> Option<ThreadContext> {
        self.sessions
            .lock()
            .ok()
            .and_then(|sessions| sessions.get(active_session_id).cloned())
    }

    fn untrack(&self, active_session_id: &str) {
        if let Ok(mut sessions) = self.sessions.lock() {
            sessions.remove(active_session_id);
        }
        self.with_diffs(|diffs| diffs.forget(active_session_id));
    }

    fn with_diffs(&self, f: impl FnOnce(&mut PrimeDiffs)) {
        if let Ok(mut diffs) = self.diffs.lock() {
            f(&mut diffs);
        }
    }
}

/// Clé du marquage yusAi dans le `runtimeMetadata` du `Create`. Prime ne
/// lit que ses propres clés (`kind`, `parentActiveSessionId`,
/// pa-daemon/src/worker/create.rs:153-175,
/// pa-daemon/src/supervisor_parent_death.rs:221-228).
const IDE_METADATA_KEY: &str = "yusai";

/// Marquage de l'IDE courant : son pid et son identité de démarrage, qui
/// distingue un pid réutilisé par un autre processus
/// (pa-types/src/platform/process.rs:22).
pub fn ide_runtime_metadata() -> Value {
    let pid = std::process::id();
    json!({
        IDE_METADATA_KEY: {
            "idePid": pid,
            "ideProcessStartId": pa_types::platform::process::process_start_id(pid),
        }
    })
}

/// L'IDE marqué existe-t-il encore ? Sans identité enregistrée, rien ne
/// prouve sa mort : la session est gardée.
fn ide_alive(marker: &Value) -> bool {
    let Some(pid) = marker
        .get("idePid")
        .and_then(Value::as_u64)
        .and_then(|pid| u32::try_from(pid).ok())
    else {
        return true;
    };
    match marker.get("ideProcessStartId").and_then(Value::as_str) {
        Some(expected) => {
            pa_types::platform::process::process_start_id(pid).as_deref() == Some(expected)
        }
        None => true,
    }
}

/// Un `DaemonClientEvent` tel que le front le reçoit.
#[derive(Debug, Clone, Serialize)]
#[serde(
    tag = "kind",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum PrimeEventPayload {
    /// Événement de session brut (`message_update`, `agent_end`, …).
    SessionEvent {
        active_session_id: String,
        event: Value,
    },
    SessionClosed {
        active_session_id: String,
        reason: String,
    },
    /// Connexion au superviseur perdue : les sessions ouvertes sont perdues.
    Disconnected { reason: String },
    /// Fichiers modifiés par un appel d'outil (voir `prime_diffs`), envoyés
    /// après son `tool_execution_end`.
    ToolFileChanges {
        active_session_id: String,
        tool_call_id: String,
        file_changes: Vec<FileChange>,
    },
}

impl PrimeEventPayload {
    fn from_client_event(event: DaemonClientEvent) -> Option<Self> {
        match event {
            DaemonClientEvent::SessionEvent {
                active_session_id,
                event,
                ..
            } => Some(Self::SessionEvent {
                active_session_id,
                event,
            }),
            DaemonClientEvent::SessionClosed {
                active_session_id,
                reason,
            } => Some(Self::SessionClosed {
                active_session_id,
                reason,
            }),
            DaemonClientEvent::DaemonClosing { reason, .. } => Some(Self::Disconnected { reason }),
            _ => None,
        }
    }
}

/// Crée une session marquée pour l'IDE courant (voir
/// [`create_session_with_metadata`]).
pub async fn create_session(client: &DaemonClient, config: Value) -> Result<String> {
    create_session_with_metadata(client, config, ide_runtime_metadata()).await
}

/// Dossier des fils Prime des conversations yusAi, hors de `sessions/` :
/// le superviseur y déplace les vieux fichiers vers `sessions-archive/`
/// (pa-daemon/src/session_archive.rs:1-17, 184-215), et un `Create` sur un
/// chemin disparu ouvrirait une session vide (pa-daemon/src/worker/create.rs:300-328).
pub(crate) const THREADS_DIR: &str = "yusai-threads";

/// Le fichier de session Prime d'une conversation yusAi.
pub fn thread_path(agent_dir: &Path, conversation_id: &str) -> Result<PathBuf> {
    let valid = !conversation_id.is_empty()
        && conversation_id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
    if !valid {
        return Err(anyhow!("invalid conversation id: {conversation_id:?}"));
    }
    Ok(agent_dir
        .join(THREADS_DIR)
        .join(format!("{conversation_id}.jsonl")))
}

/// Une session attachée et son historique : le `snapshot.messages` de
/// l'attach (pa-daemon/src/worker/connection.rs:860-894).
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OpenedSession {
    pub active_session_id: String,
    pub messages: Vec<Value>,
}

/// La commande `Create` : en mémoire sans `session_path` (comme l'ACP
/// daemon-attached, pa-daemon/src/acp/daemon.rs:537-560), sinon sur ce
/// fichier, rouvert s'il existe (pa-daemon/src/worker/create.rs:208-328).
fn create_command(
    config: Value,
    runtime_metadata: Value,
    session_path: Option<&Path>,
) -> DaemonCommand {
    DaemonCommand::Create {
        id: None,
        session_path: session_path.map(|path| path.to_string_lossy().into_owned()),
        continue_recent: None,
        no_session: session_path.is_none().then_some(true),
        name: None,
        config: Some(config),
        // Coupe la télémétrie de session du worker, qui ne lit pas
        // l'env (pa-daemon/src/agent_engine/lifecycle.rs:1075) ; le flag
        // suit le worker jusqu'à ses relances (descriptor.rs:113-117).
        telemetry_disabled: Some(true),
        runtime_metadata: Some(runtime_metadata),
        lifecycle: None,
        env: None,
        launch_env: None,
        rest: Map::default(),
    }
}

fn active_session_id_of(summary: &Value) -> Result<String> {
    summary
        .get("activeSessionId")
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| anyhow!("create response carries no activeSessionId: {summary}"))
}

/// S'attache à une session pour recevoir ses événements
/// (pa-daemon/src/acp/daemon.rs:584-595) ; renvoie son historique.
async fn attach(client: &DaemonClient, active_session_id: &str) -> Result<Vec<Value>> {
    let attached = client
        .request_ok(DaemonCommand::Attach {
            id: None,
            active_session_id: active_session_id.to_string(),
            client_id: None,
            capabilities: None,
            resume_cursor: None,
            telemetry_disabled: None,
            recovery_config: None,
            env: None,
            launch_env: None,
            rest: Map::default(),
        })
        .await?;
    Ok(attached
        .get("snapshot")
        .and_then(|snapshot| snapshot.get("messages"))
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default())
}

/// Crée une session sans fichier persistant et s'y attache. Renvoie
/// l'`activeSessionId`.
pub async fn create_session_with_metadata(
    client: &DaemonClient,
    config: Value,
    runtime_metadata: Value,
) -> Result<String> {
    let summary = client
        .request_ok(create_command(config, runtime_metadata, None))
        .await?;
    let active_session_id = active_session_id_of(&summary)?;
    if let Err(error) = attach(client, &active_session_id).await {
        let _ = kill_session(client, &active_session_id).await;
        return Err(error);
    }
    Ok(active_session_id)
}

/// Ouvre le fil d'une conversation depuis son fichier : le worker le
/// rouvre avec son modèle et son niveau de réflexion
/// (pa-daemon/src/worker/create.rs:226-280), ou le crée. Un fichier déjà
/// tenu par un worker vivant (autre fenêtre) est refusé avec
/// `SessionAlreadyActive` (pa-daemon/src/supervisor/worker_lifecycle.rs:384-391) :
/// on s'attache alors à cette session.
pub async fn open_thread(
    client: &DaemonClient,
    config: Value,
    session_path: &Path,
) -> Result<OpenedSession> {
    open_thread_with_metadata(client, config, session_path, ide_runtime_metadata()).await
}

/// [`open_thread`] avec un marquage explicite (tests : un IDE disparu).
pub async fn open_thread_with_metadata(
    client: &DaemonClient,
    config: Value,
    session_path: &Path,
    runtime_metadata: Value,
) -> Result<OpenedSession> {
    // Le dossier existe avant le verrou du worker : sinon le verrou retient
    // le chemin non canonique (pa-daemon/src/lease.rs:78-87) et refuse
    // ensuite d'écrire dans le fichier créé, dont le chemin canonique diffère
    // (lien `/var` -> `/private/var` sous macOS, lease.rs:392-396).
    if let Some(dir) = session_path.parent() {
        tokio::fs::create_dir_all(dir).await?;
    }
    let response = client
        .request(create_command(config, runtime_metadata, Some(session_path)))
        .await?;
    let (active_session_id, created) = if response.success {
        let summary = response.data.unwrap_or(Value::Null);
        (active_session_id_of(&summary)?, true)
    } else {
        match response.error_info {
            Some(DaemonErrorInfo::SessionAlreadyActive {
                active_session_id: Some(active_session_id),
                ..
            }) => (active_session_id, false),
            _ => {
                return Err(anyhow!(
                    "create failed: {}",
                    response.error.unwrap_or_default()
                ))
            }
        }
    };
    match attach(client, &active_session_id).await {
        Ok(messages) => Ok(OpenedSession {
            active_session_id,
            messages,
        }),
        Err(error) => {
            if created {
                let _ = kill_session(client, &active_session_id).await;
            }
            Err(error)
        }
    }
}

/// Supprime un fil : `Kill` des workers qui tiennent son fichier (relevés
/// par `List`), puis `delete_saved_session`, qui accepte un chemin hors de
/// `sessions/` (pa-daemon/src/saved_session_commands.rs:553-707) mais
/// refuse une session encore active. Sous macOS, Prime envoie le fichier à
/// la Corbeille par `trash` quand la commande existe, sinon le supprime
/// (saved_session_commands.rs:116-138). Un fichier encore présent après
/// coup est supprimé ici.
///
/// Prime n'efface que `session-artifacts/<nom du fichier>/`
/// (saved_session_commands.rs:142-154) ; les sous-agents vivent sous
/// `session-artifacts/<id de session>/` (pa-daemon/src/rlm_children.rs:867-882),
/// et l'id de session d'un fil yusAi n'est pas son nom de fichier. Ce
/// dossier part aussi à la Corbeille, comme le fichier. Renvoie les
/// sessions tuées.
pub async fn delete_thread(client: &DaemonClient, session_path: &Path) -> Result<Vec<String>> {
    let children_dir = thread_session_id(session_path).and_then(|session_id| {
        session_path
            .parent()?
            .parent()
            .map(|agent_dir| agent_dir.join("session-artifacts").join(session_id))
    });
    let target = pa_daemon::lease::canonical_session_path(session_path);
    let listed = client
        .request_ok(DaemonCommand::List {
            id: None,
            all: None,
            cwd: None,
            session_dir: None,
            include_client_owned: Some(true),
            rest: Map::default(),
        })
        .await?;
    let holders: Vec<String> = listed
        .get("sessions")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|session| {
            session
                .get("sessionFile")
                .and_then(Value::as_str)
                .is_some_and(|file| {
                    pa_daemon::lease::canonical_session_path(Path::new(file)) == target
                })
        })
        .filter_map(|session| session.get("activeSessionId").and_then(Value::as_str))
        .filter(|id| !id.is_empty())
        .map(str::to_string)
        .collect();
    for active_session_id in &holders {
        kill_session(client, active_session_id).await?;
    }
    if session_path.exists() {
        client
            .request_ok(DaemonCommand::DeleteSavedSession {
                id: None,
                active_session_id: None,
                session_path: session_path.to_string_lossy().into_owned(),
                rest: Map::default(),
            })
            .await?;
    }
    if session_path.exists() {
        tokio::fs::remove_file(session_path).await?;
    }
    if let Some(dir) = children_dir.filter(|dir| dir.is_dir()) {
        tokio::task::spawn_blocking(move || move_to_trash(&dir)).await??;
    }
    Ok(holders)
}

/// L'id de session écrit dans l'en-tête d'un fil
/// (`{"type":"session","id":…}`, première ligne du fichier).
fn thread_session_id(session_path: &Path) -> Option<String> {
    use std::io::BufRead;
    let file = std::fs::File::open(session_path).ok()?;
    let mut header = String::new();
    std::io::BufReader::new(file).read_line(&mut header).ok()?;
    let header: Value = serde_json::from_str(&header).ok()?;
    if header.get("type").and_then(Value::as_str) != Some("session") {
        return None;
    }
    let id = header.get("id").and_then(Value::as_str)?;
    let valid = !id.is_empty()
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
    valid.then(|| id.to_string())
}

/// Corbeille comme Prime pour le fichier du fil : la commande `trash` quand
/// elle existe (macOS 26 l'a), sinon suppression
/// (pa-daemon/src/saved_session_commands.rs:116-138).
fn move_to_trash(path: &Path) -> Result<()> {
    let trashed = std::process::Command::new("trash")
        .arg("--")
        .arg(path)
        .output()
        .is_ok_and(|output| output.status.success() || !path.exists());
    if !trashed && path.exists() {
        std::fs::remove_dir_all(path)?;
    }
    Ok(())
}

/// Envoie un prompt sans attendre la fin du tour : la réponse arrive par les
/// événements. `followUp` + `queueIfBusy` comme l'ACP
/// (pa-daemon/src/acp/daemon.rs:793-815) : un prompt pendant un tour est mis
/// en file au lieu d'être refusé.
pub async fn prompt(client: &DaemonClient, active_session_id: &str, message: &str) -> Result<()> {
    client
        .request_ok(DaemonCommand::Prompt {
            id: None,
            active_session_id: active_session_id.to_string(),
            message: message.to_string(),
            input: PromptInput {
                content: None,
                images: None,
                streaming_behavior: Some(StreamingBehavior::FollowUp),
                queue_if_busy: Some(true),
                expand_prompt_templates: None,
                source: None,
                agent_message_id: None,
                custom_message: None,
                queue_key: None,
                prefix_messages: None,
                admission_id: None,
                rlm_notice_nonce: None,
            },
            rest: Map::default(),
        })
        .await?;
    Ok(())
}

pub async fn abort(client: &DaemonClient, active_session_id: &str) -> Result<()> {
    client
        .request_ok(DaemonCommand::Abort {
            id: None,
            active_session_id: active_session_id.to_string(),
            rest: Map::default(),
        })
        .await?;
    Ok(())
}

/// Arrête la session et son worker (fermeture ACP, pa-daemon/src/acp/daemon.rs:1060-1090).
pub async fn kill_session(client: &DaemonClient, active_session_id: &str) -> Result<()> {
    let _ = abort(client, active_session_id).await;
    client
        .request_ok(DaemonCommand::Kill {
            id: None,
            active_session_id: active_session_id.to_string(),
            rest: Map::default(),
        })
        .await?;
    Ok(())
}

/// Tue les sessions marquées par un IDE qui n'existe plus. Le marquage se
/// lit dans les descripteurs que le superviseur persiste pour chaque worker,
/// commande `Create` comprise (pa-daemon/src/descriptor.rs:208-212) :
/// `List` ne le renvoie pas. Renvoie les sessions tuées.
pub async fn reap_orphaned_sessions(
    client: &DaemonClient,
    agent_dir: &Path,
    socket_path: &Path,
) -> Result<Vec<String>> {
    let dir = pa_daemon::descriptor::descriptor_dir(agent_dir, socket_path);
    let mut orphans = Vec::new();
    for entry in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
        let Ok(text) = std::fs::read_to_string(entry.path()) else {
            continue;
        };
        let Ok(descriptor) =
            serde_json::from_str::<pa_types::daemon::DaemonWorkerDescriptor>(&text)
        else {
            continue;
        };
        if descriptor.lifecycle == pa_types::daemon::DaemonWorkerLifecycle::Stopping {
            continue;
        }
        let marker = descriptor
            .create_command
            .rest
            .get("runtimeMetadata")
            .and_then(|metadata| metadata.get(IDE_METADATA_KEY));
        if marker.is_some_and(|marker| !ide_alive(marker)) {
            orphans.push(descriptor.root_active_session_id);
        }
    }
    let mut reaped = Vec::new();
    for active_session_id in orphans {
        if kill_session(client, &active_session_id).await.is_ok() {
            reaped.push(active_session_id);
        }
    }
    Ok(reaped)
}

/// Sortie de l'IDE : tue ses sessions, puis arrête le daemon s'il ne reste
/// aucune session (d'une autre instance ouverte, par exemple). Renvoie si le
/// daemon a été arrêté.
pub async fn close_ide_sessions(client: &DaemonClient, sessions: &[String]) -> Result<bool> {
    for active_session_id in sessions {
        let _ = kill_session(client, active_session_id).await;
    }
    let listed = client
        .request_ok(DaemonCommand::List {
            id: None,
            all: None,
            cwd: None,
            session_dir: None,
            include_client_owned: Some(true),
            rest: Map::default(),
        })
        .await?;
    let remaining = listed
        .get("sessions")
        .and_then(Value::as_array)
        .map_or(0, Vec::len);
    if remaining > 0 {
        return Ok(false);
    }
    client
        .request_ok(DaemonCommand::Shutdown {
            id: None,
            force: None,
            rest: Map::default(),
        })
        .await?;
    Ok(true)
}

/// Budget du nettoyage à la sortie : l'IDE ne doit pas rester bloqué.
const EXIT_CLEANUP_TIMEOUT: Duration = Duration::from_secs(5);

/// `RunEvent::Exit` : nettoyage borné dans le temps, sans lancer de daemon.
///
/// Appelé sur le thread principal depuis `applicationWillTerminate` sur
/// macOS, où un panic avorte le processus : rien de Tokio n'y tourne. Le
/// nettoyage s'exécute dans le runtime de Tauri et le thread principal
/// attend seulement sa fin sur un canal std.
pub fn on_exit(app: &AppHandle) {
    if app.try_state::<PrimeState>().is_none() {
        return;
    }
    let app = app.clone();
    let (done_tx, done_rx) = std::sync::mpsc::channel::<()>();
    tauri::async_runtime::spawn(async move {
        let state = app.state::<PrimeState>();
        let sessions: Vec<String> = state
            .sessions
            .lock()
            .map(|mut sessions| sessions.drain().map(|(id, _)| id).collect())
            .unwrap_or_default();
        let client = state.client.lock().await.clone();
        if let Some(client) = client.filter(|client| !*client.reader_dead().borrow()) {
            match close_ide_sessions(&client, &sessions).await {
                Ok(stopped) => tracing::info!(
                    sessions = sessions.len(),
                    daemon_stopped = stopped,
                    "prime exit cleanup done"
                ),
                Err(error) => tracing::warn!(error = %error, "prime exit cleanup failed"),
            }
            client.close();
        }
        let _ = done_tx.send(());
    });
    if done_rx.recv_timeout(EXIT_CLEANUP_TIMEOUT).is_err() {
        tracing::warn!("prime exit cleanup timed out");
    }
}

/// Démarrage de l'IDE : si un daemon yusAi tourne déjà (resté d'un crash),
/// on s'y connecte, ce qui tue les sessions orphelines
/// ([`connected_client`]). Sans daemon, rien à faire et rien n'est lancé.
pub fn reap_orphans_at_startup(app: AppHandle) {
    tauri::async_runtime::spawn(async move {
        let socket_path = crate::prime::daemon_socket_path();
        if !pa_daemon::socket::can_connect(&socket_path, Duration::from_millis(250)).await {
            return;
        }
        let state = app.state::<PrimeState>();
        if let Err(error) = connected_client(&app, &state).await {
            tracing::warn!(error = %error, "prime orphan cleanup could not connect");
        }
    });
}

/// Un modèle proposé dans le sélecteur du chat Prime.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PrimeModelOption {
    pub provider: String,
    pub id: String,
    pub name: String,
}

impl PrimeModelOption {
    fn from_value(model: &Value) -> Option<Self> {
        let provider = model.get("provider")?.as_str()?.to_string();
        let id = model.get("id")?.as_str()?.to_string();
        let name = model
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or(&id)
            .to_string();
        Some(Self { provider, id, name })
    }
}

/// Modèle et niveau de réflexion d'une session, avec les choix possibles.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PrimeSessionConfig {
    pub model: Option<PrimeModelOption>,
    pub thinking_level: Option<String>,
    pub available_thinking_levels: Vec<String>,
    pub models: Vec<PrimeModelOption>,
}

/// L'état du worker (`get_connection_state`) et ses modèles disponibles
/// (`get_available_models`), comme les sélecteurs de l'ACP
/// (pa-daemon/src/acp/wire_config.rs:274-320).
pub async fn session_config(
    client: &DaemonClient,
    active_session_id: &str,
) -> Result<PrimeSessionConfig> {
    let state = client
        .request_ok(DaemonCommand::GetConnectionState {
            id: None,
            active_session_id: active_session_id.to_string(),
            rest: Map::default(),
        })
        .await?;
    let available = client
        .request_ok(DaemonCommand::GetAvailableModels {
            id: None,
            active_session_id: active_session_id.to_string(),
            rest: Map::default(),
        })
        .await?;
    Ok(PrimeSessionConfig {
        model: state.get("model").and_then(PrimeModelOption::from_value),
        thinking_level: state
            .get("thinkingLevel")
            .and_then(Value::as_str)
            .map(str::to_string),
        available_thinking_levels: state
            .get("availableThinkingLevels")
            .and_then(|levels| serde_json::from_value(levels.clone()).ok())
            .unwrap_or_default(),
        models: available
            .get("models")
            .and_then(Value::as_array)
            .map(|models| {
                models
                    .iter()
                    .filter_map(PrimeModelOption::from_value)
                    .collect()
            })
            .unwrap_or_default(),
    })
}

/// Un sous-agent de la session, pour l'état affiché sur sa carte.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PrimeSubAgent {
    pub session_name: String,
    /// `running`, `done`, `error` ou `cancelled`
    /// (pa-daemon/src/rlm_children.rs:215-229).
    pub status: String,
    pub answer_preview: Option<String>,
}

/// Les sous-agents de la session (`get_rlm_children`,
/// pa-daemon/src/state_getters.rs:38-64) : les enfants ne poussent rien au
/// parent, leur état se lit à la demande.
pub async fn rlm_children(
    client: &DaemonClient,
    active_session_id: &str,
) -> Result<Vec<PrimeSubAgent>> {
    let data = client
        .request_ok(DaemonCommand::GetRlmChildren {
            id: None,
            active_session_id: active_session_id.to_string(),
            rest: Map::default(),
        })
        .await?;
    Ok(data
        .get("children")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|child| {
            Some(PrimeSubAgent {
                session_name: child.get("sessionName")?.as_str()?.to_string(),
                status: child
                    .get("status")
                    .and_then(Value::as_str)
                    .unwrap_or("running")
                    .to_string(),
                answer_preview: child
                    .get("answerPreview")
                    .and_then(Value::as_str)
                    .map(str::to_string),
            })
        })
        .collect())
}

/// Le dossier de travail du worker de la session : le `cwd` du `Create`
/// (pa-daemon/src/supervisor/worker_lifecycle.rs:136-145), dans lequel le
/// superviseur lance le worker (pa-daemon/src/supervisor/supervision.rs:416-449)
/// et que `get_connection_state` renvoie (pa-daemon/src/worker/summary.rs:77).
pub async fn session_cwd(client: &DaemonClient, active_session_id: &str) -> Result<PathBuf> {
    let state = client
        .request_ok(DaemonCommand::GetConnectionState {
            id: None,
            active_session_id: active_session_id.to_string(),
            rest: Map::default(),
        })
        .await?;
    state
        .get("cwd")
        .and_then(Value::as_str)
        .filter(|cwd| !cwd.is_empty())
        .map(PathBuf::from)
        .ok_or_else(|| anyhow!("prime session {active_session_id} reported no cwd"))
}

/// Change le modèle de la session ; le worker l'enregistre aussi comme défaut
/// des sessions suivantes (pa-daemon/src/model_switch.rs:123-126).
pub async fn set_model(
    client: &DaemonClient,
    active_session_id: &str,
    provider: &str,
    model_id: &str,
) -> Result<()> {
    client
        .request_ok(DaemonCommand::SetModel {
            id: None,
            active_session_id: active_session_id.to_string(),
            provider: provider.to_string(),
            model_id: model_id.to_string(),
            rest: Map::default(),
        })
        .await?;
    Ok(())
}

/// Change le niveau de réflexion (aussi enregistré comme défaut,
/// pa-daemon/src/model_switch.rs:221-228).
pub async fn set_thinking_level(
    client: &DaemonClient,
    active_session_id: &str,
    level: &str,
) -> Result<()> {
    client
        .request_ok(DaemonCommand::SetThinkingLevel {
            id: None,
            active_session_id: active_session_id.to_string(),
            level: level.to_string(),
            rest: Map::default(),
        })
        .await?;
    Ok(())
}

/// Config de création : le cwd de l'espace de travail. En dev,
/// `YUSAI_PRIME_FAUX_SCRIPT` remplace le modèle par une réponse scriptée
/// (le moteur `faux` de Prime, utilisé par ses tests e2e).
fn create_config(cwd: &str) -> Value {
    #[cfg_attr(not(debug_assertions), allow(unused_mut))]
    let mut config = json!({ "cwd": cwd });
    #[cfg(debug_assertions)]
    if let Some(script) = std::env::var_os("YUSAI_PRIME_FAUX_SCRIPT") {
        config["script"] = Value::String(script.to_string_lossy().into_owned());
    }
    config
}

/// Relaie les événements du client vers le front jusqu'à la perte du socket.
fn spawn_event_relay(
    app: AppHandle,
    mut events: UnboundedReceiver<DaemonClientEvent>,
    mut reader_dead: watch::Receiver<bool>,
) {
    tauri::async_runtime::spawn(async move {
        loop {
            tokio::select! {
                event = events.recv() => {
                    let Some(event) = event else { break };
                    let Some(payload) = PrimeEventPayload::from_client_event(event) else {
                        continue;
                    };
                    // L'événement part d'abord : les photos du projet se font
                    // ensuite, dans la tâche de la session.
                    let _ = app.emit(PRIME_EVENT_NAME, &payload);
                    let state = app.state::<PrimeState>();
                    match &payload {
                        PrimeEventPayload::SessionEvent { active_session_id, event } => {
                            state.with_diffs(|diffs| diffs.observe(active_session_id, event));
                            if event["type"] == "tool_execution_end" {
                                guard_global_harness(&app, &state, active_session_id);
                            }
                            if event["type"] == "message_end" {
                                if let Some(details) = refinement_outcome_details(&event["message"]) {
                                    import_live_refinement(&app, &state, active_session_id, details);
                                }
                            }
                        }
                        PrimeEventPayload::SessionClosed { active_session_id, .. } => {
                            state.with_diffs(|diffs| diffs.forget(active_session_id));
                        }
                        PrimeEventPayload::Disconnected { .. } => {
                            state.with_diffs(PrimeDiffs::clear);
                        }
                        PrimeEventPayload::ToolFileChanges { .. } => {}
                    }
                }
                changed = reader_dead.changed() => {
                    if changed.is_err() || *reader_dead.borrow() {
                        app.state::<PrimeState>().with_diffs(PrimeDiffs::clear);
                        let _ = app.emit(
                            PRIME_EVENT_NAME,
                            PrimeEventPayload::Disconnected {
                                reason: "daemon connection lost".into(),
                            },
                        );
                        break;
                    }
                }
            }
        }
    });
}

/// Les leçons et consignes de yusAi pour une session du projet. Sans
/// elles (magasin illisible), la session s'ouvre quand même.
async fn thread_guidance(app: &AppHandle, workspace_path: &str) -> Option<Guidance> {
    let store = app
        .try_state::<crate::DesktopState>()
        .map(|desktop| desktop.store.clone())?;
    let workspace_path = workspace_path.to_string();
    let computed = tauri::async_runtime::spawn_blocking(move || {
        crate::prime_guidance::thread_guidance(&store, &crate::prime::data_dir(), &workspace_path)
    })
    .await;
    match computed {
        Ok(Ok(guidance)) => {
            tracing::info!(
                injected = guidance.injected.len(),
                left_out = guidance.left_out.len(),
                "prime lessons injected"
            );
            Some(guidance)
        }
        Ok(Err(error)) => {
            tracing::warn!(error = %error, "prime lessons not injected");
            None
        }
        Err(error) => {
            tracing::warn!(error = %error, "prime lessons not injected");
            None
        }
    }
}

/// Importe en tâche de fond une refine reçue en direct dans le relais.
fn import_live_refinement(
    app: &AppHandle,
    state: &PrimeState,
    active_session_id: &str,
    details: &Value,
) {
    let Some(thread) = state.thread(active_session_id) else {
        return;
    };
    // La file importe elle-même la refine qu'elle a lancée, avec son
    // déclencheur, puis rattrape les autres.
    if crate::prime_refine::refine_in_flight(active_session_id) {
        return;
    }
    let Some(store) = app
        .try_state::<crate::DesktopState>()
        .map(|desktop| desktop.store.clone())
    else {
        return;
    };
    let details = details.clone();
    tauri::async_runtime::spawn_blocking(move || {
        match import_refinement_outcome(&store, &crate::prime::agent_dir(), &thread, &details) {
            Ok(Some(report)) => tracing::info!(?report, "prime refine imported"),
            Ok(None) => {}
            Err(error) => tracing::warn!(error = %error, "prime refine import failed"),
        }
    });
}

/// Après chaque cellule, en tâche de fond : les écritures directes du modèle
/// dans le harness global de Prime (`rlm.harness.*(…, global_=True)`)
/// deviennent des leçons de cette conversation, proposées pour le global.
/// Deux cellules finies au même moment dans deux conversations : la
/// première arrivée prend les entrées (limite notée dans CONTEXT.md).
fn guard_global_harness(app: &AppHandle, state: &PrimeState, active_session_id: &str) {
    let Some(thread) = state.thread(active_session_id) else {
        return;
    };
    let Some(store) = app
        .try_state::<crate::DesktopState>()
        .map(|desktop| desktop.store.clone())
    else {
        return;
    };
    tauri::async_runtime::spawn_blocking(move || {
        match import_global_harness_writes(&store, &crate::prime::agent_dir(), &thread) {
            Ok(Some(report)) => tracing::info!(?report, "prime global harness writes imported"),
            Ok(None) => {}
            Err(error) => tracing::warn!(error = %error, "prime global harness guard failed"),
        }
    });
}

/// Rattrapage en tâche de fond des refines de l'historique d'un fil.
fn catch_up_refinements(app: &AppHandle, thread: ThreadContext, messages: Vec<Value>) {
    let Some(store) = app
        .try_state::<crate::DesktopState>()
        .map(|desktop| desktop.store.clone())
    else {
        return;
    };
    tauri::async_runtime::spawn_blocking(move || {
        let reports =
            import_thread_outcomes(&store, &crate::prime::agent_dir(), &thread, &messages);
        if !reports.is_empty() {
            tracing::info!(?reports, "prime refines caught up from the thread");
        }
    });
}

/// Le client courant, ou une nouvelle connexion (daemon lancé si besoin).
async fn connected_client(app: &AppHandle, state: &PrimeState) -> Result<DaemonClient> {
    let mut slot = state.client.lock().await;
    if let Some(client) = slot.as_ref() {
        if !*client.reader_dead().borrow() {
            return Ok(client.clone());
        }
    }
    let socket_path = crate::prime::daemon_socket_path();
    let (client, events) = crate::prime::ensure_daemon_running(&socket_path).await?;
    spawn_event_relay(app.clone(), events, client.reader_dead());
    *slot = Some(client.clone());
    // Chaque nouvelle connexion tue les sessions d'IDE disparus, avant toute
    // ouverture : un orphelin tient le verrou de son fichier de session, et
    // le `Create` qui le rouvre serait refusé ou rattaché à lui.
    match reap_orphaned_sessions(&client, &crate::prime::agent_dir(), &socket_path).await {
        Ok(reaped) if !reaped.is_empty() => {
            tracing::info!(count = reaped.len(), "reaped orphaned prime sessions");
        }
        Ok(_) => {}
        Err(error) => tracing::warn!(error = %error, "prime orphan cleanup failed"),
    }
    Ok(client)
}

fn error_text(error: anyhow::Error) -> String {
    format!("{error:#}")
}

#[tauri::command]
pub async fn prime_create_session(
    app: AppHandle,
    state: State<'_, PrimeState>,
    workspace_path: String,
    conversation_id: String,
) -> Result<OpenedSession, String> {
    if !Path::new(&workspace_path).is_dir() {
        return Err(format!("workspace not found: {workspace_path}"));
    }
    let session_path =
        thread_path(&crate::prime::agent_dir(), &conversation_id).map_err(error_text)?;
    let client = connected_client(&app, &state).await.map_err(error_text)?;
    // La connexion Anthropic de yusAi, recopiée avant que le worker ne
    // résolve son modèle.
    crate::prime_auth::ensure_anthropic_sync(&crate::prime::agent_dir()).await;
    let config = match thread_guidance(&app, &workspace_path).await {
        Some(guidance) => with_guidance(create_config(&workspace_path), &guidance),
        None => create_config(&workspace_path),
    };
    let opened = open_thread(&client, config, &session_path)
        .await
        .map_err(error_text)?;
    let active_session_id = opened.active_session_id.clone();
    let thread = ThreadContext {
        conversation_id: conversation_id.clone(),
        workspace_id: workspace_path.clone(),
    };
    state.track(&active_session_id, thread.clone());
    catch_up_refinements(&app, thread, opened.messages.clone());
    // La racine des photos est le dossier de travail du worker, relu auprès
    // de lui, pas le répertoire courant de l'IDE.
    match session_cwd(&client, &active_session_id).await {
        Ok(root) => {
            let sink_app = app.clone();
            let sink_session = active_session_id.clone();
            state.with_diffs(|diffs| {
                diffs.track(
                    &active_session_id,
                    root,
                    Arc::new(move |tool_call_id, file_changes| {
                        let _ = sink_app.emit(
                            PRIME_EVENT_NAME,
                            PrimeEventPayload::ToolFileChanges {
                                active_session_id: sink_session.clone(),
                                tool_call_id,
                                file_changes,
                            },
                        );
                    }),
                );
            });
        }
        Err(error) => tracing::warn!(error = %error, "prime tool diffs disabled for this session"),
    }
    Ok(opened)
}

/// Bouton « Retenir » : une refine locale de la conversation, tout de suite
/// (à son tour dans la file des refines), avec les `instructions` facultatives
/// de l'utilisateur. Renvoie ce que l'import a fait dans le magasin.
#[tauri::command]
pub async fn prime_retain(
    app: AppHandle,
    state: State<'_, PrimeState>,
    active_session_id: String,
    instructions: Option<String>,
) -> Result<ImportReport, String> {
    let thread = state
        .thread(&active_session_id)
        .ok_or_else(|| format!("unknown prime session: {active_session_id}"))?;
    let store = app
        .try_state::<crate::DesktopState>()
        .map(|desktop| desktop.store.clone())
        .ok_or_else(|| "app store unavailable".to_string())?;
    // Le daemon tourne (le relancer au besoin) avant la connexion de la file.
    connected_client(&app, &state).await.map_err(error_text)?;
    let instructions = instructions
        .map(|text| text.trim().to_string())
        .filter(|text| !text.is_empty());
    let run = crate::prime_refine::run_refine(
        &crate::prime::daemon_socket_path(),
        store,
        crate::prime::agent_dir(),
        &active_session_id,
        thread,
        crate::prime_refine::RefineOrigin::Retain,
        instructions,
    )
    .await
    .map_err(error_text)?;
    Ok(run.report.unwrap_or_else(|| ImportReport {
        refinement_id: run.refinement_id,
        ..ImportReport::default()
    }))
}

#[tauri::command]
pub async fn prime_prompt(
    app: AppHandle,
    state: State<'_, PrimeState>,
    active_session_id: String,
    message: String,
) -> Result<(), String> {
    let client = connected_client(&app, &state).await.map_err(error_text)?;
    prompt(&client, &active_session_id, &message)
        .await
        .map_err(error_text)
}

#[tauri::command]
pub async fn prime_abort(
    app: AppHandle,
    state: State<'_, PrimeState>,
    active_session_id: String,
) -> Result<(), String> {
    let client = connected_client(&app, &state).await.map_err(error_text)?;
    abort(&client, &active_session_id).await.map_err(error_text)
}

#[tauri::command]
pub async fn prime_rlm_children(
    app: AppHandle,
    state: State<'_, PrimeState>,
    active_session_id: String,
) -> Result<Vec<PrimeSubAgent>, String> {
    let client = connected_client(&app, &state).await.map_err(error_text)?;
    rlm_children(&client, &active_session_id)
        .await
        .map_err(error_text)
}

#[tauri::command]
pub async fn prime_session_config(
    app: AppHandle,
    state: State<'_, PrimeState>,
    active_session_id: String,
) -> Result<PrimeSessionConfig, String> {
    let client = connected_client(&app, &state).await.map_err(error_text)?;
    session_config(&client, &active_session_id)
        .await
        .map_err(error_text)
}

/// Renvoie la config relue : le niveau peut être ajusté au nouveau modèle.
#[tauri::command]
pub async fn prime_set_model(
    app: AppHandle,
    state: State<'_, PrimeState>,
    active_session_id: String,
    provider: String,
    model_id: String,
) -> Result<PrimeSessionConfig, String> {
    let client = connected_client(&app, &state).await.map_err(error_text)?;
    set_model(&client, &active_session_id, &provider, &model_id)
        .await
        .map_err(error_text)?;
    session_config(&client, &active_session_id)
        .await
        .map_err(error_text)
}

#[tauri::command]
pub async fn prime_set_thinking_level(
    app: AppHandle,
    state: State<'_, PrimeState>,
    active_session_id: String,
    level: String,
) -> Result<PrimeSessionConfig, String> {
    let client = connected_client(&app, &state).await.map_err(error_text)?;
    set_thinking_level(&client, &active_session_id, &level)
        .await
        .map_err(error_text)?;
    session_config(&client, &active_session_id)
        .await
        .map_err(error_text)
}

/// Supprime le fil Prime d'une conversation yusAi supprimée. Sans fichier,
/// rien à faire : le daemon n'est pas lancé pour ça.
pub async fn delete_conversation_thread(app: &AppHandle, conversation_id: &str) -> Result<()> {
    let session_path = thread_path(&crate::prime::agent_dir(), conversation_id)?;
    if !session_path.exists() {
        return Ok(());
    }
    let state = app.state::<PrimeState>();
    let client = connected_client(app, &state).await?;
    for active_session_id in delete_thread(&client, &session_path).await? {
        state.untrack(&active_session_id);
    }
    Ok(())
}

/// Ferme une session : sans client connecté, il n'y a rien à fermer (un
/// daemon perdu a emporté ses sessions).
#[tauri::command]
pub async fn prime_close_session(
    state: State<'_, PrimeState>,
    active_session_id: String,
) -> Result<(), String> {
    state.untrack(&active_session_id);
    let client = state.client.lock().await.clone();
    match client {
        Some(client) if !*client.reader_dead().borrow() => {
            kill_session(&client, &active_session_id)
                .await
                .map_err(error_text)
        }
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_file_changes_payload_is_camel_case() {
        let payload = PrimeEventPayload::ToolFileChanges {
            active_session_id: "s1".into(),
            tool_call_id: "call-1".into(),
            file_changes: Vec::new(),
        };
        assert_eq!(
            serde_json::to_value(payload).unwrap(),
            json!({
                "kind": "toolFileChanges",
                "activeSessionId": "s1",
                "toolCallId": "call-1",
                "fileChanges": [],
            })
        );
    }

    #[test]
    fn event_payload_is_camel_case_for_the_webview() {
        let payload = PrimeEventPayload::SessionEvent {
            active_session_id: "s1".into(),
            event: json!({ "type": "agent_end" }),
        };
        assert_eq!(
            serde_json::to_value(payload).unwrap(),
            json!({
                "kind": "sessionEvent",
                "activeSessionId": "s1",
                "event": { "type": "agent_end" },
            })
        );
    }
}
