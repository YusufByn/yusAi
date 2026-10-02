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

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;

use anyhow::{anyhow, Result};
use pa_tui::daemon_client::{DaemonClient, DaemonClientEvent};
use pa_types::daemon::{DaemonCommand, PromptInput, StreamingBehavior};
use serde::Serialize;
use serde_json::{json, Map, Value};
use sinew_app::tool_run::FileChange;
use tauri::{AppHandle, Emitter, Manager, State};

use crate::prime_diffs::PrimeDiffs;
use tokio::sync::{mpsc::UnboundedReceiver, watch, Mutex};

pub const PRIME_EVENT_NAME: &str = "prime-event";

/// Client du daemon partagé par toutes les fenêtres, connecté à la demande,
/// sessions créées par ce processus (tuées à la sortie) et fichiers
/// modifiés par leurs appels d'outils.
#[derive(Default)]
pub struct PrimeState {
    client: Mutex<Option<DaemonClient>>,
    sessions: StdMutex<HashSet<String>>,
    diffs: StdMutex<PrimeDiffs>,
}

impl PrimeState {
    fn track(&self, active_session_id: &str) {
        if let Ok(mut sessions) = self.sessions.lock() {
            sessions.insert(active_session_id.to_string());
        }
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

/// Crée une session sans fichier persistant (comme l'ACP daemon-attached,
/// pa-daemon/src/acp/daemon.rs:545-560) et s'y attache pour recevoir ses
/// événements (daemon.rs:584-595). Renvoie l'`activeSessionId`.
pub async fn create_session_with_metadata(
    client: &DaemonClient,
    config: Value,
    runtime_metadata: Value,
) -> Result<String> {
    let summary = client
        .request_ok(DaemonCommand::Create {
            id: None,
            session_path: None,
            continue_recent: None,
            no_session: Some(true),
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
        })
        .await?;
    let active_session_id = summary
        .get("activeSessionId")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("create response carries no activeSessionId: {summary}"))?
        .to_string();
    let attached = client
        .request_ok(DaemonCommand::Attach {
            id: None,
            active_session_id: active_session_id.clone(),
            client_id: None,
            capabilities: None,
            resume_cursor: None,
            telemetry_disabled: None,
            recovery_config: None,
            env: None,
            launch_env: None,
            rest: Map::default(),
        })
        .await;
    if let Err(error) = attached {
        let _ = kill_session(client, &active_session_id).await;
        return Err(error);
    }
    Ok(active_session_id)
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
            .map(|mut sessions| sessions.drain().collect())
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
    // Chaque nouvelle connexion tue les sessions d'IDE disparus.
    let reaper = client.clone();
    tauri::async_runtime::spawn(async move {
        match reap_orphaned_sessions(&reaper, &crate::prime::agent_dir(), &socket_path).await {
            Ok(reaped) if !reaped.is_empty() => {
                tracing::info!(count = reaped.len(), "reaped orphaned prime sessions");
            }
            Ok(_) => {}
            Err(error) => tracing::warn!(error = %error, "prime orphan cleanup failed"),
        }
    });
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
) -> Result<String, String> {
    if !Path::new(&workspace_path).is_dir() {
        return Err(format!("workspace not found: {workspace_path}"));
    }
    let client = connected_client(&app, &state).await.map_err(error_text)?;
    // La connexion Anthropic de yusAi, recopiée avant que le worker ne
    // résolve son modèle.
    crate::prime_auth::ensure_anthropic_sync(&crate::prime::agent_dir()).await;
    let active_session_id = create_session(&client, create_config(&workspace_path))
        .await
        .map_err(error_text)?;
    state.track(&active_session_id);
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
    Ok(active_session_id)
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
