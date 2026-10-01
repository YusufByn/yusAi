//! Binaire multi-rôle : l'exécutable lancé en `--mode daemon` sert un
//! superviseur Prime auquel `DaemonClient` se connecte.

use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use sinew_desktop_lib::prime::ensure_daemon_running_with;

fn scratch_dir() -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.subsec_nanos());
    std::env::temp_dir().join(format!("yusai-prime-test-{}-{nanos}", std::process::id()))
}

#[tokio::test(flavor = "multi_thread")]
async fn spawns_supervisor_and_connects() {
    let root = scratch_dir();
    let agent_dir = root.join("agent");
    #[cfg(unix)]
    let socket_path = root.join("daemon.sock");
    #[cfg(not(unix))]
    let socket_path = PathBuf::from(format!(
        r"\\.\pipe\yusai-prime-test-{}",
        root.file_name().unwrap().to_string_lossy()
    ));
    let exe = PathBuf::from(env!("CARGO_BIN_EXE_Sinew"));

    let (client, _events) = ensure_daemon_running_with(&exe, &socket_path, &agent_dir)
        .await
        .expect("daemon starts and accepts a client");
    #[cfg(unix)]
    assert!(
        socket_path.exists(),
        "socket {} exists",
        socket_path.display()
    );
    assert_eq!(
        client.hello().get("schemaId"),
        Some(&serde_json::json!(pa_types::daemon::DAEMON_SCHEMA_ID))
    );
    // Le superviseur écrit son état dans le dossier temporaire passé, pas
    // dans celui de yusAi : `daemon-workers` n'est créé que par lui.
    let workers_dir = agent_dir.join("daemon-workers");
    for _ in 0..100 {
        if workers_dir.is_dir() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    assert!(
        workers_dir.is_dir(),
        "supervisor state lives in {}",
        agent_dir.display()
    );

    // Un second appel réutilise le superviseur déjà lancé.
    let (second, _second_events) = ensure_daemon_running_with(&exe, &socket_path, &agent_dir)
        .await
        .expect("running daemon is reused");
    assert_eq!(
        second.hello().get("supervisorPid"),
        client.hello().get("supervisorPid")
    );
    second.close();

    client
        .request_ok(pa_types::daemon::DaemonCommand::Shutdown {
            id: None,
            force: Some(true),
            rest: serde_json::Map::default(),
        })
        .await
        .expect("daemon accepts shutdown");
    client.close();
    let _ = std::fs::remove_dir_all(&root);
}
