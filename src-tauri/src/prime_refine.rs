//! File des refines de yusAi : une refine à la fois dans toute l'app, avec
//! nos leçons amorcées dans le harness local partagé
//! (`<agent_dir>/yusai-threads/harness/`) le temps de la refine.
//!
//! Déroulé de [`run_refine`] :
//! 1. prendre sa place dans la file ;
//! 2. amorcer les leçons qui s'appliquent à la conversation (projet, type
//!    du projet, global), pour que le planificateur de Prime puisse les
//!    modifier ou les supprimer ;
//! 3. envoyer `Refine` au worker, sur une connexion à part ;
//! 4. importer la refine dans le magasin (une seule fois par
//!    `refinementId` : le relais a pu le faire avant nous) ;
//! 5. retirer nos entrées `yl_…` du harness, même si la refine a échoué ;
//! 6. noter la refine faite (`mark_refined`) si elle a réussi. Un échec
//!    ne touche pas l'état de refine de la conversation : c'est à
//!    l'appelant de décider (par exemple `pending = 1`).
//!
//! Le superviseur coupe une requête `Refine` au bout de 30 s
//! (pa-daemon/src/supervisor/routing.rs:648-661), alors que le worker
//! poursuit la refine. La connexion de la refine passe donc directement au
//! worker (`upgrade_direct`, pa-tui/src/daemon_client.rs:872, comme le TUI
//! de Prime) : la réponse arrive quand la refine finit, réussie ou non, et
//! la mort du worker coupe le lien, donc la requête, tout de suite. Si le
//! lien direct est refusé, la requête passe par le superviseur ; après ses
//! 30 s, on relit les messages du fil jusqu'à y trouver la nouvelle ligne
//! `refinement_outcome`, dans la limite de 10 min. Sur ce chemin de repli,
//! une session fermée libère la file au relevé suivant, mais une refine
//! qui échoue ne laisse aucune trace (pa-daemon/src/session_custom.rs:298-306)
//! et la file attend la limite.
//!
//! Hors de cette file : l'auto-refine de Prime et `refine.run()` appelé par
//! le modèle (limite notée dans CONTEXT.md).

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use pa_tui::daemon_client::{DaemonClient, LONG_RUNNING_REQUEST_TIMEOUT_MS};
use pa_types::daemon::DaemonCommand;
use serde_json::Value;
use sinew_app::store::AppStore;

use crate::prime_lessons::{
    import_refinement_outcome, refinement_outcome_details, seed_thread_lessons,
    unseed_thread_lessons, ImportReport, ThreadContext,
};

/// Ce qu'une refine de la file a donné.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefineRun {
    pub refinement_id: String,
    /// Leçons amorcées dans le harness pour cette refine.
    pub seeded: usize,
    /// `None` si le relais l'avait déjà importée.
    pub report: Option<ImportReport>,
}

static REFINE_QUEUE: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// Délai du superviseur dépassé : la réponse ne viendra plus, la refine
/// continue dans le worker (pa-daemon/src/supervisor/routing.rs:193).
const ROUTE_TIMED_OUT: &str = "Session worker timed out";
/// La limite de la route longue du superviseur
/// (pa-daemon/src/supervisor/routing.rs:39).
const OUTCOME_DEADLINE: Duration = Duration::from_millis(LONG_RUNNING_REQUEST_TIMEOUT_MS);
const OUTCOME_POLL: Duration = Duration::from_secs(2);

/// Lance une refine locale de la conversation, à son tour dans la file.
pub async fn run_refine(
    socket_path: &Path,
    store: AppStore,
    agent_dir: PathBuf,
    active_session_id: &str,
    thread: ThreadContext,
    instructions: Option<String>,
) -> Result<RefineRun> {
    let _turn = REFINE_QUEUE.lock().await;

    let seeded = {
        let (store, agent_dir, thread) = (store.clone(), agent_dir.clone(), thread.clone());
        blocking(move || seed_thread_lessons(&store, &agent_dir, &thread)).await
    }
    .unwrap_or_else(|error| {
        // Sans amorçage, la refine reste utile : elle ne peut juste pas
        // retoucher nos leçons (le magasin écarte ses doublons).
        tracing::warn!(error = %error, "prime lessons not seeded for refine");
        0
    });

    let refined = refine_and_import(
        socket_path,
        &store,
        &agent_dir,
        active_session_id,
        &thread,
        instructions,
    )
    .await;

    let unseed_dir = agent_dir.clone();
    if let Err(error) = blocking(move || unseed_thread_lessons(&unseed_dir)).await {
        tracing::warn!(error = %error, "seeded prime lessons stay in the harness");
    }

    let (refinement_id, report) = refined?;
    let conversation_id = thread.conversation_id.clone();
    blocking(move || store.mark_refined(&conversation_id)).await?;
    Ok(RefineRun {
        refinement_id,
        seeded,
        report,
    })
}

async fn refine_and_import(
    socket_path: &Path,
    store: &AppStore,
    agent_dir: &Path,
    active_session_id: &str,
    thread: &ThreadContext,
    instructions: Option<String>,
) -> Result<(String, Option<ImportReport>)> {
    // Une connexion à part : le lien direct ne sert qu'à cette session, et
    // le client de l'app garde son relais d'événements intact.
    let (client, _events) = DaemonClient::connect(socket_path).await?;
    let refined = refine_on(
        &client,
        store,
        agent_dir,
        active_session_id,
        thread,
        instructions,
    )
    .await;
    client.close();
    refined
}

async fn refine_on(
    client: &DaemonClient,
    store: &AppStore,
    agent_dir: &Path,
    active_session_id: &str,
    thread: &ThreadContext,
    instructions: Option<String>,
) -> Result<(String, Option<ImportReport>)> {
    let direct = client
        .upgrade_direct(active_session_id)
        .await
        .unwrap_or(false);
    if !direct {
        tracing::info!("prime refine goes through the supervisor route");
    }
    // Les refines déjà dans le fil, pour reconnaître la nouvelle si la
    // réponse se perd.
    let before = outcome_ids(client, active_session_id)
        .await
        .unwrap_or_default();
    let response = client
        .request_with_timeout(
            DaemonCommand::Refine {
                id: None,
                active_session_id: active_session_id.to_string(),
                instructions,
                rollback_id: None,
                global: None,
                rest: serde_json::Map::default(),
            },
            LONG_RUNNING_REQUEST_TIMEOUT_MS,
        )
        .await?;
    let details = if response.success {
        outcome_details(response.data.unwrap_or_default())?
    } else {
        let error = response.error.unwrap_or_default();
        if error != ROUTE_TIMED_OUT {
            bail!("prime refine failed: {error}");
        }
        tracing::info!("prime refine outlived its route, waiting for its outcome");
        wait_for_outcome(client, active_session_id, &before).await?
    };
    let refinement_id = details
        .get("refinementId")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("prime refine outcome without refinementId"))?
        .to_string();

    let (store, agent_dir, thread) = (store.clone(), agent_dir.to_path_buf(), thread.clone());
    let report =
        blocking(move || import_refinement_outcome(&store, &agent_dir, &thread, &details)).await?;
    Ok((refinement_id, report))
}

/// Les `details` de la ligne `refinement_outcome`, tels que le worker les
/// émet à partir du `RefinementResult` de la réponse
/// (pa-daemon/src/session_custom.rs:310-318,
/// pa-core/src/session_engine/refine.rs:125-145).
fn outcome_details(result: Value) -> Result<Value> {
    let result: pa_core::refinement::RefinementResult =
        serde_json::from_value(result).context("parse prime refine result")?;
    pa_core::session_engine::refine::create_refinement_outcome_message(&result)
        .details
        .ok_or_else(|| anyhow!("prime refine outcome without details"))
}

async fn outcome_ids(client: &DaemonClient, active_session_id: &str) -> Result<HashSet<String>> {
    Ok(thread_outcomes(client, active_session_id)
        .await?
        .into_iter()
        .filter_map(|details| details["refinementId"].as_str().map(str::to_string))
        .collect())
}

async fn thread_outcomes(client: &DaemonClient, active_session_id: &str) -> Result<Vec<Value>> {
    let data = client
        .request_ok(DaemonCommand::GetMessages {
            id: None,
            active_session_id: active_session_id.to_string(),
            rest: serde_json::Map::default(),
        })
        .await?;
    Ok(data["messages"]
        .as_array()
        .map(|messages| {
            messages
                .iter()
                .filter_map(refinement_outcome_details)
                .cloned()
                .collect()
        })
        .unwrap_or_default())
}

/// Attend dans le fil une ligne `refinement_outcome` absente de `before`.
async fn wait_for_outcome(
    client: &DaemonClient,
    active_session_id: &str,
    before: &HashSet<String>,
) -> Result<Value> {
    let deadline = tokio::time::Instant::now() + OUTCOME_DEADLINE;
    while tokio::time::Instant::now() < deadline {
        tokio::time::sleep(OUTCOME_POLL).await;
        let outcomes = thread_outcomes(client, active_session_id).await?;
        if let Some(details) = outcomes.into_iter().find(|details| {
            details["refinementId"]
                .as_str()
                .is_some_and(|id| !before.contains(id))
        }) {
            return Ok(details);
        }
    }
    bail!("prime refine outcome never reached the thread")
}

async fn blocking<T: Send + 'static>(
    work: impl FnOnce() -> Result<T> + Send + 'static,
) -> Result<T> {
    tokio::task::spawn_blocking(work)
        .await
        .map_err(|error| anyhow!("blocking task failed: {error}"))?
}
