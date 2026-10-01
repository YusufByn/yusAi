//! Pont d'auth Anthropic yusAi → Prime.
//!
//! Prime réutilise la connexion Anthropic de yusAi sans reconnexion. Le
//! refresh token n'est utilisable qu'une fois (pa-core/src/auth/manager/
//! lookup.rs:256-259) et le provider de yusAi garde le sien en mémoire sans
//! relire son fichier (crates/sinew-anthropic/src/auth.rs:143-168) : un seul
//! acteur doit donc rafraîchir, et c'est ce provider.
//!
//! - Le provider partage son `Credential` (même `Arc<Mutex>`) avec ce module
//!   via [`set_anthropic_credential`] ; on le rafraîchit avec son propre
//!   `bearer_or_key`, comme le ferait une requête Sinew.
//! - Prime reçoit dans son `auth.json` le seul token d'accès, sans refresh
//!   token : il ne peut jamais le faire tourner (sans refresh token, son
//!   refresh s'arrête avant tout appel réseau,
//!   pa-core/src/auth/provider_oauth.rs:82-89).
//! - Une tâche recopie le token avant chaque expiration, et retire le
//!   credential de Prime à la déconnexion d'Anthropic dans yusAi.

use std::path::Path;
use std::sync::{Arc, Mutex as StdMutex, OnceLock};
use std::time::Duration;

use anyhow::Result;
use pa_core::auth::{AuthCredential, AuthStorage, NoOAuth, ANTHROPIC_PROVIDER_ID};
use sinew_anthropic::Credential;
use tokio::sync::Notify;

/// Marge avant l'expiration enregistrée par yusAi : son provider rafraîchit
/// 5 min avant (`REFRESH_SKEW_MS`, sinew-anthropic/src/auth.rs:21,189-191).
const REFRESH_LEAD: Duration = Duration::from_secs(5 * 60);
/// Recopie au moins aussi souvent : un refresh fait par une requête Sinew
/// arrive ainsi vite chez Prime.
const MAX_SYNC_INTERVAL: Duration = Duration::from_secs(5 * 60);
const MIN_SYNC_INTERVAL: Duration = Duration::from_secs(30);

fn credential_slot() -> &'static StdMutex<Option<Credential>> {
    static SLOT: OnceLock<StdMutex<Option<Credential>>> = OnceLock::new();
    SLOT.get_or_init(|| StdMutex::new(None))
}

fn credential_changed() -> &'static Notify {
    static NOTIFY: OnceLock<Notify> = OnceLock::new();
    NOTIFY.get_or_init(Notify::new)
}

/// Enregistre le credential du provider Anthropic de yusAi (`None` à la
/// déconnexion). Appelé là où le provider est installé ou retiré.
pub fn set_anthropic_credential(credential: Option<Credential>) {
    if let Ok(mut slot) = credential_slot().lock() {
        *slot = credential;
    }
    credential_changed().notify_one();
}

/// Credential Prime : token d'accès seul, expiration de yusAi.
fn prime_credential(access: String, expires_at_ms: i64) -> AuthCredential {
    AuthCredential::Oauth {
        access,
        refresh: None,
        expires: expires_at_ms,
        account_id: None,
        enterprise_url: None,
        endpoint: None,
        token_endpoint: None,
        client_id: None,
        resource: None,
        issuer: None,
    }
}

/// Écrit (ou retire) le credential Anthropic dans `<agent_dir>/auth.json`
/// avec le verrou de Prime (`AuthStorage::set` / `remove`). N'écrit rien si
/// le contenu est déjà à jour.
pub fn write_prime_anthropic(agent_dir: &Path, token: Option<(String, i64)>) {
    let mut storage = AuthStorage::create_with_oauth(agent_dir, Arc::new(NoOAuth));
    let current = storage.get_all().credential(ANTHROPIC_PROVIDER_ID);
    match token {
        Some((access, expires_at_ms)) => {
            let next = prime_credential(access, expires_at_ms);
            if current.as_ref() != Some(&next) {
                storage.set(ANTHROPIC_PROVIDER_ID, next);
            }
        }
        None => {
            if current.is_some() {
                storage.remove(ANTHROPIC_PROVIDER_ID);
            }
        }
    }
}

/// Une synchronisation : rafraîchit si besoin via le credential partagé,
/// puis recopie chez Prime. Renvoie l'expiration recopiée.
pub async fn sync_anthropic(agent_dir: &Path, http: &reqwest::Client) -> Result<Option<i64>> {
    let credential = credential_slot().lock().ok().and_then(|slot| slot.clone());
    let token = match credential {
        Some(credential) => {
            let access = credential
                .bearer_or_key(http)
                .await
                .map_err(|error| anyhow::anyhow!("{error}"))?;
            // L'expiration n'est exposée que par le fichier que le provider
            // réécrit à chaque refresh (sinew-anthropic/src/auth.rs:239-251).
            let expires_at_ms = sinew_anthropic::load_default_auth_status()
                .map_err(|error| anyhow::anyhow!("{error}"))?
                .expires_at_ms
                .unwrap_or(0);
            Some((access, expires_at_ms))
        }
        None => None,
    };
    let expires = token.as_ref().map(|(_, expires)| *expires);
    let agent_dir = agent_dir.to_path_buf();
    tokio::task::spawn_blocking(move || write_prime_anthropic(&agent_dir, token)).await?;
    Ok(expires)
}

/// Prochaine synchronisation : juste après le seuil de refresh du provider,
/// bornée entre 30 s et 5 min.
fn next_sync_delay(expires_at_ms: Option<i64>, now_ms: i64) -> Duration {
    let Some(expires_at_ms) = expires_at_ms else {
        return MAX_SYNC_INTERVAL;
    };
    let refresh_at_ms = expires_at_ms - REFRESH_LEAD.as_millis() as i64 + 1_000;
    let wait = Duration::from_millis(refresh_at_ms.saturating_sub(now_ms).max(0) as u64);
    wait.clamp(MIN_SYNC_INTERVAL, MAX_SYNC_INTERVAL)
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_millis() as i64)
}

/// Lance la tâche de synchronisation (une seule par processus) après une
/// première synchronisation immédiate, pour que la session qui suit ait
/// déjà le token.
pub async fn ensure_anthropic_sync(agent_dir: &Path) {
    static STARTED: OnceLock<()> = OnceLock::new();
    let http = reqwest::Client::new();
    let first = sync_anthropic(agent_dir, &http).await;
    if let Err(error) = &first {
        tracing::warn!(error = %error, "prime anthropic auth sync failed");
    }
    if STARTED.set(()).is_err() {
        return;
    }
    let agent_dir = agent_dir.to_path_buf();
    let mut expires = first.ok().flatten();
    tauri::async_runtime::spawn(async move {
        loop {
            let delay = next_sync_delay(expires, now_ms());
            tokio::select! {
                _ = tokio::time::sleep(delay) => {}
                _ = credential_changed().notified() => {}
            }
            expires = match sync_anthropic(&agent_dir, &http).await {
                Ok(expires) => expires,
                Err(error) => {
                    tracing::warn!(error = %error, "prime anthropic auth sync failed");
                    None
                }
            };
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch_dir(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "yusai-prime-auth-{name}-{}-{}",
            std::process::id(),
            now_ms()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Prime lit le token recopié comme une auth OAuth Anthropic, et un
    /// token expiré sans refresh token n'est plus servi (aucun refresh
    /// tenté).
    #[test]
    fn prime_reads_the_mirrored_token_and_never_refreshes_it() {
        let agent_dir = scratch_dir("mirror");
        write_prime_anthropic(
            &agent_dir,
            Some(("sk-ant-oat-test".into(), now_ms() + 3_600_000)),
        );
        let mut prime = AuthStorage::create(&agent_dir);
        assert_eq!(
            prime.get_api_key(ANTHROPIC_PROVIDER_ID).as_deref(),
            Some("sk-ant-oat-test")
        );
        let stored: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(agent_dir.join("auth.json")).unwrap())
                .unwrap();
        assert_eq!(stored["anthropic"]["type"], "oauth");
        assert!(stored["anthropic"]
            .get("refresh")
            .is_none_or(|r| r.is_null()));

        write_prime_anthropic(&agent_dir, Some(("sk-ant-oat-old".into(), now_ms() - 1)));
        let mut prime = AuthStorage::create(&agent_dir);
        assert_eq!(prime.get_api_key(ANTHROPIC_PROVIDER_ID), None);

        write_prime_anthropic(&agent_dir, None);
        let prime = AuthStorage::create(&agent_dir);
        assert!(!prime.has(ANTHROPIC_PROVIDER_ID));
        let _ = std::fs::remove_dir_all(&agent_dir);
    }

    #[test]
    fn next_sync_lands_just_after_the_provider_refresh_threshold() {
        let now = 1_000_000_000;
        let lead = REFRESH_LEAD.as_millis() as i64;
        // Loin de l'expiration : plafonné à 5 min.
        assert_eq!(
            next_sync_delay(Some(now + 3_600_000), now),
            MAX_SYNC_INTERVAL
        );
        // Seuil dans 2 min : réveil 1 s après le seuil.
        assert_eq!(
            next_sync_delay(Some(now + lead + 120_000), now),
            Duration::from_millis(121_000)
        );
        // Seuil dépassé ou inconnu : bornes.
        assert_eq!(next_sync_delay(Some(now), now), MIN_SYNC_INTERVAL);
        assert_eq!(next_sync_delay(None, now), MAX_SYNC_INTERVAL);
    }
}
