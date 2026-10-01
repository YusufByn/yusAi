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

    // Télémétrie coupée : le superviseur ne crée pas d'identifiant
    // d'installation (pa-core/src/session_engine/telemetry.rs:1078).
    assert!(
        !agent_dir.join("telemetry.json").exists(),
        "telemetry is disabled for the supervisor"
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

/// Un prompt envoyé par le protocole natif ressort en `text_delta` dans les
/// `SessionEvent` (réponse scriptée par le moteur `faux` de Prime).
#[tokio::test(flavor = "multi_thread")]
async fn prompt_streams_assistant_text() {
    use pa_tui::daemon_client::DaemonClientEvent;
    use sinew_desktop_lib::prime_session::{create_session, kill_session, prompt};

    let root = scratch_dir();
    let agent_dir = root.join("agent");
    let workspace = root.join("workspace");
    std::fs::create_dir_all(&workspace).unwrap();
    let script = root.join("faux.json");
    std::fs::write(
        &script,
        serde_json::json!({
            "engine": "faux",
            "responses": [{ "text": "bonjour depuis Prime" }],
        })
        .to_string(),
    )
    .unwrap();
    #[cfg(unix)]
    let socket_path = root.join("daemon.sock");
    #[cfg(not(unix))]
    let socket_path = PathBuf::from(format!(
        r"\\.\pipe\yusai-prime-test-{}",
        root.file_name().unwrap().to_string_lossy()
    ));
    let exe = PathBuf::from(env!("CARGO_BIN_EXE_Sinew"));

    let (client, mut events) = ensure_daemon_running_with(&exe, &socket_path, &agent_dir)
        .await
        .expect("daemon starts");
    let session = create_session(
        &client,
        serde_json::json!({
            "cwd": workspace.to_string_lossy(),
            "script": script.to_string_lossy(),
        }),
    )
    .await
    .expect("session created and attached");
    prompt(&client, &session, "salut")
        .await
        .expect("prompt admitted");

    let mut text = String::new();
    let mut seen_types = Vec::new();
    let collected = tokio::time::timeout(std::time::Duration::from_secs(60), async {
        while let Some(event) = events.recv().await {
            let DaemonClientEvent::SessionEvent {
                active_session_id,
                event,
                ..
            } = event
            else {
                continue;
            };
            assert_eq!(active_session_id, session);
            let kind = event["type"].as_str().unwrap_or_default().to_string();
            if kind == "message_update" && event["assistantMessageEvent"]["type"] == "text_delta" {
                text.push_str(
                    event["assistantMessageEvent"]["delta"]
                        .as_str()
                        .unwrap_or_default(),
                );
            }
            let done = kind == "agent_end";
            seen_types.push(kind);
            if done {
                break;
            }
        }
    })
    .await;
    assert!(collected.is_ok(), "turn ended; events seen: {seen_types:?}");
    assert_eq!(text, "bonjour depuis Prime", "events seen: {seen_types:?}");

    // Télémétrie de session coupée au Create : ni identifiant ni copie
    // locale des événements.
    assert!(!agent_dir.join("telemetry.json").exists());
    assert!(!agent_dir.join("telemetry.jsonl").exists());

    kill_session(&client, &session)
        .await
        .expect("session killed");
    let _ = client
        .request_ok(pa_types::daemon::DaemonCommand::Shutdown {
            id: None,
            force: Some(true),
            rest: serde_json::Map::default(),
        })
        .await;
    client.close();
    let _ = std::fs::remove_dir_all(&root);
}
