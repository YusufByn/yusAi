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
    use sinew_desktop_lib::prime_session::{create_session, kill_session, prompt, session_config};

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

    // La config de session se lit (le moteur faux n'a qu'un niveau, `off`).
    let config = session_config(&client, &session)
        .await
        .expect("session config");
    assert_eq!(
        config.model.map(|model| model.id).as_deref(),
        Some("faux-1")
    );
    assert_eq!(config.available_thinking_levels, vec!["off".to_string()]);

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

/// Les appels d'outils arrivent au client tels que `PrimeChatPane` les lit :
/// `tool_execution_start` (`toolCallId`, `toolName`, `args`) puis
/// `tool_execution_end` (`result.content`, `isError`), émis par le worker
/// (pa-daemon/src/worker/turn.rs:905-932). Un script sans `"engine": "faux"`
/// passe par le moteur scripté, qui rejoue des appels d'outils
/// (pa-daemon/src/engine/scripted.rs:16-23).
#[tokio::test(flavor = "multi_thread")]
async fn tool_calls_reach_the_client() {
    use pa_tui::daemon_client::DaemonClientEvent;
    use sinew_desktop_lib::prime_session::{create_session, kill_session, prompt};

    let root = scratch_dir();
    let agent_dir = root.join("agent");
    let workspace = root.join("workspace");
    std::fs::create_dir_all(&workspace).unwrap();
    let script = root.join("scripted.json");
    std::fs::write(
        &script,
        serde_json::json!({
            "responses": [{
                "text": "je lance deux cellules",
                "toolCalls": [
                    {
                        "toolCallId": "call-ok",
                        "toolName": "ipython",
                        "args": { "code": "print(bash('ls'))" },
                        "result": "README.md",
                        "isError": false,
                    },
                    {
                        "toolCallId": "call-err",
                        "toolName": "ipython",
                        "args": { "code": "edit('a.rs', [])" },
                        "result": "ValueError: no edits",
                        "isError": true,
                    },
                ],
            }],
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
    prompt(&client, &session, "vas-y")
        .await
        .expect("prompt admitted");

    let mut tool_events = Vec::new();
    let mut seen_types = Vec::new();
    let collected = tokio::time::timeout(std::time::Duration::from_secs(60), async {
        while let Some(event) = events.recv().await {
            let DaemonClientEvent::SessionEvent { event, .. } = event else {
                continue;
            };
            let kind = event["type"].as_str().unwrap_or_default().to_string();
            if kind.starts_with("tool_execution_") {
                tool_events.push(event);
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

    let summary: Vec<_> = tool_events
        .iter()
        .map(|event| {
            (
                event["type"].as_str().unwrap_or_default(),
                event["toolCallId"].as_str().unwrap_or_default(),
            )
        })
        .collect();
    assert_eq!(
        summary,
        vec![
            ("tool_execution_start", "call-ok"),
            ("tool_execution_end", "call-ok"),
            ("tool_execution_start", "call-err"),
            ("tool_execution_end", "call-err"),
        ],
        "events seen: {seen_types:?}"
    );
    assert_eq!(tool_events[0]["toolName"], "ipython");
    assert_eq!(tool_events[0]["args"]["code"], "print(bash('ls'))");
    assert_eq!(tool_events[1]["isError"], false);
    assert_eq!(tool_events[1]["result"]["content"][0]["type"], "text");
    assert_eq!(tool_events[1]["result"]["content"][0]["text"], "README.md");
    assert_eq!(tool_events[3]["isError"], true);
    assert_eq!(
        tool_events[3]["result"]["content"][0]["text"],
        "ValueError: no edits"
    );

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

/// Fichiers modifiés par un appel d'outil : photo de référence à chaque
/// `agent_start`, comparaison à chaque `tool_execution_end`. Le test joue
/// l'outil en écrivant le fichier pendant la pause scriptée entre le début
/// et la fin de l'appel. Une modification faite à la main entre deux tours
/// n'est pas attribuée à l'appel du tour suivant. La racine photographiée
/// est le dossier de travail de la session, relu auprès du worker.
#[tokio::test(flavor = "multi_thread")]
async fn tool_file_changes_follow_each_turn() {
    use std::sync::Arc;

    use pa_tui::daemon_client::DaemonClientEvent;
    use sinew_app::tool_run::{FileChange, FileChangeKind};
    use sinew_desktop_lib::prime_diffs::PrimeDiffs;
    use sinew_desktop_lib::prime_session::{create_session, kill_session, prompt, session_cwd};

    let root = scratch_dir();
    let agent_dir = root.join("agent");
    let workspace = root.join("workspace");
    std::fs::create_dir_all(&workspace).unwrap();
    std::fs::write(workspace.join("a.txt"), "one\n").unwrap();
    std::fs::write(workspace.join("b.txt"), "hand\n").unwrap();
    let tool_call = |id: &str| {
        serde_json::json!({
            "toolCallId": id,
            "toolName": "ipython",
            "args": { "code": "await edit(path='a.txt', old_str=old, new_str=new)" },
            "result": "Edited a.txt",
            "delayMs": 1500,
        })
    };
    let script = root.join("scripted.json");
    std::fs::write(
        &script,
        serde_json::json!({
            "responses": [
                { "text": "tour 1", "toolCalls": [tool_call("call-1")] },
                { "text": "tour 2", "toolCalls": [tool_call("call-2")] },
            ],
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

    let cwd = session_cwd(&client, &session).await.expect("session cwd");
    assert_eq!(
        cwd, workspace,
        "snapshot root is the session's working directory"
    );
    assert_ne!(Some(cwd.clone()), std::env::current_dir().ok());

    let (changes_tx, mut changes_rx) =
        tokio::sync::mpsc::unbounded_channel::<(String, Vec<FileChange>)>();
    let mut diffs = PrimeDiffs::default();
    diffs.track(
        &session,
        cwd,
        Arc::new(move |tool_call_id, changes| {
            let _ = changes_tx.send((tool_call_id, changes));
        }),
    );

    let mut next_content = ["one\ntwo\n", "one\ntwo\nthree\n"].into_iter();
    let mut seen_types = Vec::new();
    for turn in 0..2 {
        if turn == 1 {
            // Modification à la main entre les deux tours.
            std::fs::write(workspace.join("b.txt"), "hand edited\n").unwrap();
        }
        prompt(&client, &session, "vas-y")
            .await
            .expect("prompt admitted");
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
                diffs.observe(&active_session_id, &event);
                let kind = event["type"].as_str().unwrap_or_default().to_string();
                match kind.as_str() {
                    // La référence est prise avant que « l'outil » n'écrive.
                    "agent_start" => diffs.flush(&session).await,
                    "tool_execution_start" => {
                        std::fs::write(workspace.join("a.txt"), next_content.next().unwrap())
                            .unwrap();
                    }
                    _ => {}
                }
                let done = kind == "agent_end";
                seen_types.push(kind);
                if done {
                    break;
                }
            }
        })
        .await;
        assert!(
            collected.is_ok(),
            "turn {turn} ended; events seen: {seen_types:?}"
        );
    }
    diffs.flush(&session).await;

    let mut reported = Vec::new();
    while let Ok(entry) = changes_rx.try_recv() {
        reported.push(entry);
    }
    let summary: Vec<_> = reported
        .iter()
        .map(|(tool_call_id, changes)| {
            (
                tool_call_id.as_str(),
                changes
                    .iter()
                    .map(|change| {
                        (
                            change.relative_path.as_str(),
                            matches!(change.kind, FileChangeKind::Modified),
                            change.added_lines,
                            change.removed_lines,
                        )
                    })
                    .collect::<Vec<_>>(),
            )
        })
        .collect();
    assert_eq!(
        summary,
        vec![
            ("call-1", vec![("a.txt", true, 1, 0)]),
            ("call-2", vec![("a.txt", true, 1, 0)]),
        ],
        "events seen: {seen_types:?}"
    );

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

/// Fils persistants : une conversation rouvre son fichier de session après
/// un `Kill` (la fermeture de l'IDE), avec son historique et son niveau de
/// réflexion, même si une autre session a changé le défaut global entre-temps
/// (pa-daemon/src/model_switch.rs:221-228). Le fichier vit hors de
/// `sessions/`, et un fichier déjà tenu est rattaché à sa session.
#[tokio::test(flavor = "multi_thread")]
async fn threads_reopen_from_their_file() {
    use pa_tui::daemon_client::DaemonClientEvent;
    use sinew_desktop_lib::prime_session::{
        kill_session, open_thread, prompt, session_config, set_thinking_level, thread_path,
    };

    let root = scratch_dir();
    let agent_dir = root.join("agent");
    let workspace = root.join("workspace");
    std::fs::create_dir_all(&workspace).unwrap();
    let script = root.join("faux.json");
    std::fs::write(
        &script,
        serde_json::json!({
            "engine": "faux",
            "reasoning": true,
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
    let config = serde_json::json!({
        "cwd": workspace.to_string_lossy(),
        "script": script.to_string_lossy(),
    });

    let (client, mut events) = ensure_daemon_running_with(&exe, &socket_path, &agent_dir)
        .await
        .expect("daemon starts");
    assert!(thread_path(&agent_dir, "../escape").is_err());
    let first_path = thread_path(&agent_dir, "conv-1").unwrap();
    let other_path = thread_path(&agent_dir, "conv-2").unwrap();

    let first = open_thread(&client, config.clone(), &first_path)
        .await
        .expect("thread created");
    assert!(first.messages.is_empty());
    assert!(first_path.is_file(), "the thread file is written");
    assert!(!first_path.starts_with(agent_dir.join("sessions")));

    // Un second ouvreur du même fichier rejoint la session qui le tient.
    let again = open_thread(&client, config.clone(), &first_path)
        .await
        .expect("held thread joined");
    assert_eq!(again.active_session_id, first.active_session_id);

    prompt(&client, &first.active_session_id, "salut")
        .await
        .expect("prompt admitted");
    let mut seen_types = Vec::new();
    let turn = tokio::time::timeout(std::time::Duration::from_secs(60), async {
        while let Some(event) = events.recv().await {
            let DaemonClientEvent::SessionEvent { event, .. } = event else {
                continue;
            };
            let kind = event["type"].as_str().unwrap_or_default().to_string();
            let done = kind == "agent_end";
            seen_types.push(kind);
            if done {
                break;
            }
        }
    })
    .await;
    assert!(turn.is_ok(), "turn ended; events seen: {seen_types:?}");

    let levels = session_config(&client, &first.active_session_id)
        .await
        .expect("session config")
        .available_thinking_levels;
    assert!(
        levels.iter().any(|level| level == "high") && levels.iter().any(|level| level == "low"),
        "reasoning faux model levels: {levels:?}"
    );
    set_thinking_level(&client, &first.active_session_id, "high")
        .await
        .expect("thinking level set");
    // Une autre conversation change le défaut global après coup.
    let other = open_thread(&client, config.clone(), &other_path)
        .await
        .expect("other thread created");
    set_thinking_level(&client, &other.active_session_id, "low")
        .await
        .expect("thinking level set");

    // Fermeture de l'IDE : Kill des deux sessions.
    kill_session(&client, &first.active_session_id)
        .await
        .expect("first killed");
    kill_session(&client, &other.active_session_id)
        .await
        .expect("other killed");

    let reopened = open_thread(&client, config.clone(), &first_path)
        .await
        .expect("thread reopened from its file");
    assert_ne!(reopened.active_session_id, first.active_session_id);
    let texts: Vec<(String, String)> = reopened
        .messages
        .iter()
        .map(|message| {
            let role = message["role"].as_str().unwrap_or_default().to_string();
            let text = match &message["content"] {
                serde_json::Value::String(text) => text.clone(),
                serde_json::Value::Array(blocks) => blocks
                    .iter()
                    .filter(|block| block["type"] == "text")
                    .filter_map(|block| block["text"].as_str())
                    .collect::<Vec<_>>()
                    .join(""),
                _ => String::new(),
            };
            (role, text)
        })
        .filter(|(role, _)| role == "user" || role == "assistant")
        .collect();
    assert_eq!(
        texts,
        vec![
            ("user".to_string(), "salut".to_string()),
            ("assistant".to_string(), "bonjour depuis Prime".to_string()),
        ],
        "messages: {:?}",
        reopened.messages
    );
    let restored = session_config(&client, &reopened.active_session_id)
        .await
        .expect("reopened config");
    assert_eq!(restored.thinking_level.as_deref(), Some("high"));
    // Le défaut global, lui, est passé à `low` : c'est le fichier qui a parlé.
    let fresh = open_thread(
        &client,
        config.clone(),
        &thread_path(&agent_dir, "conv-3").unwrap(),
    )
    .await
    .expect("fresh thread created");
    let fresh_config = session_config(&client, &fresh.active_session_id)
        .await
        .expect("fresh config");
    assert_eq!(fresh_config.thinking_level.as_deref(), Some("low"));
    kill_session(&client, &fresh.active_session_id)
        .await
        .expect("fresh killed");

    kill_session(&client, &reopened.active_session_id)
        .await
        .expect("reopened killed");
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

/// Suppression d'une conversation : le worker qui tient le fil est tué,
/// puis `delete_saved_session` retire le fichier, hors de `sessions/`.
#[tokio::test(flavor = "multi_thread")]
async fn deleting_a_thread_kills_its_worker_and_removes_the_file() {
    use sinew_desktop_lib::prime_session::{delete_thread, open_thread, thread_path};

    let root = scratch_dir();
    let agent_dir = root.join("agent");
    let workspace = root.join("workspace");
    std::fs::create_dir_all(&workspace).unwrap();
    let script = root.join("faux.json");
    std::fs::write(
        &script,
        serde_json::json!({ "engine": "faux", "responses": [{ "text": "ok" }] }).to_string(),
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

    let (client, _events) = ensure_daemon_running_with(&exe, &socket_path, &agent_dir)
        .await
        .expect("daemon starts");
    // Nom unique : sous macOS, Prime envoie le fichier à la Corbeille.
    let stem = format!(
        "yusai-prime-test-thread-{}",
        root.file_name().unwrap().to_string_lossy()
    );
    let path = thread_path(&agent_dir, &stem).unwrap();
    let opened = open_thread(
        &client,
        serde_json::json!({
            "cwd": workspace.to_string_lossy(),
            "script": script.to_string_lossy(),
        }),
        &path,
    )
    .await
    .expect("thread opened");
    assert!(path.is_file());

    let killed = delete_thread(&client, &path).await.expect("thread deleted");
    assert_eq!(killed, vec![opened.active_session_id.clone()]);
    assert!(!path.exists(), "the thread file is gone");
    let listed = client
        .request_ok(pa_types::daemon::DaemonCommand::List {
            id: None,
            all: None,
            cwd: None,
            session_dir: None,
            include_client_owned: Some(true),
            rest: serde_json::Map::default(),
        })
        .await
        .expect("list");
    assert!(
        !listed.to_string().contains(&opened.active_session_id),
        "the session is no longer listed: {listed}"
    );
    // Rien à faire sur un fil déjà supprimé.
    assert!(delete_thread(&client, &path)
        .await
        .expect("noop")
        .is_empty());

    // Retire de la Corbeille le fichier que ce test y a envoyé.
    if let Some(home) = std::env::var_os("HOME") {
        if let Ok(entries) = std::fs::read_dir(PathBuf::from(home).join(".Trash")) {
            for entry in entries.flatten() {
                if entry.file_name().to_string_lossy().starts_with(&stem) {
                    let _ = std::fs::remove_file(entry.path());
                }
            }
        }
    }

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

/// Réflexion : les `thinking_delta` puis `thinking_end` arrivent avant le
/// texte (pa-daemon/src/agent_engine/turn/run_once.rs:531-556), et le bloc
/// `thinking` revient dans l'historique à la réouverture.
#[tokio::test(flavor = "multi_thread")]
async fn thinking_streams_and_comes_back_with_the_thread() {
    use pa_tui::daemon_client::DaemonClientEvent;
    use sinew_desktop_lib::prime_session::{kill_session, open_thread, prompt, thread_path};

    let root = scratch_dir();
    let agent_dir = root.join("agent");
    let workspace = root.join("workspace");
    std::fs::create_dir_all(&workspace).unwrap();
    let script = root.join("faux.json");
    std::fs::write(
        &script,
        serde_json::json!({
            "engine": "faux",
            "reasoning": true,
            "responses": [{
                "content": [
                    { "type": "thinking", "thinking": "je pèse le pour et le contre" },
                    { "type": "text", "text": "voilà" },
                ],
            }],
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
    let config = serde_json::json!({
        "cwd": workspace.to_string_lossy(),
        "script": script.to_string_lossy(),
    });

    let (client, mut events) = ensure_daemon_running_with(&exe, &socket_path, &agent_dir)
        .await
        .expect("daemon starts");
    let path = thread_path(&agent_dir, "conv-think").unwrap();
    let opened = open_thread(&client, config.clone(), &path)
        .await
        .expect("thread opened");
    prompt(&client, &opened.active_session_id, "réfléchis")
        .await
        .expect("prompt admitted");

    let mut stream_kinds = Vec::new();
    let mut thinking = String::new();
    let mut seen_types = Vec::new();
    let turn = tokio::time::timeout(std::time::Duration::from_secs(60), async {
        while let Some(event) = events.recv().await {
            let DaemonClientEvent::SessionEvent { event, .. } = event else {
                continue;
            };
            let kind = event["type"].as_str().unwrap_or_default().to_string();
            if let Some(stream) = event["assistantMessageEvent"]["type"].as_str() {
                if stream_kinds.last().map(String::as_str) != Some(stream) {
                    stream_kinds.push(stream.to_string());
                }
                if stream == "thinking_delta" {
                    thinking.push_str(
                        event["assistantMessageEvent"]["delta"]
                            .as_str()
                            .unwrap_or_default(),
                    );
                }
            }
            let done = kind == "agent_end";
            seen_types.push(kind);
            if done {
                break;
            }
        }
    })
    .await;
    assert!(turn.is_ok(), "turn ended; events seen: {seen_types:?}");
    assert_eq!(thinking, "je pèse le pour et le contre");
    // `thinking_start` ne passe pas : la trame sans delta est remplacée par
    // le premier `thinking_delta` qui la suit (pa-daemon/src/streaming.rs:118-127).
    // Le client ouvre donc le bloc au premier delta.
    let position = |kind: &str| stream_kinds.iter().position(|seen| seen == kind);
    assert!(
        position("thinking_start").is_none(),
        "stream kinds: {stream_kinds:?}"
    );
    assert!(
        position("thinking_delta").is_some()
            && position("thinking_delta") < position("thinking_end")
            && position("thinking_end") < position("text_delta"),
        "stream kinds: {stream_kinds:?}"
    );

    kill_session(&client, &opened.active_session_id)
        .await
        .expect("killed");
    let reopened = open_thread(&client, config, &path)
        .await
        .expect("thread reopened");
    let blocks: Vec<(String, String)> = reopened
        .messages
        .iter()
        .filter(|message| message["role"] == "assistant")
        .flat_map(|message| message["content"].as_array().cloned().unwrap_or_default())
        .map(|block| {
            let kind = block["type"].as_str().unwrap_or_default().to_string();
            let text = block["thinking"]
                .as_str()
                .or_else(|| block["text"].as_str())
                .unwrap_or_default()
                .to_string();
            (kind, text)
        })
        .collect();
    assert_eq!(
        blocks,
        vec![
            (
                "thinking".to_string(),
                "je pèse le pour et le contre".to_string()
            ),
            ("text".to_string(), "voilà".to_string()),
        ]
    );

    kill_session(&client, &reopened.active_session_id)
        .await
        .expect("reopened killed");
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

/// Cycle `ClientOwned` (pa-daemon/src/supervisor/sessions.rs:621-638) : une
/// déconnexion sans `Kill` ne fait qu'un `detach`
/// (pa-daemon/src/supervisor/clients.rs:354-373) et le worker continue de
/// tourner. Ce test fige ce comportement : yusAi doit donc tuer lui-même
/// ses sessions.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn client_owned_worker_survives_disconnect() {
    use pa_types::daemon::{DaemonCommand, DaemonSessionLifecycle};

    let root = scratch_dir();
    let agent_dir = root.join("agent");
    let workspace = root.join("workspace");
    std::fs::create_dir_all(&workspace).unwrap();
    let script = root.join("faux.json");
    std::fs::write(
        &script,
        serde_json::json!({ "engine": "faux", "responses": [{ "text": "ok" }] }).to_string(),
    )
    .unwrap();
    let socket_path = root.join("daemon.sock");
    let exe = PathBuf::from(env!("CARGO_BIN_EXE_Sinew"));

    let (mut client, events) = ensure_daemon_running_with(&exe, &socket_path, &agent_dir)
        .await
        .expect("daemon starts");
    let summary = client
        .request_ok(DaemonCommand::Create {
            id: None,
            session_path: None,
            continue_recent: None,
            no_session: Some(true),
            name: None,
            config: Some(serde_json::json!({
                "cwd": workspace.to_string_lossy(),
                "script": script.to_string_lossy(),
            })),
            telemetry_disabled: Some(true),
            runtime_metadata: None,
            lifecycle: Some(DaemonSessionLifecycle::ClientOwned),
            env: None,
            launch_env: None,
            rest: serde_json::Map::default(),
        })
        .await
        .expect("client-owned session created");
    let session = summary["activeSessionId"].as_str().unwrap().to_string();

    // Le pid du worker, lu dans son descripteur.
    let descriptor_dir = pa_daemon::descriptor::descriptor_dir(&agent_dir, &socket_path);
    let worker_pid = std::fs::read_dir(&descriptor_dir)
        .expect("descriptor dir")
        .filter_map(Result::ok)
        .filter_map(|entry| std::fs::read_to_string(entry.path()).ok())
        .filter_map(|text| serde_json::from_str::<serde_json::Value>(&text).ok())
        .filter(|descriptor| descriptor["ownerClientId"].is_string())
        .find_map(|descriptor| descriptor["pid"].as_u64())
        .expect("client-owned worker descriptor with a pid");
    let alive = |pid: u64| {
        std::process::Command::new("kill")
            .args(["-0", &pid.to_string()])
            .status()
            .is_ok_and(|status| status.success())
    };
    assert!(alive(worker_pid), "worker {worker_pid} runs");

    // Coupure de la connexion, sans Kill.
    client.hard_close();
    drop(client);
    drop(events);
    tokio::time::sleep(std::time::Duration::from_secs(3)).await;
    assert!(
        alive(worker_pid),
        "client-owned worker {worker_pid} still runs after its client disconnected"
    );

    // Nettoyage : un nouveau client du même processus (même clientId) tue la
    // session puis arrête le daemon.
    let (cleanup, _events) = ensure_daemon_running_with(&exe, &socket_path, &agent_dir)
        .await
        .expect("daemon still up");
    cleanup
        .request_ok(DaemonCommand::Kill {
            id: None,
            active_session_id: session,
            rest: serde_json::Map::default(),
        })
        .await
        .expect("owned session killed by its owner id");
    let _ = cleanup
        .request_ok(DaemonCommand::Shutdown {
            id: None,
            force: Some(true),
            rest: serde_json::Map::default(),
        })
        .await;
    cleanup.close();
    let _ = std::fs::remove_dir_all(&root);
}

/// Nettoyage des workers orphelins : au démarrage, seules les sessions dont
/// l'IDE n'existe plus sont tuées ; à la sortie, l'IDE tue ses sessions et
/// n'arrête le daemon que s'il n'en reste aucune.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn orphaned_sessions_are_reaped_and_exit_stops_the_daemon() {
    use sinew_desktop_lib::prime_session::{
        close_ide_sessions, create_session_with_metadata, ide_runtime_metadata,
        reap_orphaned_sessions,
    };

    let root = scratch_dir();
    let agent_dir = root.join("agent");
    let workspace = root.join("workspace");
    std::fs::create_dir_all(&workspace).unwrap();
    let script = root.join("faux.json");
    std::fs::write(
        &script,
        serde_json::json!({ "engine": "faux", "responses": [{ "text": "ok" }] }).to_string(),
    )
    .unwrap();
    let config = serde_json::json!({
        "cwd": workspace.to_string_lossy(),
        "script": script.to_string_lossy(),
    });
    let socket_path = root.join("daemon.sock");
    let exe = PathBuf::from(env!("CARGO_BIN_EXE_Sinew"));
    let (client, _events) = ensure_daemon_running_with(&exe, &socket_path, &agent_dir)
        .await
        .expect("daemon starts");

    // Un IDE disparu : le pid d'un processus terminé.
    let mut gone = std::process::Command::new("true").spawn().unwrap();
    let gone_pid = gone.id();
    gone.wait().unwrap();
    let orphan = create_session_with_metadata(
        &client,
        config.clone(),
        serde_json::json!({ "yusai": { "idePid": gone_pid, "ideProcessStartId": "gone" } }),
    )
    .await
    .expect("orphan session");
    // Deux sessions d'IDE vivants (ce processus de test).
    let mine = create_session_with_metadata(&client, config.clone(), ide_runtime_metadata())
        .await
        .expect("own session");
    let other = create_session_with_metadata(&client, config.clone(), ide_runtime_metadata())
        .await
        .expect("other live session");

    let live_sessions = |client: &pa_tui::daemon_client::DaemonClient| {
        let client = client.clone();
        async move {
            let listed = client
                .request_ok(pa_types::daemon::DaemonCommand::List {
                    id: None,
                    all: None,
                    cwd: None,
                    session_dir: None,
                    include_client_owned: Some(true),
                    rest: serde_json::Map::default(),
                })
                .await
                .expect("list");
            listed["sessions"]
                .as_array()
                .unwrap()
                .iter()
                .filter_map(|row| row["activeSessionId"].as_str().map(str::to_string))
                .collect::<Vec<_>>()
        }
    };

    let reaped = reap_orphaned_sessions(&client, &agent_dir, &socket_path)
        .await
        .expect("reap");
    assert_eq!(
        reaped,
        vec![orphan.clone()],
        "only the dead IDE's session is reaped"
    );
    let live = live_sessions(&client).await;
    assert!(!live.contains(&orphan), "orphan killed: {live:?}");
    assert!(
        live.contains(&mine) && live.contains(&other),
        "live IDE sessions kept: {live:?}"
    );

    // Sortie avec une autre session encore ouverte : le daemon reste.
    let stopped = close_ide_sessions(&client, std::slice::from_ref(&mine))
        .await
        .expect("exit cleanup");
    assert!(!stopped, "a remaining session keeps the daemon up");
    assert_eq!(live_sessions(&client).await, vec![other.clone()]);

    // Dernière sortie : plus aucune session, le daemon s'arrête.
    let stopped = close_ide_sessions(&client, std::slice::from_ref(&other))
        .await
        .expect("exit cleanup");
    assert!(stopped, "no session left: the daemon shuts down");
    client.close();
    let mut down = false;
    for _ in 0..100 {
        if !pa_daemon::socket::can_connect(&socket_path, std::time::Duration::from_millis(100))
            .await
        {
            down = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    assert!(down, "the supervisor stopped listening");
    let _ = std::fs::remove_dir_all(&root);
}
