//! Fermeture des conversations Prime (décisions dans CONTEXT.md) :
//! - fermeture courte : conversation affichée dans aucune fenêtre depuis
//!   10 min, au moins 3 nouveaux tours utilisateur depuis la dernière
//!   refine → refine locale, le worker reste ;
//! - mise en veille, notre propre minuteur (celle de Prime ne touche pas
//!   nos fils attachés) : conversation non affichée et sans activité depuis
//!   90 min → refine s'il y a au moins 1 nouveau tour, puis `Kill` du
//!   worker, même si la refine échoue ; le fil se rouvre depuis son fichier
//!   au prochain affichage ;
//! - « sans activité » : aucun tour en cours et aucun sous-agent vivant ;
//! - une refine de fermeture qui échoue laisse `pending = 1` (reprise au
//!   prochain démarrage) et la fermeture courte ne réessaie pas avant un
//!   nouveau tour.
//!
//! [`CloseTracker`] suit l'affichage (par conversation et par fenêtre) et
//! l'activité (par session) ; [`close_action`] décide, sans effet ;
//! [`close_conversation`] agit.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant};

use pa_tui::daemon_client::DaemonClient;
use sinew_app::store::AppStore;

use crate::prime_lessons::ThreadContext;
use crate::prime_refine::{run_refine, RefineOrigin, RefineRun};

/// Fermeture courte : non affichée depuis…
pub const CLOSE_HIDDEN: Duration = Duration::from_secs(10 * 60);
/// … avec au moins tant de nouveaux tours utilisateur.
pub const CLOSE_TURNS: i64 = 3;
/// Mise en veille : non affichée et sans activité depuis…
pub const IDLE_AFTER: Duration = Duration::from_secs(90 * 60);
/// Fréquence du relevé.
pub const SCAN_EVERY: Duration = Duration::from_secs(60);

/// Ce que la fermeture fait d'une session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CloseAction {
    /// Fermeture courte : refine, le worker reste.
    Refine,
    /// Mise en veille : refine s'il y a un nouveau tour, puis `Kill`.
    Sleep,
}

/// L'état d'une session au moment du relevé.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ActivitySnapshot {
    /// Depuis quand la conversation n'est affichée nulle part ; `None` si
    /// elle l'est.
    pub hidden_since: Option<Instant>,
    /// Fin du dernier tour, ou dernier prompt, ou ouverture.
    pub last_activity: Instant,
    pub turn_running: bool,
    /// Une refine de fermeture a échoué depuis le dernier prompt.
    pub close_refine_failed: bool,
}

/// La décision du minuteur, sans effet. `subagents_running` vient de
/// `get_rlm_children` (demandé seulement quand une action se profile).
pub fn close_action(
    now: Instant,
    activity: &ActivitySnapshot,
    user_turns: i64,
    subagents_running: bool,
) -> Option<CloseAction> {
    let hidden_since = activity.hidden_since?;
    if activity.turn_running || subagents_running {
        return None;
    }
    let hidden_for = now.saturating_duration_since(hidden_since);
    let idle_since = hidden_since.max(activity.last_activity);
    if hidden_for >= IDLE_AFTER && now.saturating_duration_since(idle_since) >= IDLE_AFTER {
        return Some(CloseAction::Sleep);
    }
    if hidden_for >= CLOSE_HIDDEN && user_turns >= CLOSE_TURNS && !activity.close_refine_failed {
        return Some(CloseAction::Refine);
    }
    None
}

#[derive(Debug, Clone)]
struct SessionActivity {
    conversation_id: String,
    last_activity: Instant,
    turn_running: bool,
    close_refine_failed: bool,
    closing: bool,
}

#[derive(Debug, Default)]
struct Inner {
    /// Fenêtres qui affichent chaque conversation.
    displays: HashMap<String, HashSet<String>>,
    /// Depuis quand chaque conversation n'est plus affichée.
    hidden_since: HashMap<String, Instant>,
    sessions: HashMap<String, SessionActivity>,
}

/// Affichage et activité des conversations Prime ouvertes.
#[derive(Debug, Default)]
pub struct CloseTracker {
    inner: Mutex<Inner>,
}

impl CloseTracker {
    fn with<T>(&self, f: impl FnOnce(&mut Inner) -> T) -> T {
        f(&mut self.inner.lock().unwrap_or_else(PoisonError::into_inner))
    }

    /// Une fenêtre affiche (ou n'affiche plus) une conversation.
    pub fn set_displayed(
        &self,
        conversation_id: &str,
        window: &str,
        displayed: bool,
        now: Instant,
    ) {
        self.with(|inner| {
            let windows = inner
                .displays
                .entry(conversation_id.to_string())
                .or_default();
            if displayed {
                windows.insert(window.to_string());
                inner.hidden_since.remove(conversation_id);
            } else if windows.remove(window) && windows.is_empty() {
                inner.hidden_since.insert(conversation_id.to_string(), now);
            }
        });
    }

    /// Une fenêtre fermée n'affiche plus rien.
    pub fn forget_window(&self, window: &str, now: Instant) {
        self.with(|inner| {
            for (conversation_id, windows) in &mut inner.displays {
                if windows.remove(window) && windows.is_empty() {
                    inner.hidden_since.insert(conversation_id.clone(), now);
                }
            }
        });
    }

    pub fn session_opened(&self, active_session_id: &str, conversation_id: &str, now: Instant) {
        self.with(|inner| {
            let displayed = inner
                .displays
                .get(conversation_id)
                .is_some_and(|windows| !windows.is_empty());
            if !displayed {
                inner
                    .hidden_since
                    .entry(conversation_id.to_string())
                    .or_insert(now);
            }
            inner.sessions.insert(
                active_session_id.to_string(),
                SessionActivity {
                    conversation_id: conversation_id.to_string(),
                    last_activity: now,
                    turn_running: false,
                    close_refine_failed: false,
                    closing: false,
                },
            );
        });
    }

    pub fn session_closed(&self, active_session_id: &str) {
        self.with(|inner| inner.sessions.remove(active_session_id));
    }

    /// Un prompt de l'utilisateur : activité, et la fermeture courte peut
    /// réessayer.
    pub fn user_prompted(&self, active_session_id: &str, now: Instant) {
        self.update(active_session_id, |session| {
            session.last_activity = now;
            session.close_refine_failed = false;
        });
    }

    pub fn turn_started(&self, active_session_id: &str) {
        self.update(active_session_id, |session| session.turn_running = true);
    }

    pub fn turn_ended(&self, active_session_id: &str, now: Instant) {
        self.update(active_session_id, |session| {
            session.turn_running = false;
            session.last_activity = now;
        });
    }

    fn update(&self, active_session_id: &str, f: impl FnOnce(&mut SessionActivity)) {
        self.with(|inner| {
            if let Some(session) = inner.sessions.get_mut(active_session_id) {
                f(session);
            }
        });
    }

    /// Les sessions suivies et leur conversation, hors fermeture en cours.
    pub fn sessions(&self) -> Vec<(String, String)> {
        self.with(|inner| {
            inner
                .sessions
                .iter()
                .filter(|(_, session)| !session.closing)
                .map(|(id, session)| (id.clone(), session.conversation_id.clone()))
                .collect()
        })
    }

    pub fn snapshot(&self, active_session_id: &str) -> Option<ActivitySnapshot> {
        self.with(|inner| {
            let session = inner.sessions.get(active_session_id)?;
            let displayed = inner
                .displays
                .get(&session.conversation_id)
                .is_some_and(|windows| !windows.is_empty());
            Some(ActivitySnapshot {
                hidden_since: if displayed {
                    None
                } else {
                    inner.hidden_since.get(&session.conversation_id).copied()
                },
                last_activity: session.last_activity,
                turn_running: session.turn_running,
                close_refine_failed: session.close_refine_failed,
            })
        })
    }

    /// Réserve la session pour une fermeture ; faux si elle l'est déjà.
    pub fn begin_close(&self, active_session_id: &str) -> bool {
        self.with(|inner| match inner.sessions.get_mut(active_session_id) {
            Some(session) if !session.closing => {
                session.closing = true;
                true
            }
            _ => false,
        })
    }

    pub fn end_close(&self, active_session_id: &str, refine_failed: bool) {
        self.update(active_session_id, |session| {
            session.closing = false;
            session.close_refine_failed |= refine_failed;
        });
    }
}

/// Ce qu'une fermeture a fait.
#[derive(Debug)]
pub struct CloseOutcome {
    /// La refine, si la conversation avait de nouveaux tours.
    pub refine: Option<anyhow::Result<RefineRun>>,
    /// Le worker a été tué (mise en veille).
    pub killed: bool,
}

impl CloseOutcome {
    pub fn refine_failed(&self) -> bool {
        matches!(self.refine, Some(Err(_)))
    }
}

/// Ferme une conversation : refine locale par la file (auteur
/// `refine:close`) s'il y a au moins un nouveau tour, puis, pour une mise en
/// veille, `Kill` du worker même si la refine a échoué, sauf si
/// `back_in_use` dit que la conversation a repris entre-temps (affichée de
/// nouveau, ou un tour en cours). Une refine qui échoue laisse
/// `pending = 1`.
pub async fn close_conversation(
    client: &DaemonClient,
    socket_path: &Path,
    store: AppStore,
    agent_dir: PathBuf,
    active_session_id: &str,
    thread: ThreadContext,
    action: CloseAction,
    back_in_use: impl Fn() -> bool,
) -> CloseOutcome {
    let conversation_id = thread.conversation_id.clone();
    let turns = {
        let (store, conversation_id) = (store.clone(), conversation_id.clone());
        tokio::task::spawn_blocking(move || store.refine_state(&conversation_id))
            .await
            .ok()
            .and_then(Result::ok)
            .map_or(0, |state| state.user_turns_since_refine)
    };
    let refine = if turns >= 1 {
        Some(
            run_refine(
                socket_path,
                store.clone(),
                agent_dir,
                active_session_id,
                thread,
                RefineOrigin::Close,
                None,
            )
            .await,
        )
    } else {
        None
    };
    if let Some(Err(error)) = &refine {
        tracing::warn!(error = %error, "prime close refine failed, retried at next start");
        let pending =
            tokio::task::spawn_blocking(move || store.set_refine_pending(&conversation_id, true))
                .await;
        if !matches!(pending, Ok(Ok(()))) {
            tracing::warn!("prime close refine not marked pending");
        }
    }
    let killed = action == CloseAction::Sleep
        && !back_in_use()
        && match crate::prime_session::kill_session(client, active_session_id).await {
            Ok(()) => true,
            Err(error) => {
                tracing::warn!(error = %error, "prime idle session not killed");
                false
            }
        };
    CloseOutcome { refine, killed }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MIN: Duration = Duration::from_secs(60);

    fn activity(
        start: Instant,
        hidden_after: Option<Duration>,
        idle_after: Duration,
    ) -> ActivitySnapshot {
        ActivitySnapshot {
            hidden_since: hidden_after.map(|after| start + after),
            last_activity: start + idle_after,
            turn_running: false,
            close_refine_failed: false,
        }
    }

    #[test]
    fn a_displayed_conversation_never_closes() {
        let start = Instant::now();
        let now = start + 200 * MIN;
        assert_eq!(
            close_action(now, &activity(start, None, Duration::ZERO), 10, false),
            None
        );
    }

    #[test]
    fn ten_hidden_minutes_and_three_turns_refine() {
        let start = Instant::now();
        let shown = activity(start, Some(Duration::ZERO), Duration::ZERO);
        assert_eq!(close_action(start + 9 * MIN, &shown, 3, false), None);
        assert_eq!(close_action(start + 10 * MIN, &shown, 2, false), None);
        assert_eq!(
            close_action(start + 10 * MIN, &shown, 3, false),
            Some(CloseAction::Refine)
        );
        // Pas pendant un tour, ni avec un sous-agent vivant.
        let running = ActivitySnapshot {
            turn_running: true,
            ..shown
        };
        assert_eq!(close_action(start + 10 * MIN, &running, 3, false), None);
        assert_eq!(close_action(start + 10 * MIN, &shown, 3, true), None);
        // Après un échec, pas avant un nouveau tour.
        let failed = ActivitySnapshot {
            close_refine_failed: true,
            ..shown
        };
        assert_eq!(close_action(start + 10 * MIN, &failed, 3, false), None);
    }

    #[test]
    fn ninety_hidden_and_idle_minutes_put_the_worker_to_sleep() {
        let start = Instant::now();
        let idle = activity(start, Some(Duration::ZERO), Duration::ZERO);
        assert_eq!(close_action(start + 89 * MIN, &idle, 0, false), None);
        assert_eq!(
            close_action(start + 90 * MIN, &idle, 0, false),
            Some(CloseAction::Sleep)
        );
        assert_eq!(
            close_action(start + 90 * MIN, &idle, 5, false),
            Some(CloseAction::Sleep)
        );
        // Une refine ratée n'empêche pas la mise en veille.
        let failed = ActivitySnapshot {
            close_refine_failed: true,
            ..idle
        };
        assert_eq!(
            close_action(start + 90 * MIN, &failed, 1, false),
            Some(CloseAction::Sleep)
        );
        // Un tour fini il y a 30 min repousse la veille.
        let recent = activity(start, Some(Duration::ZERO), 60 * MIN);
        assert_eq!(close_action(start + 100 * MIN, &recent, 0, false), None);
        assert_eq!(
            close_action(start + 150 * MIN, &recent, 0, false),
            Some(CloseAction::Sleep)
        );
        // Ni pendant un tour, ni avec un sous-agent vivant.
        let running = ActivitySnapshot {
            turn_running: true,
            ..idle
        };
        assert_eq!(close_action(start + 200 * MIN, &running, 1, false), None);
        assert_eq!(close_action(start + 200 * MIN, &idle, 1, true), None);
    }

    #[test]
    fn the_tracker_follows_windows_and_turns() {
        let tracker = CloseTracker::default();
        let start = Instant::now();
        tracker.set_displayed("conv-1", "main", true, start);
        tracker.session_opened("s1", "conv-1", start);
        assert_eq!(tracker.snapshot("s1").unwrap().hidden_since, None);
        // Deux fenêtres : cachée seulement quand les deux l'ont quittée.
        tracker.set_displayed("conv-1", "second", true, start);
        tracker.set_displayed("conv-1", "main", false, start + MIN);
        assert_eq!(tracker.snapshot("s1").unwrap().hidden_since, None);
        tracker.forget_window("second", start + 2 * MIN);
        assert_eq!(
            tracker.snapshot("s1").unwrap().hidden_since,
            Some(start + 2 * MIN)
        );
        tracker.set_displayed("conv-1", "main", true, start + 3 * MIN);
        assert_eq!(tracker.snapshot("s1").unwrap().hidden_since, None);

        tracker.turn_started("s1");
        assert!(tracker.snapshot("s1").unwrap().turn_running);
        tracker.turn_ended("s1", start + 4 * MIN);
        let snapshot = tracker.snapshot("s1").unwrap();
        assert!(!snapshot.turn_running);
        assert_eq!(snapshot.last_activity, start + 4 * MIN);

        // Une seule fermeture à la fois ; un échec bloque jusqu'au prompt.
        assert!(tracker.begin_close("s1"));
        assert!(!tracker.begin_close("s1"));
        assert!(tracker.sessions().is_empty());
        tracker.end_close("s1", true);
        assert!(tracker.snapshot("s1").unwrap().close_refine_failed);
        tracker.user_prompted("s1", start + 5 * MIN);
        assert!(!tracker.snapshot("s1").unwrap().close_refine_failed);
        assert_eq!(
            tracker.sessions(),
            vec![("s1".to_string(), "conv-1".to_string())]
        );

        tracker.session_closed("s1");
        assert!(tracker.snapshot("s1").is_none());
    }

    #[test]
    fn a_session_opened_out_of_sight_counts_as_hidden_from_its_opening() {
        let tracker = CloseTracker::default();
        let start = Instant::now();
        tracker.session_opened("s1", "conv-1", start);
        assert_eq!(tracker.snapshot("s1").unwrap().hidden_since, Some(start));
    }
}
