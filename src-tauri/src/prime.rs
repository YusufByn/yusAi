//! Intégration Prime Agent (vendor/prime-agent) : binaire multi-rôle et
//! lancement du daemon propre à yusAi.
//!
//! Le même exécutable sert trois rôles :
//! - IDE (lancement normal) ;
//! - superviseur Prime (`--mode daemon --daemon-socket <path>`) ;
//! - worker de session Prime (`worker`, avec `WORKER_ROLE_ENV=1`), lancé
//!   par le superviseur via `current_exe() worker`
//!   (pa-daemon/src/supervisor/supervision.rs:433-439).
//!
//! On parle au daemon par son protocole natif
//! (`pa_tui::daemon_client::DaemonClient` + `pa_types::daemon::DaemonCommand`),
//! jamais par ACP.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context, Result};
use pa_tui::daemon_client::{DaemonClient, DaemonClientEvent};
use tokio::sync::mpsc::UnboundedReceiver;

/// Dossier d'état de Prime (`pa_daemon::paths::AGENT_DIR_ENV`).
const AGENT_DIR_ENV: &str = pa_daemon::paths::AGENT_DIR_ENV;
/// Dossier des ressources packagées de Prime (prime-agent-runtime, skills…).
const PACKAGE_DIR_ENV: &str = "PI_PACKAGE_DIR";
/// Interrupteur de télémétrie de Prime (pa-telemetry/src/env.rs:27-33).
const TELEMETRY_ENV: &str = "PRIME_AGENT_TELEMETRY";
/// Venv du noyau Python. Sans elle, Prime prend `~/.prime/agent/kernel-venv`
/// (pa-core/src/kernel/bootstrap/venv/layout.rs:20-29), partagé avec une
/// installation séparée de Prime qui pourrait le reconstruire à une autre
/// version du runtime. Les workers l'héritent du superviseur
/// (pa-daemon/src/supervisor/supervision.rs:433-447).
const KERNEL_VENV_ENV: &str = "PRIME_AGENT_KERNEL_VENV";

/// Budget de démarrage du superviseur (pa-cli: `DAEMON_STARTUP_TIMEOUT_MS`).
const DAEMON_STARTUP_TIMEOUT: Duration = Duration::from_secs(30);
/// Attente de fermeture d'un daemon obsolète (pa-cli: `DAEMON_SHUTDOWN_WAIT_MS`).
const DAEMON_SHUTDOWN_WAIT: Duration = Duration::from_secs(5);
const DAEMON_PROBE_INTERVAL: Duration = Duration::from_millis(25);

/// Point d'entrée de `main` : exécute le rôle Prime demandé par argv et
/// renvoie son code de sortie, ou `None` pour lancer l'IDE normalement.
///
/// Même routage que pa-cli (pa-cli/src/lib.rs:177-183) : avec la variable
/// de rôle worker, `worker` ou `--mode daemon` démarrent un worker (un
/// superviseur ne doit jamais démarrer dans l'env d'un worker).
pub fn run_role_from_args() -> Option<i32> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let daemon_mode = args.windows(2).any(|pair| pair == ["--mode", "daemon"]);
    let worker_env = std::env::var(pa_daemon::worker::WORKER_ROLE_ENV).unwrap_or_default() == "1";

    if worker_env && (args.first().map(String::as_str) == Some("worker") || daemon_mode) {
        pa_types::memory_release::cap_thread_arenas();
        return Some(block_on_role(pa_daemon::worker::run_worker()));
    }

    if daemon_mode {
        pa_types::memory_release::cap_thread_arenas();
        // Lancement manuel en dev : l'IDE passe ces valeurs explicitement.
        #[cfg(debug_assertions)]
        if std::env::var_os(PACKAGE_DIR_ENV).is_none() {
            std::env::set_var(PACKAGE_DIR_ENV, dev_package_dir());
        }
        let socket_path = flag_value(&args, "--daemon-socket")
            .map(PathBuf::from)
            .unwrap_or_else(daemon_socket_path);
        // Les workers reçoivent ce dossier par leur env de lancement
        // (pa-daemon/src/descriptor.rs:102-105).
        let agent_dir = flag_value(&args, "--agent-dir")
            .map(PathBuf::from)
            .unwrap_or_else(agent_dir);
        let options = pa_daemon::supervisor::SupervisorOptions {
            socket_path,
            agent_dir,
        };
        return Some(block_on_role(pa_daemon::supervisor::run_supervisor(
            options,
        )));
    }

    #[cfg(debug_assertions)]
    if args.first().map(String::as_str) == Some("--prime-ping") {
        let socket_path = args
            .get(1)
            .map(PathBuf::from)
            .unwrap_or_else(daemon_socket_path);
        return Some(block_on_role(ping(socket_path)));
    }

    None
}

/// Runtime tokio dédié au rôle (pa-cli/src/daemon_mode.rs:18-21 et
/// pa-cli/src/lib.rs:350-358).
fn block_on_role<F>(future: F) -> i32
where
    F: std::future::Future<Output = Result<()>>,
{
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("Error: {error:#}");
            return 1;
        }
    };
    match runtime.block_on(future) {
        Ok(()) => 0,
        Err(error) => {
            eprintln!("Error: {error:#}");
            1
        }
    }
}

fn flag_value<'a>(args: &'a [String], flag: &str) -> Option<&'a str> {
    let prefix = format!("{flag}=");
    args.iter().enumerate().find_map(|(index, arg)| {
        if arg == flag {
            args.get(index + 1).map(String::as_str)
        } else {
            arg.strip_prefix(&prefix)
        }
    })
}

/// Dossier de données local de yusAi (le même que `AppStore::open_default`).
fn data_dir() -> PathBuf {
    directories::ProjectDirs::from("dev", "hyrak", "sinew")
        .map(|dirs| dirs.data_local_dir().to_path_buf())
        .unwrap_or_else(|| std::env::temp_dir().join("sinew"))
}

/// Dossier d'état de Prime propre à yusAi (sessions, auth, logs workers).
pub fn agent_dir() -> PathBuf {
    data_dir().join("prime-agent")
}

/// Socket du superviseur propre à yusAi.
///
/// Hors du dossier de sockets de Prime (`<tmp>/prime-agent-<uid>/`) pour que
/// la découverte de daemons de la CLI `prime-agent` ne le voie pas
/// (pa-cli/src/daemon_discovery/mod.rs:148 et 483). Dans le dossier temp et
/// pas sous `data_dir` : macOS refuse les chemins AF_UNIX trop longs
/// (pa-types/src/platform/transport.rs:96-117). Le hash du dossier d'état
/// rend le nom propre à l'utilisateur.
#[cfg(unix)]
pub fn daemon_socket_path() -> PathBuf {
    let key = pa_daemon::paths::hash_key(&agent_dir().to_string_lossy(), 12);
    std::env::temp_dir()
        .join(format!("yusai-prime-{key}"))
        .join("daemon.sock")
}

#[cfg(not(unix))]
pub fn daemon_socket_path() -> PathBuf {
    let key = pa_daemon::paths::hash_key(&agent_dir().to_string_lossy(), 12);
    PathBuf::from(format!(r"\\.\pipe\yusai-prime-daemon-{key}"))
}

/// Environnement propre à yusAi passé au seul superviseur (ses workers en
/// héritent) : jamais posé sur le processus IDE, dont les terminaux et
/// l'outil bash viseraient sinon l'état Prime de yusAi.
///
/// `PRIME_AGENT_TELEMETRY=0` coupe la télémétrie du superviseur
/// (pa-daemon/src/supervisor.rs:343-346) et l'événement « model refused »
/// des workers (pa-daemon/src/model_allowlist.rs:139-145) ; la liste des
/// modèles autorisés n'en dépend pas (model_allowlist.rs:31-91). La
/// télémétrie de session des workers se coupe au `Create`
/// (prime_session.rs).
fn supervisor_env(agent_dir: &Path, kernel_venv: &Path) -> Vec<(&'static str, OsString)> {
    #[cfg_attr(not(debug_assertions), allow(unused_mut))]
    let mut env = vec![
        (AGENT_DIR_ENV, agent_dir.as_os_str().to_owned()),
        (TELEMETRY_ENV, OsString::from("0")),
        (KERNEL_VENV_ENV, kernel_venv.as_os_str().to_owned()),
    ];
    #[cfg(debug_assertions)]
    env.push((PACKAGE_DIR_ENV, dev_package_dir().into_os_string()));
    env
}

/// En dev, les ressources de Prime viennent de vendor/prime-agent (les
/// ressources de l'app viendront avec le packaging).
#[cfg(debug_assertions)]
fn dev_package_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../vendor/prime-agent")
}

/// Résultat d'une sonde du socket (pa-cli/src/interactive_mode/daemon.rs:20-30).
enum DaemonProbe {
    Absent,
    Current,
    Stale(Box<DaemonClient>),
}

/// Connexion + hello, puis comparaison protocole/schéma avec ce build
/// (pa-cli/src/interactive_mode/daemon.rs:33-50).
async fn probe_daemon(socket_path: &Path) -> DaemonProbe {
    let Ok((client, _events)) = DaemonClient::connect(socket_path).await else {
        return DaemonProbe::Absent;
    };
    let hello = client.hello();
    let current = hello.get("protocol").and_then(|p| p.get("version"))
        == Some(&serde_json::json!(
            pa_types::daemon::DAEMON_PROTOCOL_VERSION
        ))
        && hello.get("schemaId") == Some(&serde_json::json!(pa_types::daemon::DAEMON_SCHEMA_ID));
    if current {
        client.close();
        DaemonProbe::Current
    } else {
        DaemonProbe::Stale(Box::new(client))
    }
}

/// S'assure qu'un superviseur à jour écoute sur `socket_path` (le lance en
/// processus détaché sinon) puis renvoie un client connecté et son flux
/// d'événements.
pub async fn ensure_daemon_running(
    socket_path: &Path,
) -> Result<(DaemonClient, UnboundedReceiver<DaemonClientEvent>)> {
    let exe = std::env::current_exe().context("resolve the yusAi executable")?;
    ensure_daemon_running_with(&exe, socket_path, &agent_dir()).await
}

/// [`ensure_daemon_running`] avec un exécutable et un dossier d'état
/// explicites (tests). Équivalent de `ensure_daemon_running` /
/// `ensure_daemon_running_with` de pa-cli/src/interactive_mode/daemon.rs:61-117.
/// Le venv du noyau est `<agent_dir>/kernel-venv`.
pub async fn ensure_daemon_running_with(
    exe: &Path,
    socket_path: &Path,
    agent_dir: &Path,
) -> Result<(DaemonClient, UnboundedReceiver<DaemonClientEvent>)> {
    ensure_daemon_running_with_kernel_venv(
        exe,
        socket_path,
        agent_dir,
        &agent_dir.join("kernel-venv"),
    )
    .await
}

/// [`ensure_daemon_running_with`] avec un venv de noyau explicite : les
/// tests qui exécutent des cellules gardent un venv stable d'un passage à
/// l'autre au lieu d'en construire un par dossier temporaire.
pub async fn ensure_daemon_running_with_kernel_venv(
    exe: &Path,
    socket_path: &Path,
    agent_dir: &Path,
    kernel_venv: &Path,
) -> Result<(DaemonClient, UnboundedReceiver<DaemonClientEvent>)> {
    match probe_daemon(socket_path).await {
        DaemonProbe::Current => return DaemonClient::connect_with_retry(socket_path).await,
        DaemonProbe::Stale(client) => shutdown_stale_daemon(*client, socket_path).await?,
        DaemonProbe::Absent => {}
    }
    spawn_supervisor_detached(exe, socket_path, agent_dir, kernel_venv)?;
    let deadline = Instant::now() + DAEMON_STARTUP_TIMEOUT;
    loop {
        match probe_daemon(socket_path).await {
            DaemonProbe::Current => break,
            // Un lanceur concurrent a pris le socket avec un autre schéma :
            // on resonde jusqu'à l'échéance.
            DaemonProbe::Stale(client) => client.close(),
            DaemonProbe::Absent => {}
        }
        if Instant::now() > deadline {
            return Err(anyhow!(
                "Timed out waiting for the Prime Agent daemon to start on {}",
                socket_path.display()
            ));
        }
        tokio::time::sleep(DAEMON_PROBE_INTERVAL).await;
    }
    DaemonClient::connect_with_retry(socket_path).await
}

/// Ferme un daemon obsolète s'il est inactif ; un daemon occupé refuse le
/// remplacement (pa-cli/src/interactive_mode/daemon.rs:121-161).
async fn shutdown_stale_daemon(client: DaemonClient, socket_path: &Path) -> Result<()> {
    let sessions = client
        .request_ok(pa_types::daemon::DaemonCommand::List {
            id: None,
            all: None,
            cwd: None,
            session_dir: None,
            include_client_owned: None,
            rest: serde_json::Map::default(),
        })
        .await;
    let busy = sessions.map_or(true, |data| {
        data.get("sessions")
            .and_then(serde_json::Value::as_array)
            .is_none_or(|rows| {
                rows.iter()
                    .any(|row| row.get("isSessionActive") == Some(&serde_json::json!(true)))
            })
    });
    if busy {
        client.close();
        return Err(anyhow!(
            "An incompatible Prime Agent daemon with active work is running on {}",
            socket_path.display()
        ));
    }
    let _ = client
        .request_ok(pa_types::daemon::DaemonCommand::Shutdown {
            id: None,
            force: None,
            rest: serde_json::Map::default(),
        })
        .await;
    client.close();
    let deadline = Instant::now() + DAEMON_SHUTDOWN_WAIT;
    while Instant::now() < deadline
        && pa_daemon::socket::can_connect(socket_path, Duration::from_millis(250)).await
    {
        tokio::time::sleep(DAEMON_PROBE_INTERVAL).await;
    }
    Ok(())
}

/// Lance `exe --mode daemon --daemon-socket <path>` détaché
/// (pa-cli/src/interactive_mode/daemon.rs:177-212), avec le dossier d'état
/// et l'env de [`supervisor_env`]. Le stderr du superviseur va dans
/// `<agent_dir>/yusai-supervisor.log`.
fn spawn_supervisor_detached(
    exe: &Path,
    socket_path: &Path,
    agent_dir: &Path,
    kernel_venv: &Path,
) -> Result<()> {
    std::fs::create_dir_all(agent_dir)
        .with_context(|| format!("create the Prime agent dir {}", agent_dir.display()))?;
    let stderr = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(agent_dir.join("yusai-supervisor.log"))
        .map(Stdio::from)
        .unwrap_or_else(|_| Stdio::null());
    let mut command = Command::new(exe);
    command
        .args(["--mode", "daemon", "--daemon-socket"])
        .arg(socket_path)
        .arg("--agent-dir")
        .arg(agent_dir)
        .current_dir(agent_dir)
        .envs(supervisor_env(agent_dir, kernel_venv))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(stderr)
        // Les variables de rôle héritées feraient démarrer un worker au lieu
        // du superviseur (même liste que pa-cli).
        .env_remove(pa_daemon::worker::WORKER_ROLE_ENV)
        .env_remove(pa_daemon::worker::WORKER_TOKEN_ENV)
        .env_remove(pa_daemon::worker::WORKER_ACTIVE_SESSION_ID_ENV)
        .env_remove(pa_daemon::worker::WORKER_RECOVERY_JOURNAL_ENV)
        .env_remove(pa_daemon::worker::WORKER_SUPERVISOR_SOCKET_ENV)
        .env_remove(pa_daemon::worker::WORKER_SOCKET_ENV)
        .env_remove(pa_daemon::worker::WORKER_INSTANCE_ID_ENV)
        .env_remove(pa_daemon::worker::WORKER_SCRIPT_ENV)
        .env_remove(pa_daemon::lease::SESSION_LEASE_OWNER_ID_ENV);
    pa_core::platform::process::set_new_process_group(&mut command);
    command
        .spawn()
        .with_context(|| format!("spawn the Prime Agent daemon on {}", socket_path.display()))?;
    Ok(())
}

/// Commande de debug : `Sinew --prime-ping [socket]` lance le daemon si
/// besoin, s'y connecte et affiche le hello.
#[cfg(debug_assertions)]
async fn ping(socket_path: PathBuf) -> Result<()> {
    let (client, _events) = ensure_daemon_running(&socket_path).await?;
    println!("socket: {}", socket_path.display());
    println!("protocol: {:?}", client.protocol());
    println!("hello: {}", client.hello());
    client.close();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn supervisor_env_isolates_state_telemetry_and_kernel_venv() {
        let env = supervisor_env(Path::new("/agent"), Path::new("/venv"));
        let value = |key: &str| {
            env.iter()
                .find(|(name, _)| *name == key)
                .map(|(_, value)| value.clone())
        };
        assert_eq!(value(AGENT_DIR_ENV), Some(OsString::from("/agent")));
        assert_eq!(value(TELEMETRY_ENV), Some(OsString::from("0")));
        assert_eq!(value(KERNEL_VENV_ENV), Some(OsString::from("/venv")));
    }
}
