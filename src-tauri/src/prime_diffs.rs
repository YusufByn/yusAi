//! Fichiers modifiés par les appels d'outils de Prime.
//!
//! Le noyau Prime capture les diffs de `edit` mais les perd avant le
//! résultat de l'outil (`convert_execute_result` ne recopie pas `diffs`,
//! pa-core/src/session_engine/runtime_wiring.rs:342-367). On photographie
//! donc le dossier de travail de la session, comme l'outil bash de Sinew
//! (crates/sinew-app/src/bash.rs:202, 448-449) :
//! - à chaque `agent_start` (début de tour), une photo de référence, pour
//!   que les modifications faites à la main entre deux tours ne soient pas
//!   attribuées à Prime ;
//! - à chaque `tool_execution_end`, une nouvelle photo comparée à la
//!   référence, qu'elle remplace ; les fichiers modifiés partent vers le
//!   front, rattachés au `toolCallId`.
//!
//! Les photos tournent dans une tâche par session, dans l'ordre des
//! événements : le relais n'attend jamais le parcours du projet.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use serde_json::Value;
use sinew_app::tool_run::{diff_snapshots, snapshot_workspace, FileChange};
use tokio::sync::{mpsc, oneshot};

/// Reçoit les fichiers modifiés par un appel : `(toolCallId, changements)`.
pub type ToolChangesSink = Arc<dyn Fn(String, Vec<FileChange>) + Send + Sync>;

enum DiffJob {
    Baseline,
    ToolEnded(String),
    Flush(oneshot::Sender<()>),
}

/// Les sessions suivies, par `activeSessionId`.
#[derive(Default)]
pub struct PrimeDiffs {
    sessions: HashMap<String, mpsc::UnboundedSender<DiffJob>>,
}

impl PrimeDiffs {
    /// Suit une session dont le dossier de travail est `root`. À appeler
    /// dans un runtime tokio.
    pub fn track(&mut self, active_session_id: &str, root: PathBuf, sink: ToolChangesSink) {
        let (tx, rx) = mpsc::unbounded_channel();
        tokio::spawn(run_session(root, rx, sink));
        self.sessions.insert(active_session_id.to_string(), tx);
    }

    pub fn forget(&mut self, active_session_id: &str) {
        self.sessions.remove(active_session_id);
    }

    pub fn clear(&mut self) {
        self.sessions.clear();
    }

    /// Un événement de session, déjà relayé au front.
    pub fn observe(&self, active_session_id: &str, event: &Value) {
        let Some(tx) = self.sessions.get(active_session_id) else {
            return;
        };
        let job = match event.get("type").and_then(Value::as_str) {
            Some("agent_start") => DiffJob::Baseline,
            Some("tool_execution_end") => match event.get("toolCallId").and_then(Value::as_str) {
                Some(tool_call_id) => DiffJob::ToolEnded(tool_call_id.to_string()),
                None => return,
            },
            _ => return,
        };
        let _ = tx.send(job);
    }

    /// Attend la fin des photos déjà demandées pour la session.
    pub async fn flush(&self, active_session_id: &str) {
        let Some(tx) = self.sessions.get(active_session_id) else {
            return;
        };
        let (done_tx, done_rx) = oneshot::channel();
        if tx.send(DiffJob::Flush(done_tx)).is_ok() {
            let _ = done_rx.await;
        }
    }
}

async fn run_session(
    root: PathBuf,
    mut jobs: mpsc::UnboundedReceiver<DiffJob>,
    sink: ToolChangesSink,
) {
    let root = Arc::new(root);
    let mut baseline = None;
    while let Some(job) = jobs.recv().await {
        match job {
            DiffJob::Baseline => baseline = snapshot(&root).await,
            DiffJob::ToolEnded(tool_call_id) => {
                let Some(after) = snapshot(&root).await else {
                    continue;
                };
                // Sans référence (suivi commencé en plein tour), cette photo
                // en devient une et rien n'est attribué.
                if let Some(before) = baseline.replace(after.clone()) {
                    let changes = diff_snapshots(before, after);
                    if !changes.is_empty() {
                        sink(tool_call_id, changes);
                    }
                }
            }
            DiffJob::Flush(done) => {
                let _ = done.send(());
            }
        }
    }
}

async fn snapshot(root: &Arc<PathBuf>) -> Option<sinew_app::tool_run::WorkspaceSnapshot> {
    let root = Arc::clone(root);
    match tokio::task::spawn_blocking(move || snapshot_workspace(&root)).await {
        Ok(snapshot) => Some(snapshot),
        Err(error) => {
            tracing::warn!(error = %error, "prime workspace snapshot failed");
            None
        }
    }
}
