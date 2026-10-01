//! Sessions Prime vues du chat : commandes Tauri (créer, envoyer un prompt,
//! annuler, fermer) sur le protocole natif du daemon, et relais des
//! `DaemonClientEvent` vers le front (événement Tauri `prime-event`).
//!
//! Le daemon n'est lancé qu'à la première création de session.

use std::path::Path;

use anyhow::{anyhow, Result};
use pa_tui::daemon_client::{DaemonClient, DaemonClientEvent};
use pa_types::daemon::{DaemonCommand, PromptInput, StreamingBehavior};
use serde::Serialize;
use serde_json::{json, Map, Value};
use tauri::{AppHandle, Emitter, State};
use tokio::sync::{mpsc::UnboundedReceiver, watch, Mutex};

pub const PRIME_EVENT_NAME: &str = "prime-event";

/// Client du daemon partagé par toutes les fenêtres, connecté à la demande.
#[derive(Default)]
pub struct PrimeState {
    client: Mutex<Option<DaemonClient>>,
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

/// Crée une session sans fichier persistant (comme l'ACP daemon-attached,
/// pa-daemon/src/acp/daemon.rs:545-560) et s'y attache pour recevoir ses
/// événements (daemon.rs:584-595). Renvoie l'`activeSessionId`.
pub async fn create_session(client: &DaemonClient, config: Value) -> Result<String> {
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
            runtime_metadata: None,
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
                    if let Some(payload) = PrimeEventPayload::from_client_event(event) {
                        let _ = app.emit(PRIME_EVENT_NAME, payload);
                    }
                }
                changed = reader_dead.changed() => {
                    if changed.is_err() || *reader_dead.borrow() {
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
    let (client, events) =
        crate::prime::ensure_daemon_running(&crate::prime::daemon_socket_path()).await?;
    spawn_event_relay(app.clone(), events, client.reader_dead());
    *slot = Some(client.clone());
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
    create_session(&client, create_config(&workspace_path))
        .await
        .map_err(error_text)
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

/// Ferme une session : sans client connecté, il n'y a rien à fermer (un
/// daemon perdu a emporté ses sessions).
#[tauri::command]
pub async fn prime_close_session(
    state: State<'_, PrimeState>,
    active_session_id: String,
) -> Result<(), String> {
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
