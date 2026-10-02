//! Binaire multi-rôle : l'exécutable lancé en `--mode daemon` sert un
//! superviseur Prime auquel `DaemonClient` se connecte.

use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use sinew_desktop_lib::prime::ensure_daemon_running_with;

/// Un dossier par test. Le compteur est indispensable : les tests démarrent
/// ensemble et l'horloge macOS est à la microseconde, deux tests pouvaient
/// tirer le même nom et partager dossier, socket et daemon (l'un arrêtait
/// le daemon ou effaçait le dossier de l'autre).
fn scratch_dir() -> PathBuf {
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.subsec_nanos());
    std::env::temp_dir().join(format!(
        "yusai-prime-test-{}-{}-{nanos}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ))
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
    // Miroir local coupé pour les sous-agents (`disable_telemetry_mirror`).
    let settings: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(agent_dir.join("settings.json")).unwrap())
            .unwrap();
    assert_eq!(settings["telemetry"]["localMirror"], false);

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
            // Les photos du tour précédent sont faites (elles tournent en
            // tâche de fond après `tool_execution_end`) : la modification à
            // la main tombe bien entre les deux tours.
            diffs.flush(&session).await;
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
    // Le dossier des sous-agents porte l'id de session de l'en-tête, pas le
    // nom du fichier (pa-daemon/src/rlm_children.rs:867-882).
    let header: serde_json::Value = serde_json::from_str(
        std::fs::read_to_string(&path)
            .unwrap()
            .lines()
            .next()
            .unwrap(),
    )
    .unwrap();
    let session_id = header["id"].as_str().unwrap().to_string();
    assert_ne!(session_id, stem);
    let children_dir = agent_dir.join("session-artifacts").join(&session_id);
    std::fs::create_dir_all(children_dir.join("sub-test")).unwrap();
    std::fs::write(
        children_dir.join("sub-test").join("rlm-subagent.json"),
        "{}",
    )
    .unwrap();

    let killed = delete_thread(&client, &path).await.expect("thread deleted");
    assert_eq!(killed, vec![opened.active_session_id.clone()]);
    assert!(!path.exists(), "the thread file is gone");
    assert!(!children_dir.exists(), "the sub-agents' artifacts are gone");
    let trash_cli = std::process::Command::new("trash")
        .arg("-h")
        .output()
        .is_ok();
    if let (true, Some(home)) = (trash_cli, std::env::var_os("HOME")) {
        let trashed = PathBuf::from(home).join(".Trash").join(&session_id);
        assert!(
            trashed.join("sub-test").join("rlm-subagent.json").is_file(),
            "the sub-agents' artifacts went to the Trash"
        );
        let _ = std::fs::remove_dir_all(trashed);
    }
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

/// Les fichiers d'état du harness sous `agent_dir`, en chemins relatifs triés.
fn harness_files(agent_dir: &std::path::Path) -> Vec<String> {
    fn walk(dir: &std::path::Path, root: &std::path::Path, found: &mut Vec<String>) {
        for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
            let path = entry.path();
            if path.is_dir() {
                walk(&path, root, found);
            } else if matches!(
                path.file_name().and_then(|name| name.to_str()),
                Some("harness_state.json" | "refinement_history.jsonl")
            ) {
                let relative = path.strip_prefix(root).unwrap_or(&path);
                found.push(relative.to_string_lossy().into_owned());
            }
        }
    }
    let mut found = Vec::new();
    walk(agent_dir, agent_dir, &mut found);
    found.sort();
    found
}

/// Attend la ligne `custom` `refinement_outcome` d'une refine
/// (pa-daemon/src/session_custom.rs:308-319, émise par
/// pa-daemon/src/worker/summary.rs:169-185).
async fn next_refinement_outcome(
    events: &mut tokio::sync::mpsc::UnboundedReceiver<pa_tui::daemon_client::DaemonClientEvent>,
) -> Option<serde_json::Value> {
    use pa_tui::daemon_client::DaemonClientEvent;
    tokio::time::timeout(std::time::Duration::from_secs(30), async {
        while let Some(event) = events.recv().await {
            let DaemonClientEvent::SessionEvent { event, .. } = event else {
                continue;
            };
            if event["type"] == "message_end"
                && event["message"]["customType"] == "refinement_outcome"
            {
                return Some(event["message"].clone());
            }
        }
        None
    })
    .await
    .ok()
    .flatten()
}

/// `/refine` par le daemon, planificateur scripté par le moteur `faux` (le
/// worker résout le même modèle faux pour la refine,
/// pa-daemon/src/agent_engine/session_engine_impl.rs:1282-1310, et
/// `complete_simple` consomme la réponse suivante du script).
///
/// Fige où le worker écrit aujourd'hui, à rebours de ce que relit le
/// digest du prompt (`session-artifacts/<fil>/harness/` en local,
/// `harness/` en global, pa-core/src/session_engine/engine.rs:363-377, 548) :
/// - local : `<dossier du fichier de fil>/harness/`, commun à tous les fils
///   (pa-core/src/session_engine/refine.rs:237-240,
///   pa-daemon/src/agent_engine/lifecycle.rs:1044-1063) ;
/// - global : `agent_dir` lui-même, sans `harness/`
///   (pa-daemon/src/agent_engine/session_engine_impl.rs:1289).
///
/// Vérifie aussi que `appendSystemPrompt` survit à la relance d'un worker
/// tué : le superviseur rejoue le `Create` durable, qui garde la clé
/// (pa-daemon/src/supervisor/worker_lifecycle.rs:235-252).
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn refine_runs_scripted_and_append_system_prompt_survives_a_worker_restart() {
    use pa_tui::daemon_client::DaemonClientEvent;
    use sinew_desktop_lib::prime_session::{kill_session, open_thread, prompt, thread_path};

    const MARKER: &str = "YUSAI-LESSON-MARKER: toujours lancer cargo test avant de commiter.";
    let root = scratch_dir();
    let agent_dir = root.join("agent");
    let workspace = root.join("workspace");
    std::fs::create_dir_all(&workspace).unwrap();
    let proposal = |summary: &str, id: &str| {
        serde_json::json!({
            "summary": summary,
            "rationale": "trajectory evidence",
            "expectedOutcome": "the lesson is reused",
            "edits": [{
                "action": "create",
                "kind": "memory",
                "id": id,
                "title": summary,
                "content": "Toujours lancer cargo test avant de commiter.",
            }],
        })
        .to_string()
    };
    let script = root.join("faux.json");
    std::fs::write(
        &script,
        serde_json::json!({
            "engine": "faux",
            "responses": [
                { "text": "ok" },
                { "text": proposal("local lesson", "yusai-local") },
                { "text": proposal("global lesson", "yusai-global") },
            ],
        })
        .to_string(),
    )
    .unwrap();
    let socket_path = root.join("daemon.sock");
    let exe = PathBuf::from(env!("CARGO_BIN_EXE_Sinew"));

    let (client, mut events) = ensure_daemon_running_with(&exe, &socket_path, &agent_dir)
        .await
        .expect("daemon starts");
    let path = thread_path(&agent_dir, "conv-refine").unwrap();
    let opened = open_thread(
        &client,
        serde_json::json!({
            "cwd": workspace.to_string_lossy(),
            "script": script.to_string_lossy(),
            "appendSystemPrompt": [MARKER],
        }),
        &path,
    )
    .await
    .expect("thread opened");
    let session = opened.active_session_id.clone();

    prompt(&client, &session, "salut")
        .await
        .expect("prompt admitted");
    let turn = tokio::time::timeout(std::time::Duration::from_secs(60), async {
        while let Some(event) = events.recv().await {
            if let DaemonClientEvent::SessionEvent { event, .. } = event {
                if event["type"] == "agent_end" {
                    return;
                }
            }
        }
    })
    .await;
    assert!(turn.is_ok(), "turn ended");

    async fn system_prompt(
        client: &pa_tui::daemon_client::DaemonClient,
        active_session_id: &str,
    ) -> String {
        client
            .request_ok(pa_types::daemon::DaemonCommand::GetSystemPrompt {
                id: None,
                active_session_id: active_session_id.to_string(),
                rest: serde_json::Map::default(),
            })
            .await
            .expect("get_system_prompt")["systemPrompt"]
            .as_str()
            .unwrap_or_default()
            .to_string()
    }
    assert!(
        system_prompt(&client, &session).await.contains(MARKER),
        "appendSystemPrompt reaches the system prompt"
    );

    // Refine locale.
    let local = client
        .request_ok(pa_types::daemon::DaemonCommand::Refine {
            id: None,
            active_session_id: session.clone(),
            instructions: None,
            rollback_id: None,
            global: None,
            rest: serde_json::Map::default(),
        })
        .await
        .expect("local refine runs");
    assert_eq!(local["summary"], "local lesson", "refine result: {local}");
    assert_eq!(local["scope"], "local");
    assert_eq!(
        local["appliedEdits"][0]["applied"], true,
        "refine result: {local}"
    );
    let outcome = next_refinement_outcome(&mut events)
        .await
        .expect("refinement_outcome row");
    assert_eq!(outcome["details"]["refinementId"], local["id"]);
    assert_eq!(outcome["details"]["scope"], "local");
    assert_eq!(
        harness_files(&agent_dir),
        vec!["yusai-threads/harness/harness_state.json".to_string()],
        "local refine lands next to the thread files, not in session-artifacts/conv-refine/"
    );

    // Refine globale.
    let global = client
        .request_ok(pa_types::daemon::DaemonCommand::Refine {
            id: None,
            active_session_id: session.clone(),
            instructions: None,
            rollback_id: None,
            global: Some(true),
            rest: serde_json::Map::default(),
        })
        .await
        .expect("global refine runs");
    assert_eq!(global["scope"], "global", "refine result: {global}");
    let outcome = next_refinement_outcome(&mut events)
        .await
        .expect("refinement_outcome row");
    assert_eq!(outcome["details"]["scope"], "global");
    assert_eq!(
        harness_files(&agent_dir),
        vec![
            "harness_state.json".to_string(),
            "refinement_history.jsonl".to_string(),
            "yusai-threads/harness/harness_state.json".to_string(),
        ],
        "global refine lands in agent_dir itself, not in agent_dir/harness/"
    );
    let global_state = std::fs::read_to_string(agent_dir.join("harness_state.json")).unwrap();
    assert!(global_state.contains("yusai-global"));

    // Le fil ne garde que les lignes affichées (`refinement_outcome`) et
    // destinées au modèle (`refinement_notice`), que le worker écrit
    // lui-même (pa-daemon/src/worker/summary.rs:169-185). L'audit
    // `prime-agent.refinement`, base du retour arrière, reste dans la
    // session en mémoire du moteur (pa-core/src/session_engine/refine.rs:400-404) :
    // l'historique local meurt avec le worker ; le global survit dans
    // `refinement_history.jsonl`.
    let thread = std::fs::read_to_string(&path).unwrap();
    assert_eq!(thread.matches("\"refinement_outcome\"").count(), 2);
    assert_eq!(thread.matches("\"refinement_notice\"").count(), 2);
    assert_eq!(thread.matches("\"prime-agent.refinement\"").count(), 0);

    // Worker tué : le superviseur le relance avec le `Create` durable.
    let descriptor_dir = pa_daemon::descriptor::descriptor_dir(&agent_dir, &socket_path);
    let descriptor_of = || {
        std::fs::read_dir(&descriptor_dir)
            .into_iter()
            .flatten()
            .flatten()
            .filter_map(|entry| std::fs::read_to_string(entry.path()).ok())
            .filter_map(|text| {
                serde_json::from_str::<pa_types::daemon::DaemonWorkerDescriptor>(&text).ok()
            })
            .find(|descriptor| descriptor.root_active_session_id == session)
    };
    let before = descriptor_of().expect("worker descriptor");
    assert_eq!(
        before.create_command.rest.get("appendSystemPrompt"),
        Some(&serde_json::json!([MARKER])),
        "the durable create keeps appendSystemPrompt"
    );
    let killed = std::process::Command::new("kill")
        .args(["-9", &before.pid.to_string()])
        .status()
        .expect("kill runs");
    assert!(killed.success());
    let mut relaunched = None;
    for _ in 0..200 {
        if let Some(descriptor) = descriptor_of() {
            if descriptor.pid != before.pid
                && descriptor.lifecycle == pa_types::daemon::DaemonWorkerLifecycle::Ready
            {
                relaunched = Some(descriptor);
                break;
            }
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    assert!(relaunched.is_some(), "the supervisor relaunches the worker");
    assert!(
        system_prompt(&client, &session).await.contains(MARKER),
        "appendSystemPrompt is replayed after the worker restart"
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

/// Le contenu d'un fichier d'état du harness (vide s'il manque).
fn harness_entry_ids(path: &std::path::Path, kind: &str) -> Vec<String> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    let state: serde_json::Value = serde_json::from_str(&text).unwrap();
    let mut ids: Vec<String> = state["entries"][kind]
        .as_object()
        .map(|entries| entries.keys().cloned().collect())
        .unwrap_or_default();
    ids.sort();
    ids
}

/// Les refines deviennent des leçons yusAi : capture d'une
/// `refinement_outcome` réelle (planificateur scripté), import unique par
/// `refinementId`, retrait du harness des entrées importées, proposition de
/// montée pour une refine globale, rattrapage depuis l'historique du fil.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn refinements_become_lessons_once_and_leave_the_harness() {
    use pa_tui::daemon_client::DaemonClientEvent;
    use sinew_app::store::{AppStore, LessonLevel, LessonScope, ProposalKind};
    use sinew_desktop_lib::prime_lessons::{
        import_refinement_outcome, import_thread_outcomes, refine_harness_file, ThreadContext,
    };
    use sinew_desktop_lib::prime_session::{kill_session, open_thread, prompt, thread_path};

    let root = scratch_dir();
    let agent_dir = root.join("agent");
    let workspace = root.join("workspace");
    std::fs::create_dir_all(&workspace).unwrap();
    let local = serde_json::json!({
        "summary": "local lessons",
        "rationale": "trajectory evidence",
        "expectedOutcome": "reused",
        "edits": [
            {
                "action": "create", "kind": "memory", "id": "yusai-local",
                "title": "Tests", "content": "Toujours lancer cargo test avant de commiter.",
            },
            {
                "action": "create", "kind": "skill", "id": "fmt-skill",
                "title": "Format", "content": "Formater le code avec cargo fmt.",
                "reference": { "type": "python", "import": "fmt_skill", "callable": "run" },
                "arguments": {},
            },
        ],
    });
    let global = serde_json::json!({
        "summary": "global lesson",
        "rationale": "trajectory evidence",
        "expectedOutcome": "reused",
        "edits": [{
            "action": "create", "kind": "memory", "id": "yusai-global",
            "title": "Langue", "content": "Répondre en français.",
        }],
    });
    let script = root.join("faux.json");
    std::fs::write(
        &script,
        serde_json::json!({
            "engine": "faux",
            "responses": [
                { "text": "ok" },
                { "text": local.to_string() },
                { "text": global.to_string() },
            ],
        })
        .to_string(),
    )
    .unwrap();
    let socket_path = root.join("daemon.sock");
    let exe = PathBuf::from(env!("CARGO_BIN_EXE_Sinew"));
    let (client, mut events) = ensure_daemon_running_with(&exe, &socket_path, &agent_dir)
        .await
        .expect("daemon starts");
    let path = thread_path(&agent_dir, "conv-lessons").unwrap();
    let config = serde_json::json!({
        "cwd": workspace.to_string_lossy(),
        "script": script.to_string_lossy(),
    });
    let opened = open_thread(&client, config.clone(), &path)
        .await
        .expect("thread opened");
    let session = opened.active_session_id.clone();
    prompt(&client, &session, "salut")
        .await
        .expect("prompt admitted");
    let turn = tokio::time::timeout(std::time::Duration::from_secs(60), async {
        while let Some(event) = events.recv().await {
            if let DaemonClientEvent::SessionEvent { event, .. } = event {
                if event["type"] == "agent_end" {
                    return;
                }
            }
        }
    })
    .await;
    assert!(turn.is_ok(), "turn ended");

    let store = AppStore::open_at(root.join("desktop-state.sqlite3")).unwrap();
    let thread = ThreadContext {
        conversation_id: "conv-lessons".to_string(),
        workspace_id: workspace.to_string_lossy().into_owned(),
    };
    let refine = |global: bool| {
        client.request_ok(pa_types::daemon::DaemonCommand::Refine {
            id: None,
            active_session_id: session.clone(),
            instructions: None,
            rollback_id: None,
            global: global.then_some(true),
            rest: serde_json::Map::default(),
        })
    };

    // Refine locale : une leçon projet, une proposition de skill.
    refine(false).await.expect("local refine runs");
    let outcome = next_refinement_outcome(&mut events)
        .await
        .expect("refinement_outcome row");
    let local_file = refine_harness_file(&agent_dir, false);
    assert_eq!(
        harness_entry_ids(&local_file, "memory"),
        vec!["yusai-local"]
    );
    let report = import_refinement_outcome(&store, &agent_dir, &thread, &outcome["details"])
        .unwrap()
        .expect("first import");
    assert_eq!(report.created.len(), 1, "report: {report:?}");
    assert_eq!(report.proposals.len(), 1, "report: {report:?}");
    assert_eq!(report.removed_entries, 1, "report: {report:?}");
    let scope = LessonScope {
        workspace_id: thread.workspace_id.clone(),
        project_type: None,
    };
    let lessons = store.applicable_lessons(&scope).unwrap();
    assert_eq!(lessons.len(), 1);
    assert_eq!(lessons[0].level, LessonLevel::Project);
    assert_eq!(
        lessons[0].content,
        "Toujours lancer cargo test avant de commiter."
    );
    let history = store.lesson_events(&lessons[0].id).unwrap();
    assert_eq!(history[0].conversation_id.as_deref(), Some("conv-lessons"));
    assert_eq!(
        history[0].refinement_id.as_deref(),
        outcome["details"]["refinementId"].as_str()
    );
    // L'entrée importée quitte le harness partagé ; la skill (proposition)
    // y reste.
    assert!(harness_entry_ids(&local_file, "memory").is_empty());
    assert_eq!(harness_entry_ids(&local_file, "skill"), vec!["fmt-skill"]);
    // Une seule fois par refine.
    assert_eq!(
        import_refinement_outcome(&store, &agent_dir, &thread, &outcome["details"]).unwrap(),
        None
    );

    // Refine globale (comme `refine.run(global_=True)`) : niveau projet +
    // proposition de montée.
    refine(true).await.expect("global refine runs");
    let outcome = next_refinement_outcome(&mut events)
        .await
        .expect("refinement_outcome row");
    let global_file = refine_harness_file(&agent_dir, true);
    assert_eq!(
        harness_entry_ids(&global_file, "memory"),
        vec!["yusai-global"]
    );
    let report = import_refinement_outcome(&store, &agent_dir, &thread, &outcome["details"])
        .unwrap()
        .expect("global import");
    assert_eq!(report.created.len(), 1, "report: {report:?}");
    assert_eq!(report.removed_entries, 1, "report: {report:?}");
    assert!(harness_entry_ids(&global_file, "memory").is_empty());
    let proposals = store.pending_lesson_proposals().unwrap();
    let kinds: Vec<ProposalKind> = proposals.iter().map(|proposal| proposal.kind).collect();
    assert_eq!(kinds, vec![ProposalKind::Skill, ProposalKind::Promote]);
    assert_eq!(proposals[1].lesson_id.as_ref(), Some(&report.created[0]));
    assert_eq!(proposals[1].target_level, Some(LessonLevel::Global));
    assert_eq!(
        store.lesson(&report.created[0]).unwrap().unwrap().level,
        LessonLevel::Project
    );

    // Rattrapage : l'historique rouvert porte les deux refines ; déjà
    // importées ici, importées une fois dans un magasin neuf.
    kill_session(&client, &session)
        .await
        .expect("session killed");
    let reopened = open_thread(&client, config, &path)
        .await
        .expect("thread reopened");
    assert!(import_thread_outcomes(&store, &agent_dir, &thread, &reopened.messages).is_empty());
    let fresh = AppStore::open_at(root.join("fresh.sqlite3")).unwrap();
    let caught_up = import_thread_outcomes(&fresh, &agent_dir, &thread, &reopened.messages);
    assert_eq!(caught_up.len(), 2, "messages: {:?}", reopened.messages);
    assert_eq!(fresh.applicable_lessons(&scope).unwrap().len(), 2);
    assert!(import_thread_outcomes(&fresh, &agent_dir, &thread, &reopened.messages).is_empty());

    kill_session(&client, &reopened.active_session_id)
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

/// Garde globale, avec un vrai noyau (sauté sans `uv`) : une cellule
/// `rlm.harness.create_memory(…, global_=True)` écrit dans
/// `<agent_dir>/harness/harness_state.json`, que Prime réinjecte dans toutes
/// les sessions (pa-core/src/session_engine/engine.rs:548). La garde en fait
/// une leçon de la conversation, proposée pour le global, et la retire du
/// fichier.
#[tokio::test(flavor = "multi_thread")]
async fn model_global_harness_writes_become_proposed_project_lessons() {
    use pa_tui::daemon_client::DaemonClientEvent;
    use sinew_app::store::{AppStore, LessonLevel, LessonScope, ProposalKind};
    use sinew_desktop_lib::prime::ensure_daemon_running_with_kernel_venv;
    use sinew_desktop_lib::prime_lessons::{
        global_harness_file, import_global_harness_writes, ThreadContext,
    };
    use sinew_desktop_lib::prime_session::{kill_session, open_thread, prompt, thread_path};

    if !uv_available() {
        eprintln!("uv not found; skipping the live-kernel global harness e2e");
        return;
    }
    let kernel_venv = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("prime-kernel-venv");
    let root = scratch_dir();
    let agent_dir = root.join("agent");
    let workspace = root.join("workspace");
    std::fs::create_dir_all(&workspace).unwrap();
    let script = root.join("faux.json");
    std::fs::write(
        &script,
        serde_json::json!({ "engine": "faux", "responses": [
            { "content": [{
                "type": "toolCall",
                "name": "ipython",
                "arguments": { "code": "rlm.harness.create_memory(\"Langue\", \"Répondre en français.\", global_=True)" },
            }] },
            { "text": "noté" },
        ] })
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
    let (client, mut events) =
        ensure_daemon_running_with_kernel_venv(&exe, &socket_path, &agent_dir, &kernel_venv)
            .await
            .expect("daemon starts");
    let opened = open_thread(
        &client,
        serde_json::json!({
            "cwd": workspace.to_string_lossy(),
            "script": script.to_string_lossy(),
        }),
        &thread_path(&agent_dir, "conv-global").unwrap(),
    )
    .await
    .expect("thread opened");
    let session = opened.active_session_id.clone();
    prompt(&client, &session, "retiens que je parle français")
        .await
        .expect("prompt admitted");
    let mut cell_ended = false;
    // La première construction du venv peut prendre quelques minutes.
    let turn = tokio::time::timeout(std::time::Duration::from_secs(600), async {
        while let Some(event) = events.recv().await {
            if let DaemonClientEvent::SessionEvent { event, .. } = event {
                if event["type"] == "tool_execution_end" {
                    assert_ne!(event["isError"], true, "cell failed: {event}");
                    cell_ended = true;
                }
                if event["type"] == "agent_end" {
                    return;
                }
            }
        }
    })
    .await;
    assert!(
        turn.is_ok() && cell_ended,
        "the cell ran and the turn ended"
    );

    let file = global_harness_file(&agent_dir);
    let written: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&file).expect("global harness written"))
            .unwrap();
    assert_eq!(
        written["entries"]["memory"]
            .as_object()
            .map(|entries| entries.len()),
        Some(1),
        "global harness: {written}"
    );

    let store = AppStore::open_at(root.join("desktop-state.sqlite3")).unwrap();
    let thread = ThreadContext {
        conversation_id: "conv-global".to_string(),
        workspace_id: workspace.to_string_lossy().into_owned(),
    };
    let report = import_global_harness_writes(&store, &agent_dir, &thread)
        .unwrap()
        .expect("global write imported");
    assert_eq!(report.created.len(), 1, "report: {report:?}");
    assert_eq!(report.removed_entries, 1, "report: {report:?}");
    let lessons = store
        .applicable_lessons(&LessonScope {
            workspace_id: thread.workspace_id.clone(),
            project_type: None,
        })
        .unwrap();
    assert_eq!(lessons.len(), 1);
    assert_eq!(lessons[0].level, LessonLevel::Project);
    assert_eq!(lessons[0].content, "Répondre en français.");
    let proposals = store.pending_lesson_proposals().unwrap();
    assert_eq!(proposals.len(), 1);
    assert_eq!(proposals[0].kind, ProposalKind::Promote);
    assert_eq!(proposals[0].target_level, Some(LessonLevel::Global));
    assert_eq!(proposals[0].lesson_id.as_ref(), Some(&lessons[0].id));
    let left: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&file).unwrap()).unwrap();
    assert!(left["entries"]["memory"].as_object().unwrap().is_empty());
    assert_eq!(
        import_global_harness_writes(&store, &agent_dir, &thread).unwrap(),
        None
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

/// La file des refines est commune au processus : les tests qui lancent
/// des refines (et mesurent leur durée) passent l'un après l'autre.
static REFINE_TESTS: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// Un fil scripté prêt pour des refines : daemon lancé, premier tour fini
/// (première réponse du script), puis les réponses données, dans l'ordre
/// des appels au modèle (planificateur de refine ou tour). Le relais de
/// l'app n'existe pas ici : la file importe elle-même.
#[cfg(unix)]
async fn scripted_thread_with_events(
    root: &std::path::Path,
    workspace: &std::path::Path,
    refine_plans: &[(serde_json::Value, u64)],
) -> (
    pa_tui::daemon_client::DaemonClient,
    String,
    tokio::sync::mpsc::UnboundedReceiver<pa_tui::daemon_client::DaemonClientEvent>,
) {
    use pa_tui::daemon_client::DaemonClientEvent;
    use sinew_desktop_lib::prime_session::{open_thread, prompt, thread_path};

    let mut responses = vec![serde_json::json!({ "text": "ok" })];
    responses.extend(refine_plans.iter().map(
        |(plan, delay_ms)| serde_json::json!({ "text": plan.to_string(), "delayMs": delay_ms }),
    ));
    let script = root.join("faux.json");
    std::fs::write(
        &script,
        serde_json::json!({ "engine": "faux", "responses": responses }).to_string(),
    )
    .unwrap();
    let agent_dir = root.join("agent");
    let exe = PathBuf::from(env!("CARGO_BIN_EXE_Sinew"));
    let (client, mut events) =
        ensure_daemon_running_with(&exe, &root.join("daemon.sock"), &agent_dir)
            .await
            .expect("daemon starts");
    let path = thread_path(&agent_dir, "conv-queue").unwrap();
    let config = serde_json::json!({
        "cwd": workspace.to_string_lossy(),
        "script": script.to_string_lossy(),
    });
    let opened = open_thread(&client, config, &path)
        .await
        .expect("thread opened");
    let session = opened.active_session_id.clone();
    prompt(&client, &session, "salut")
        .await
        .expect("prompt admitted");
    let turn = tokio::time::timeout(std::time::Duration::from_secs(60), async {
        while let Some(event) = events.recv().await {
            if let DaemonClientEvent::SessionEvent { event, .. } = event {
                if event["type"] == "agent_end" {
                    return;
                }
            }
        }
    })
    .await;
    assert!(turn.is_ok(), "turn ended");
    (client, session, events)
}

/// [`scripted_thread_with_events`], événements vidés en tâche de fond.
#[cfg(unix)]
async fn scripted_thread(
    root: &std::path::Path,
    workspace: &std::path::Path,
    responses: &[(serde_json::Value, u64)],
) -> (pa_tui::daemon_client::DaemonClient, String) {
    let (client, session, mut events) =
        scripted_thread_with_events(root, workspace, responses).await;
    tokio::spawn(async move { while events.recv().await.is_some() {} });
    (client, session)
}

/// Arrête le daemon d'un fil scripté et supprime son dossier, une fois
/// sortis le daemon et ses workers (qui écrivent encore en s'arrêtant).
#[cfg(unix)]
async fn stop_scripted_thread(
    client: pa_tui::daemon_client::DaemonClient,
    session: &str,
    root: &std::path::Path,
) {
    sinew_desktop_lib::prime_session::kill_session(&client, session)
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
    for _ in 0..100 {
        let running = std::process::Command::new("pgrep")
            .args(["-f", &root.to_string_lossy()])
            .output()
            .is_ok_and(|output| !output.stdout.is_empty());
        if !running {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    let _ = std::fs::remove_dir_all(root);
}

/// File des refines : nos leçons sont amorcées dans le harness local, le
/// planificateur peut donc les modifier (une edit sur une entrée absente
/// échoue) ; deux refines lancées ensemble passent l'une après l'autre (la
/// seconde voit la leçon créée par la première) ; à la fin, il ne reste
/// dans le harness que l'entrée qui n'est pas à nous.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn queued_refines_see_our_lessons_and_run_one_at_a_time() {
    let _alone = REFINE_TESTS.lock().await;
    use sinew_app::store::{
        AppStore, InsertLessonOutcome, LessonKind, LessonLevel, LessonOrigin, LessonScope,
        NewLesson, ProposalKind,
    };
    use sinew_desktop_lib::prime_lessons::{refine_harness_file, ThreadContext};
    use sinew_desktop_lib::prime_refine::{run_refine, RefineOrigin};

    let root = scratch_dir();
    let agent_dir = root.join("agent");
    let workspace = root.join("workspace");
    std::fs::create_dir_all(&workspace).unwrap();
    let workspace_id = workspace.to_string_lossy().into_owned();
    let store = AppStore::open_at(root.join("desktop-state.sqlite3")).unwrap();
    let insert = |level: LessonLevel, content: &str| {
        let new = NewLesson {
            level,
            workspace_id: Some(workspace_id.clone()),
            project_type: None,
            kind: LessonKind::Memory,
            title: content.to_string(),
            content: content.to_string(),
        };
        match store.insert_lesson(&new, &LessonOrigin::user()).unwrap() {
            InsertLessonOutcome::Created(lesson) => lesson,
            other => panic!("not created: {other:?}"),
        }
    };
    let project = insert(LessonLevel::Project, "Lancer cargo test.");
    let global = insert(LessonLevel::Global, "Répondre en anglais.");
    // Une entrée de Prime qui n'est pas à nous : elle doit rester.
    let local_file = refine_harness_file(&agent_dir, false);
    std::fs::create_dir_all(local_file.parent().unwrap()).unwrap();
    std::fs::write(
        &local_file,
        serde_json::json!({
            "schema": 1,
            "entries": { "memory": { "theirs": {
                "id": "theirs", "kind": "memory", "title": "Theirs",
                "content": "Une entrée de Prime.", "path": "general",
                "reference": {}, "arguments": {}, "metadata": {},
                "source": "refine", "created_at": "2026-10-02T00:00:00.000Z",
                "updated_at": "2026-10-02T00:00:00.000Z", "version": 1,
            } } },
            "refinements": [],
        })
        .to_string(),
    )
    .unwrap();

    let first = serde_json::json!({
        "summary": "first",
        "rationale": "trajectory evidence",
        "expectedOutcome": "reused",
        "edits": [
            { "action": "update", "kind": "memory", "id": project.id,
              "title": "Tests", "content": "Lancer cargo test --workspace." },
            { "action": "delete", "kind": "memory", "id": global.id },
            { "action": "create", "kind": "memory", "id": "yusai-new",
              "title": "Format", "content": "Formater avec cargo fmt." },
        ],
    });
    let second = serde_json::json!({
        "summary": "second",
        "rationale": "trajectory evidence",
        "expectedOutcome": "reused",
        "edits": [
            { "action": "update", "kind": "memory", "id": project.id,
              "title": "Tests", "content": "Lancer cargo nextest." },
        ],
    });
    // La première refine traîne : sans la file, la seconde s'amorcerait et
    // planifierait pendant ce temps.
    let (client, session) =
        scripted_thread(&root, &workspace, &[(first, 2_000), (second, 0)]).await;
    let socket_path = root.join("daemon.sock");
    let thread = ThreadContext {
        conversation_id: "conv-queue".to_string(),
        workspace_id: workspace_id.clone(),
    };
    store.note_user_turn("conv-queue").unwrap();

    let (one, two) = tokio::join!(
        run_refine(
            &socket_path,
            store.clone(),
            agent_dir.clone(),
            &session,
            thread.clone(),
            RefineOrigin::Retain,
            None
        ),
        async {
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            run_refine(
                &socket_path,
                store.clone(),
                agent_dir.clone(),
                &session,
                thread.clone(),
                RefineOrigin::Retain,
                None,
            )
            .await
        },
    );
    let one = one.expect("first refine");
    let two = two.expect("second refine");

    assert_eq!(one.seeded, 2, "run: {one:?}");
    let report = one.report.expect("first import");
    assert_eq!(
        report.updated,
        vec![project.id.clone()],
        "report: {report:?}"
    );
    assert_eq!(report.created.len(), 1, "report: {report:?}");
    assert_eq!(report.proposals.len(), 1, "report: {report:?}");
    assert!(report.failed.is_empty(), "report: {report:?}");
    // La seconde s'est amorcée après l'import de la première : elle voit
    // la leçon créée, et sa mise à jour trouve l'entrée.
    assert_eq!(two.seeded, 3, "run: {two:?}");
    let report = two.report.expect("second import");
    assert_eq!(
        report.updated,
        vec![project.id.clone()],
        "report: {report:?}"
    );

    assert_eq!(
        store.lesson(&project.id).unwrap().unwrap().content,
        "Lancer cargo nextest."
    );
    let proposals = store.pending_lesson_proposals().unwrap();
    assert_eq!(proposals.len(), 1);
    assert_eq!(proposals[0].kind, ProposalKind::Archive);
    assert_eq!(proposals[0].lesson_id.as_ref(), Some(&global.id));
    let scope = LessonScope {
        workspace_id,
        project_type: None,
    };
    assert_eq!(store.applicable_lessons(&scope).unwrap().len(), 3);
    // Ni nos leçons ni l'entrée importée ne restent dans le harness.
    assert_eq!(harness_entry_ids(&local_file, "memory"), vec!["theirs"]);
    let state = store.refine_state("conv-queue").unwrap();
    assert_eq!(state.user_turns_since_refine, 0);
    assert!(state.last_refined_at_ms.is_some());
    assert!(!state.pending);

    stop_scripted_thread(client, &session, &root).await;
}

/// Une leçon de projet, pour vérifier qu'elle quitte le harness après une
/// refine ratée.
#[cfg(unix)]
fn project_lesson(store: &sinew_app::store::AppStore, workspace: &std::path::Path) {
    use sinew_app::store::{InsertLessonOutcome, LessonKind, LessonLevel, LessonOrigin, NewLesson};
    let new = NewLesson {
        level: LessonLevel::Project,
        workspace_id: Some(workspace.to_string_lossy().into_owned()),
        project_type: None,
        kind: LessonKind::Memory,
        title: "Tests".to_string(),
        content: "Lancer cargo test.".to_string(),
    };
    assert!(matches!(
        store.insert_lesson(&new, &LessonOrigin::user()).unwrap(),
        InsertLessonOutcome::Created(_)
    ));
}

/// Le superviseur coupe une requête `Refine` au bout de 30 s
/// (pa-daemon/src/supervisor/routing.rs:648-661) alors que le worker
/// poursuit : la file parle directement au worker et reçoit la vraie
/// réponse, réussie ou non, sans attendre de limite.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn refines_longer_than_the_supervisor_route_get_their_answer() {
    let _alone = REFINE_TESTS.lock().await;
    use sinew_app::store::{AppStore, LessonScope};
    use sinew_desktop_lib::prime_lessons::{refine_harness_file, ThreadContext};
    use sinew_desktop_lib::prime_refine::{run_refine, RefineOrigin};

    let root = scratch_dir();
    let agent_dir = root.join("agent");
    let workspace = root.join("workspace");
    std::fs::create_dir_all(&workspace).unwrap();
    let store = AppStore::open_at(root.join("desktop-state.sqlite3")).unwrap();
    project_lesson(&store, &workspace);
    let slow = serde_json::json!({
        "summary": "slow",
        "rationale": "trajectory evidence",
        "expectedOutcome": "reused",
        "edits": [{ "action": "create", "kind": "memory", "id": "yusai-slow",
                    "title": "Lent", "content": "Une refine qui prend son temps." }],
    });
    // Après la refine lente, une refine dont le plan est illisible : elle
    // échoue après plus de 30 s.
    let broken = serde_json::json!("pas un plan");
    let (client, session) =
        scripted_thread(&root, &workspace, &[(slow, 33_000), (broken, 31_000)]).await;
    let socket_path = root.join("daemon.sock");
    let thread = ThreadContext {
        conversation_id: "conv-queue".to_string(),
        workspace_id: workspace.to_string_lossy().into_owned(),
    };
    let refine = || {
        run_refine(
            &socket_path,
            store.clone(),
            agent_dir.clone(),
            &session,
            thread.clone(),
            RefineOrigin::Retain,
            None,
        )
    };

    let started = std::time::Instant::now();
    let run = refine().await.expect("refine lands after 30 s");
    assert!(
        started.elapsed() >= std::time::Duration::from_secs(33),
        "the slow plan was used"
    );
    let report = run.report.expect("imported by the queue");
    assert_eq!(report.created.len(), 1, "report: {report:?}");
    let scope = LessonScope {
        workspace_id: thread.workspace_id.clone(),
        project_type: None,
    };
    assert!(store
        .applicable_lessons(&scope)
        .unwrap()
        .iter()
        .any(|lesson| lesson.content == "Une refine qui prend son temps."));
    let local_file = refine_harness_file(&agent_dir, false);
    assert!(harness_entry_ids(&local_file, "memory").is_empty());
    let refined_at = store.refine_state("conv-queue").unwrap().last_refined_at_ms;
    assert!(refined_at.is_some());

    let started = std::time::Instant::now();
    let error = refine().await.expect_err("an unreadable plan fails");
    let elapsed = started.elapsed();
    assert!(
        elapsed >= std::time::Duration::from_secs(31)
            && elapsed < std::time::Duration::from_secs(60),
        "the failure arrives with the refine, not at the 10 min limit: {elapsed:?}"
    );
    assert!(
        !error.to_string().contains("timed out"),
        "Prime's own failure: {error:#}"
    );
    assert!(harness_entry_ids(&local_file, "memory").is_empty());
    assert_eq!(
        store.refine_state("conv-queue").unwrap().last_refined_at_ms,
        refined_at
    );

    stop_scripted_thread(client, &session, &root).await;
}

/// Un worker tué pendant la refine (le superviseur le relance aussitôt,
/// sans la refine) libère la file tout de suite.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn a_worker_killed_during_a_refine_frees_the_queue_at_once() {
    let _alone = REFINE_TESTS.lock().await;
    use sinew_app::store::AppStore;
    use sinew_desktop_lib::prime_lessons::{refine_harness_file, ThreadContext};
    use sinew_desktop_lib::prime_refine::{run_refine, RefineOrigin};

    let root = scratch_dir();
    let agent_dir = root.join("agent");
    let workspace = root.join("workspace");
    std::fs::create_dir_all(&workspace).unwrap();
    let store = AppStore::open_at(root.join("desktop-state.sqlite3")).unwrap();
    project_lesson(&store, &workspace);
    let plan = serde_json::json!({
        "summary": "never",
        "rationale": "trajectory evidence",
        "expectedOutcome": "reused",
        "edits": [],
    });
    let (client, session) = scripted_thread(&root, &workspace, &[(plan, 60_000)]).await;
    let socket_path = root.join("daemon.sock");
    let thread = ThreadContext {
        conversation_id: "conv-queue".to_string(),
        workspace_id: workspace.to_string_lossy().into_owned(),
    };
    let descriptor_dir = pa_daemon::descriptor::descriptor_dir(&agent_dir, &socket_path);
    let worker_pid = std::fs::read_dir(&descriptor_dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|entry| std::fs::read_to_string(entry.path()).ok())
        .filter_map(|text| {
            serde_json::from_str::<pa_types::daemon::DaemonWorkerDescriptor>(&text).ok()
        })
        .find(|descriptor| descriptor.root_active_session_id == session)
        .expect("worker descriptor")
        .pid;
    let local_file = refine_harness_file(&agent_dir, false);

    let refine = tokio::spawn({
        let (socket_path, store, agent_dir, session) = (
            socket_path.clone(),
            store.clone(),
            agent_dir.clone(),
            session.clone(),
        );
        async move {
            run_refine(
                &socket_path,
                store,
                agent_dir,
                &session,
                thread,
                RefineOrigin::Retain,
                None,
            )
            .await
        }
    });
    // La refine est partie : nos leçons sont dans le harness.
    let mut seeded = false;
    for _ in 0..100 {
        if !harness_entry_ids(&local_file, "memory").is_empty() {
            seeded = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    assert!(seeded, "lessons seeded");
    tokio::time::sleep(std::time::Duration::from_secs(1)).await;
    let killed = std::process::Command::new("kill")
        .args(["-9", &worker_pid.to_string()])
        .status()
        .expect("kill runs");
    assert!(killed.success());
    let killed_at = std::time::Instant::now();

    let outcome = tokio::time::timeout(std::time::Duration::from_secs(20), refine)
        .await
        .expect("the queue is freed long before the plan's 60 s")
        .unwrap();
    assert!(outcome.is_err(), "outcome: {outcome:?}");
    assert!(killed_at.elapsed() < std::time::Duration::from_secs(20));
    assert!(harness_entry_ids(&local_file, "memory").is_empty());
    assert!(store
        .refine_state("conv-queue")
        .unwrap()
        .last_refined_at_ms
        .is_none());

    stop_scripted_thread(client, &session, &root).await;
}

/// Les leçons du magasin arrivent dans le prompt système par
/// `appendSystemPrompt` : consignes, une puce par leçon, puce de
/// dépassement au-delà de 4 000 caractères. Le texte est figé au `Create` :
/// une leçon ajoutée ensuite n'arrive qu'à la réouverture du fil.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn lessons_reach_the_system_prompt_when_a_thread_opens() {
    use sinew_app::store::{
        AppStore, InsertLessonOutcome, LessonKind, LessonLevel, LessonOrigin, NewLesson,
    };
    use sinew_desktop_lib::prime_guidance::{thread_guidance, with_guidance};
    use sinew_desktop_lib::prime_session::{kill_session, open_thread, thread_path};

    let root = scratch_dir();
    let agent_dir = root.join("agent");
    let workspace = root.join("workspace");
    std::fs::create_dir_all(&workspace).unwrap();
    let workspace_id = workspace.to_string_lossy().into_owned();
    let store = AppStore::open_at(root.join("desktop-state.sqlite3")).unwrap();
    let insert = |level: LessonLevel, title: &str, content: &str| {
        let new = NewLesson {
            level,
            workspace_id: Some(workspace_id.clone()),
            project_type: None,
            kind: LessonKind::Memory,
            title: title.to_string(),
            content: content.to_string(),
        };
        assert!(matches!(
            store.insert_lesson(&new, &LessonOrigin::user()).unwrap(),
            InsertLessonOutcome::Created(_)
        ));
    };
    insert(LessonLevel::Global, "Langue", "Répondre en français.");
    for index in 0..15 {
        insert(
            LessonLevel::Project,
            &format!("Règle {index:02}"),
            &format!("{index:02} {}", "détail ".repeat(45)),
        );
    }
    let script = root.join("faux.json");
    std::fs::write(
        &script,
        serde_json::json!({ "engine": "faux", "responses": [{ "text": "ok" }] }).to_string(),
    )
    .unwrap();
    let socket_path = root.join("daemon.sock");
    let exe = PathBuf::from(env!("CARGO_BIN_EXE_Sinew"));
    let (client, _events) = ensure_daemon_running_with(&exe, &socket_path, &agent_dir)
        .await
        .expect("daemon starts");
    let path = thread_path(&agent_dir, "conv-guidance").unwrap();
    let base = serde_json::json!({
        "cwd": workspace.to_string_lossy(),
        "script": script.to_string_lossy(),
    });
    let system_prompt = |session: String| {
        let client = &client;
        async move {
            client
                .request_ok(pa_types::daemon::DaemonCommand::GetSystemPrompt {
                    id: None,
                    active_session_id: session,
                    rest: serde_json::Map::default(),
                })
                .await
                .expect("get_system_prompt")["systemPrompt"]
                .as_str()
                .unwrap_or_default()
                .to_string()
        }
    };

    let guidance = thread_guidance(&store, &root, &workspace_id).unwrap();
    assert!(!guidance.left_out.is_empty(), "guidance: {guidance:?}");
    let opened = open_thread(&client, with_guidance(base.clone(), &guidance), &path)
        .await
        .expect("thread opened");
    let prompt = system_prompt(opened.active_session_id.clone()).await;
    let section = &prompt[prompt
        .find("# Additional Guidance")
        .expect("guidance section")..];
    assert!(section.contains("\n- Consignes de yusAi : n'écris jamais dans le harness global"));
    assert!(section.contains(&format!("{}", root.join("prime-skills/projects").display())));
    // La leçon la plus récente d'abord, coupée à 300 caractères.
    assert!(section.contains("\n- [projet · fait] Règle 14 : 14 détail"));
    assert!(section.contains("…\n"));
    assert!(section.contains(&format!(
        "\n- {} autres leçons de yusAi ne sont pas injectées (limite de taille).",
        guidance.left_out.len()
    )));
    for line in &guidance.lines {
        assert!(section.contains(&format!("- {line}")), "missing: {line}");
    }
    assert!(
        !section.contains("Répondre en français."),
        "the global lesson is left out"
    );

    // Une leçon ajoutée en cours de route : absente jusqu'à la réouverture.
    insert(LessonLevel::Project, "Nouvelle", "Toujours relire le diff.");
    assert!(!system_prompt(opened.active_session_id.clone())
        .await
        .contains("Toujours relire le diff."));
    kill_session(&client, &opened.active_session_id)
        .await
        .expect("session killed");
    let guidance = thread_guidance(&store, &root, &workspace_id).unwrap();
    let reopened = open_thread(&client, with_guidance(base, &guidance), &path)
        .await
        .expect("thread reopened");
    assert!(system_prompt(reopened.active_session_id.clone())
        .await
        .contains("\n- [projet · fait] Nouvelle : Toujours relire le diff."));

    kill_session(&client, &reopened.active_session_id)
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

/// Une refine et un tour qui se chevauchent, dans les deux sens : un
/// prompt envoyé pendant une refine (« Retenir » est actif hors tour, mais
/// un tour peut partir pendant la refine), et une refine lancée pendant un
/// tour. La refine « Retenir » est notée `refine:retain` dans l'historique.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn a_refine_and_a_turn_can_overlap() {
    let _alone = REFINE_TESTS.lock().await;
    use pa_tui::daemon_client::DaemonClientEvent;
    use sinew_app::store::{AppStore, LessonScope};
    use sinew_desktop_lib::prime_lessons::{refine_harness_file, ThreadContext};
    use sinew_desktop_lib::prime_refine::{run_refine, RefineOrigin};
    use sinew_desktop_lib::prime_session::prompt;
    use std::time::{Duration, Instant};

    let root = scratch_dir();
    let agent_dir = root.join("agent");
    let workspace = root.join("workspace");
    std::fs::create_dir_all(&workspace).unwrap();
    let store = AppStore::open_at(root.join("desktop-state.sqlite3")).unwrap();
    project_lesson(&store, &workspace);
    let plan = |id: &str, content: &str| {
        serde_json::json!({
            "summary": id,
            "rationale": "trajectory evidence",
            "expectedOutcome": "reused",
            "edits": [{ "action": "create", "kind": "memory", "id": id,
                        "title": "Chevauchement", "content": content }],
        })
    };
    // Ordre des appels au modèle : planificateur (lent), tour envoyé
    // pendant la refine, tour lent, planificateur lancé pendant ce tour.
    let (client, session, mut events) = scripted_thread_with_events(
        &root,
        &workspace,
        &[
            (
                plan("pendant-refine", "Leçon d'une refine doublée d'un tour."),
                4_000,
            ),
            (serde_json::json!("réponse pendant la refine"), 0),
            (serde_json::json!("réponse lente"), 4_000),
            (
                plan("pendant-tour", "Leçon d'une refine lancée pendant un tour."),
                0,
            ),
        ],
    )
    .await;
    let turn_ends = std::sync::Arc::new(std::sync::Mutex::new(Vec::<Instant>::new()));
    let recorder = {
        let turn_ends = turn_ends.clone();
        tokio::spawn(async move {
            while let Some(event) = events.recv().await {
                if let DaemonClientEvent::SessionEvent { event, .. } = event {
                    if event["type"] == "agent_end" {
                        turn_ends.lock().unwrap().push(Instant::now());
                    }
                }
            }
        })
    };
    let wait_turn_ends = |count: usize| {
        let turn_ends = turn_ends.clone();
        async move {
            for _ in 0..300 {
                if turn_ends.lock().unwrap().len() >= count {
                    return true;
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            false
        }
    };
    let socket_path = root.join("daemon.sock");
    let thread = ThreadContext {
        conversation_id: "conv-queue".to_string(),
        workspace_id: workspace.to_string_lossy().into_owned(),
    };
    let refine = |thread: ThreadContext| {
        let (socket_path, store, agent_dir, session) = (
            socket_path.clone(),
            store.clone(),
            agent_dir.clone(),
            session.clone(),
        );
        tokio::spawn(async move {
            let run = run_refine(
                &socket_path,
                store,
                agent_dir,
                &session,
                thread,
                RefineOrigin::Retain,
                Some("Retiens surtout les chevauchements.".to_string()),
            )
            .await;
            (run, Instant::now())
        })
    };
    let local_file = refine_harness_file(&agent_dir, false);

    // 1. Un prompt pendant la refine : admis, et son tour se joue pendant
    // que le planificateur attend.
    let started = refine(thread.clone());
    let mut seeded = false;
    for _ in 0..100 {
        if !harness_entry_ids(&local_file, "memory").is_empty() {
            seeded = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(seeded, "lessons seeded");
    tokio::time::sleep(Duration::from_millis(1_000)).await;
    prompt(&client, &session, "un tour pendant la refine")
        .await
        .expect("a prompt during a refine is admitted");
    assert!(wait_turn_ends(1).await, "the turn ends");
    let (run, refined_at) = started.await.unwrap();
    let run = run.expect("the refine ends too");
    let turn_end = turn_ends.lock().unwrap()[0];
    eprintln!(
        "prompt during refine: turn ended {:?} the refine",
        if turn_end < refined_at {
            "before"
        } else {
            "after"
        }
    );
    assert!(
        turn_end < refined_at,
        "the turn does not wait for the refine"
    );
    let report = run.report.expect("imported by the queue");
    assert_eq!(report.created.len(), 1, "report: {report:?}");
    let events = store.lesson_events(&report.created[0]).unwrap();
    assert_eq!(events[0].actor, "refine:retain");
    assert_eq!(events[0].conversation_id.as_deref(), Some("conv-queue"));

    // 2. Une refine pendant un tour : elle tourne aussi.
    prompt(&client, &session, "un tour lent")
        .await
        .expect("prompt admitted");
    tokio::time::sleep(Duration::from_millis(1_000)).await;
    let started_at = Instant::now();
    let (run, refined_at) = refine(thread.clone()).await.unwrap();
    let run = run.expect("a refine during a turn runs");
    assert!(wait_turn_ends(2).await, "the slow turn ends");
    let turn_end = turn_ends.lock().unwrap()[1];
    eprintln!(
        "refine during turn: refine took {:?}, ended {:?} the turn",
        refined_at - started_at,
        if refined_at < turn_end {
            "before"
        } else {
            "after"
        }
    );
    assert!(
        refined_at < turn_end,
        "the refine does not wait for the turn"
    );
    let report = run.report.expect("imported by the queue");
    assert_eq!(report.created.len(), 1, "report: {report:?}");
    let scope = LessonScope {
        workspace_id: thread.workspace_id.clone(),
        project_type: None,
    };
    assert_eq!(store.applicable_lessons(&scope).unwrap().len(), 3);
    assert!(harness_entry_ids(&local_file, "memory").is_empty());
    let mut contents: Vec<String> = store
        .applicable_lessons(&scope)
        .unwrap()
        .into_iter()
        .map(|lesson| lesson.content)
        .collect();
    contents.sort();
    assert_eq!(
        contents,
        vec![
            "Lancer cargo test.",
            "Leçon d'une refine doublée d'un tour.",
            "Leçon d'une refine lancée pendant un tour.",
        ]
    );

    // Rien ne se perd dans le fil : les deux réponses et les deux refines.
    let messages = client
        .request_ok(pa_types::daemon::DaemonCommand::GetMessages {
            id: None,
            active_session_id: session.clone(),
            rest: serde_json::Map::default(),
        })
        .await
        .expect("get_messages")["messages"]
        .to_string();
    assert!(messages.contains("réponse pendant la refine"));
    assert!(messages.contains("réponse lente"));
    assert_eq!(
        messages
            .matches("\"customType\":\"refinement_outcome\"")
            .count(),
        2
    );

    recorder.abort();
    stop_scripted_thread(client, &session, &root).await;
}

/// Le dernier état (`session_state`) écrit dans le fichier d'un fil.
fn last_session_state(path: &std::path::Path) -> Option<String> {
    std::fs::read_to_string(path)
        .ok()?
        .lines()
        .rev()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .find(|row| row["type"] == "session_state")
        .and_then(|row| row["state"]["status"].as_str().map(str::to_string))
}

/// Fermeture d'une conversation : la fermeture courte refine sans tuer ; la
/// mise en veille tue le worker (fil `archived`), et le fil se rouvre avec
/// son modèle et son niveau de réflexion ; une refine de veille qui échoue
/// n'empêche pas le `Kill` et laisse `pending = 1`.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn closing_refines_then_puts_the_worker_to_sleep_and_the_thread_reopens() {
    let _alone = REFINE_TESTS.lock().await;
    use pa_tui::daemon_client::DaemonClientEvent;
    use sinew_app::store::{AppStore, LessonScope};
    use sinew_desktop_lib::prime_close::{close_conversation, CloseAction};
    use sinew_desktop_lib::prime_lessons::ThreadContext;
    use sinew_desktop_lib::prime_session::{
        kill_session, open_thread, prompt, session_config, set_thinking_level, thread_path,
    };

    let root = scratch_dir();
    let agent_dir = root.join("agent");
    let workspace = root.join("workspace");
    std::fs::create_dir_all(&workspace).unwrap();
    let store = AppStore::open_at(root.join("desktop-state.sqlite3")).unwrap();
    let plan = serde_json::json!({
        "summary": "close",
        "rationale": "trajectory evidence",
        "expectedOutcome": "reused",
        "edits": [{ "action": "create", "kind": "memory", "id": "yusai-close",
                    "title": "Fermeture", "content": "Une leçon de fermeture." }],
    });
    // Chaque worker lit le script depuis le début : le worker rouvert reçoit
    // « ok » à son premier appel, qui n'est pas un plan de refine.
    let script = root.join("faux.json");
    std::fs::write(
        &script,
        serde_json::json!({
            "engine": "faux",
            "reasoning": true,
            "responses": [{ "text": "ok" }, { "text": plan.to_string() }],
        })
        .to_string(),
    )
    .unwrap();
    let socket_path = root.join("daemon.sock");
    let exe = PathBuf::from(env!("CARGO_BIN_EXE_Sinew"));
    let (client, mut events) = ensure_daemon_running_with(&exe, &socket_path, &agent_dir)
        .await
        .expect("daemon starts");
    let path = thread_path(&agent_dir, "conv-close").unwrap();
    let config = serde_json::json!({
        "cwd": workspace.to_string_lossy(),
        "script": script.to_string_lossy(),
    });
    let opened = open_thread(&client, config.clone(), &path)
        .await
        .expect("thread opened");
    let session = opened.active_session_id.clone();
    prompt(&client, &session, "salut")
        .await
        .expect("prompt admitted");
    let turn = tokio::time::timeout(std::time::Duration::from_secs(60), async {
        while let Some(event) = events.recv().await {
            if let DaemonClientEvent::SessionEvent { event, .. } = event {
                if event["type"] == "agent_end" {
                    return;
                }
            }
        }
    })
    .await;
    assert!(turn.is_ok(), "turn ended");
    tokio::spawn(async move { while events.recv().await.is_some() {} });
    // Ce que compte `prime_prompt`.
    store.note_user_turn("conv-close").unwrap();
    set_thinking_level(&client, &session, "high")
        .await
        .expect("thinking level set");
    let before = session_config(&client, &session).await.unwrap();
    let thread = ThreadContext {
        conversation_id: "conv-close".to_string(),
        workspace_id: workspace.to_string_lossy().into_owned(),
    };
    let close = |session: String, action: CloseAction| {
        let (client, socket_path, store, agent_dir, thread) = (
            &client,
            socket_path.clone(),
            store.clone(),
            agent_dir.clone(),
            thread.clone(),
        );
        async move {
            close_conversation(
                client,
                &socket_path,
                store,
                agent_dir,
                &session,
                thread,
                action,
                || false,
            )
            .await
        }
    };

    // Fermeture courte : refine, le worker reste.
    let outcome = close(session.clone(), CloseAction::Refine).await;
    let run = outcome.refine.expect("a turn to refine").expect("refined");
    assert!(!outcome.killed);
    let report = run.report.expect("imported");
    assert_eq!(report.created.len(), 1, "report: {report:?}");
    let history = store.lesson_events(&report.created[0]).unwrap();
    assert_eq!(history[0].actor, "refine:close");
    let state = store.refine_state("conv-close").unwrap();
    assert_eq!(state.user_turns_since_refine, 0);
    assert!(!state.pending);
    assert!(
        session_config(&client, &session).await.is_ok(),
        "still alive"
    );

    // Mise en veille sans nouveau tour : pas de refine, le worker est tué.
    let outcome = close(session.clone(), CloseAction::Sleep).await;
    assert!(outcome.refine.is_none());
    assert!(outcome.killed);
    assert!(session_config(&client, &session).await.is_err(), "killed");
    assert_eq!(last_session_state(&path).as_deref(), Some("archived"));

    // Le fil se rouvre depuis son fichier, modèle et niveau restaurés.
    let reopened = open_thread(&client, config.clone(), &path)
        .await
        .expect("thread reopened");
    assert_ne!(reopened.active_session_id, session);
    assert!(reopened
        .messages
        .iter()
        .any(|message| message.to_string().contains("salut")));
    let after = session_config(&client, &reopened.active_session_id)
        .await
        .unwrap();
    assert_eq!(after.thinking_level.as_deref(), Some("high"));
    assert_eq!(
        after
            .model
            .as_ref()
            .map(|model| (&model.provider, &model.id)),
        before
            .model
            .as_ref()
            .map(|model| (&model.provider, &model.id))
    );
    assert_ne!(last_session_state(&path).as_deref(), Some("archived"));

    // Mise en veille dont la refine échoue : tué quand même, `pending = 1`.
    store.note_user_turn("conv-close").unwrap();
    let outcome = close(reopened.active_session_id.clone(), CloseAction::Sleep).await;
    assert!(outcome.refine_failed(), "outcome: {outcome:?}");
    assert!(outcome.killed);
    let state = store.refine_state("conv-close").unwrap();
    assert!(state.pending);
    assert_eq!(state.user_turns_since_refine, 1);
    assert_eq!(last_session_state(&path).as_deref(), Some("archived"));
    let scope = LessonScope {
        workspace_id: thread.workspace_id.clone(),
        project_type: None,
    };
    assert_eq!(store.applicable_lessons(&scope).unwrap().len(), 1);

    let _ = kill_session(&client, &reopened.active_session_id).await;
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

/// Refines mises en attente à la sortie (Cmd+Q), reprises au démarrage :
/// fil rouvert, refiné puis tué ; refine sur la session que l'UI a déjà
/// ouverte, sans la tuer ; conversation disparue, attente levée ; refine
/// ratée, attente gardée.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn deferred_refines_run_at_the_next_start() {
    let _alone = REFINE_TESTS.lock().await;
    use pa_tui::daemon_client::DaemonClientEvent;
    use sinew_app::store::AppStore;
    use sinew_desktop_lib::prime_close::{refine_pending, PendingOutcome};
    use sinew_desktop_lib::prime_session::{kill_session, open_thread, prompt, thread_path};

    let root = scratch_dir();
    let agent_dir = root.join("agent");
    let workspace = root.join("workspace");
    std::fs::create_dir_all(&workspace).unwrap();
    let workspace_id = workspace.to_string_lossy().into_owned();
    let store = AppStore::open_at(root.join("desktop-state.sqlite3")).unwrap();
    // Un script par worker : chacun lit le sien depuis le début.
    let script = |name: &str, responses: Vec<serde_json::Value>| {
        let path = root.join(format!("{name}.json"));
        std::fs::write(
            &path,
            serde_json::json!({ "engine": "faux", "responses": responses }).to_string(),
        )
        .unwrap();
        serde_json::json!({
            "cwd": workspace.to_string_lossy(),
            "script": path.to_string_lossy(),
        })
    };
    let plan = |content: &str| {
        serde_json::json!({ "text": serde_json::json!({
            "summary": content,
            "rationale": "trajectory evidence",
            "expectedOutcome": "reused",
            "edits": [{ "action": "create", "kind": "memory", "id": "yusai-deferred",
                        "title": "Reprise", "content": content }],
        }).to_string() })
    };
    let socket_path = root.join("daemon.sock");
    let exe = PathBuf::from(env!("CARGO_BIN_EXE_Sinew"));
    let (client, mut events) = ensure_daemon_running_with(&exe, &socket_path, &agent_dir)
        .await
        .expect("daemon starts");
    // Une conversation avec un tour, puis Cmd+Q : `pending = 1`, worker tué.
    let path = thread_path(&agent_dir, "conv-deferred").unwrap();
    let opened = open_thread(
        &client,
        script("first", vec![serde_json::json!({ "text": "ok" })]),
        &path,
    )
    .await
    .expect("thread opened");
    prompt(&client, &opened.active_session_id, "salut")
        .await
        .expect("prompt admitted");
    let turn = tokio::time::timeout(std::time::Duration::from_secs(60), async {
        while let Some(event) = events.recv().await {
            if let DaemonClientEvent::SessionEvent { event, .. } = event {
                if event["type"] == "agent_end" {
                    return;
                }
            }
        }
    })
    .await;
    assert!(turn.is_ok(), "turn ended");
    tokio::spawn(async move { while events.recv().await.is_some() {} });
    store.note_user_turn("conv-deferred").unwrap();
    assert!(store.defer_refine_if_unrefined("conv-deferred").unwrap());
    kill_session(&client, &opened.active_session_id)
        .await
        .expect("killed at exit");
    let deferred = |workspace: Option<String>, open: Option<String>, config: serde_json::Value| {
        let (client, socket_path, store, agent_dir) = (
            &client,
            socket_path.clone(),
            store.clone(),
            agent_dir.clone(),
        );
        async move {
            refine_pending(
                client,
                &socket_path,
                store,
                agent_dir,
                "conv-deferred",
                workspace,
                open,
                |_| config,
                |_| false,
            )
            .await
        }
    };

    // Démarrage : fil rouvert, refiné, worker tué.
    let outcome = deferred(
        Some(workspace_id.clone()),
        None,
        script("restart", vec![plan("Reprise au démarrage.")]),
    )
    .await;
    let PendingOutcome::Refined { run, killed } = outcome else {
        panic!("outcome: {outcome:?}");
    };
    assert!(killed);
    let report = run.report.expect("imported");
    assert_eq!(
        store.lesson_events(&report.created[0]).unwrap()[0].actor,
        "refine:close"
    );
    assert!(store.pending_refines().unwrap().is_empty());
    assert_eq!(last_session_state(&path).as_deref(), Some("archived"));

    // L'UI a déjà rouvert la conversation : refine sur sa session, gardée.
    let ui = open_thread(
        &client,
        script("ui", vec![plan("Reprise dans l'UI.")]),
        &path,
    )
    .await
    .expect("ui opens the thread");
    store.note_user_turn("conv-deferred").unwrap();
    store.defer_refine_if_unrefined("conv-deferred").unwrap();
    let outcome = deferred(
        Some(workspace_id.clone()),
        Some(ui.active_session_id.clone()),
        serde_json::Value::Null,
    )
    .await;
    assert!(
        matches!(outcome, PendingOutcome::Refined { killed: false, .. }),
        "outcome: {outcome:?}"
    );
    assert!(store.pending_refines().unwrap().is_empty());
    assert_ne!(last_session_state(&path).as_deref(), Some("archived"));
    kill_session(&client, &ui.active_session_id)
        .await
        .expect("ui session killed");

    // Refine ratée : worker tué quand même, attente gardée.
    store.note_user_turn("conv-deferred").unwrap();
    store.defer_refine_if_unrefined("conv-deferred").unwrap();
    let outcome = deferred(
        Some(workspace_id.clone()),
        None,
        script("broken", vec![serde_json::json!({ "text": "pas un plan" })]),
    )
    .await;
    assert!(
        matches!(outcome, PendingOutcome::Failed { killed: true, .. }),
        "outcome: {outcome:?}"
    );
    assert_eq!(store.pending_refines().unwrap(), vec!["conv-deferred"]);

    // Conversation sans projet (supprimée) : attente levée, rien d'ouvert.
    let outcome = deferred(None, None, serde_json::Value::Null).await;
    assert!(
        matches!(outcome, PendingOutcome::Dropped(_)),
        "outcome: {outcome:?}"
    );
    assert!(store.pending_refines().unwrap().is_empty());

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

/// `uv` construit le venv du noyau (pa-core/src/kernel/bootstrap/venv/uv.rs:85-101).
fn uv_available() -> bool {
    let on_path = std::env::var_os("PATH")
        .is_some_and(|paths| std::env::split_paths(&paths).any(|dir| dir.join("uv").is_file()));
    on_path
        || std::env::var_os("HOME")
            .is_some_and(|home| PathBuf::from(home).join(".local/bin/uv").is_file())
}

/// Les sessions de `{"type": "list"}` dont l'`activeSessionId` est donné.
async fn listed_session_ids(client: &pa_tui::daemon_client::DaemonClient) -> Vec<String> {
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
        .into_iter()
        .flatten()
        .filter_map(|session| session["activeSessionId"].as_str().map(str::to_string))
        .collect()
}

/// Les enfants RLM d'une session (pa-daemon/src/state_getters.rs:38-64).
async fn rlm_children(
    client: &pa_tui::daemon_client::DaemonClient,
    active_session_id: &str,
) -> Vec<serde_json::Value> {
    client
        .request_ok(pa_types::daemon::DaemonCommand::GetRlmChildren {
            id: None,
            active_session_id: active_session_id.to_string(),
            rest: serde_json::Map::default(),
        })
        .await
        .expect("get_rlm_children")["children"]
        .as_array()
        .cloned()
        .unwrap_or_default()
}

/// Attend qu'une session disparaisse de `list`.
async fn wait_until_unlisted(client: &pa_tui::daemon_client::DaemonClient, id: &str) -> bool {
    for _ in 0..100 {
        if !listed_session_ids(client)
            .await
            .iter()
            .any(|listed| listed == id)
        {
            return true;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    false
}

/// Sous-agents, avec un vrai noyau Python (sauté sans `uv`) : une cellule
/// `rlm.spawn` lance un enfant, dont la réponse arrive au parent en ligne
/// `custom` `agent_message` (pa-core/src/session_engine/agent_messaging.rs:303-326).
/// Les enfants ne portent pas le marquage yusAi
/// (pa-daemon/src/rlm_children/host.rs:109-115) mais meurent avec leur
/// parent : au `Kill` (pa-daemon/src/worker/commands.rs:659), et donc aussi
/// quand le nettoyage des orphelins tue le parent d'un IDE disparu.
/// Le venv du noyau est stable d'un passage à l'autre (`CARGO_TARGET_TMPDIR`).
#[tokio::test(flavor = "multi_thread")]
async fn subagents_report_to_their_parent_and_die_with_it() {
    use pa_tui::daemon_client::DaemonClientEvent;
    use sinew_desktop_lib::prime::ensure_daemon_running_with_kernel_venv;
    use sinew_desktop_lib::prime_session::{
        kill_session, open_thread, open_thread_with_metadata, prompt, reap_orphaned_sessions,
        thread_path,
    };

    if !uv_available() {
        eprintln!("uv not found; skipping the live-kernel sub-agent e2e");
        return;
    }
    let kernel_venv = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("prime-kernel-venv");
    let root = scratch_dir();
    let agent_dir = root.join("agent");
    let workspace = root.join("workspace");
    std::fs::create_dir_all(&workspace).unwrap();
    let write = |name: &str, script: serde_json::Value| {
        let path = root.join(name);
        std::fs::write(&path, script.to_string()).unwrap();
        path
    };
    let spawn_cell = |name: &str| {
        serde_json::json!({ "content": [{
            "type": "toolCall",
            "name": "ipython",
            "arguments": { "code": format!("handle = await rlm.spawn(\"fais ta tâche\", name=\"{name}\")") },
        }] })
    };
    // L'enfant répond au parent par le handler du noyau, comme les tests de
    // Prime (pa-daemon/tests/agent_family_e2e.rs:256-264).
    let child = write(
        "child.json",
        serde_json::json!({ "engine": "faux", "responses": [
            { "content": [{
                "type": "toolCall",
                "name": "ipython",
                "arguments": { "code": "from rlm import host_request\nawait host_request(\"agent_message.send\", {\"message\": \"fini du kid\", \"receiver_role\": \"parent\"})" },
            }] },
            { "text": "fini" },
        ] }),
    );
    let parent = write(
        "parent.json",
        serde_json::json!({ "engine": "faux", "responses": [
            spawn_cell("kid"),
            { "text": "enfant lancé" },
            { "text": "bien reçu" },
        ] }),
    );
    // Un enfant qui reste occupé, pour le parent orphelin.
    let busy_child = write(
        "busy-child.json",
        serde_json::json!({ "responses": [{ "text": "toujours là", "delayMs": 60_000 }] }),
    );
    let orphan_parent = write(
        "orphan-parent.json",
        serde_json::json!({ "engine": "faux", "responses": [
            spawn_cell("busy"),
            { "text": "enfant lancé" },
        ] }),
    );
    #[cfg(unix)]
    let socket_path = root.join("daemon.sock");
    #[cfg(not(unix))]
    let socket_path = PathBuf::from(format!(
        r"\\.\pipe\yusai-prime-test-{}",
        root.file_name().unwrap().to_string_lossy()
    ));
    let exe = PathBuf::from(env!("CARGO_BIN_EXE_Sinew"));

    let (client, mut events) =
        ensure_daemon_running_with_kernel_venv(&exe, &socket_path, &agent_dir, &kernel_venv)
            .await
            .expect("daemon starts");

    // 1. Parent vivant : l'enfant répond, puis meurt avec le parent.
    let opened = open_thread(
        &client,
        serde_json::json!({
            "cwd": workspace.to_string_lossy(),
            "script": parent.to_string_lossy(),
            "childScript": child.to_string_lossy(),
        }),
        &thread_path(&agent_dir, "conv-agents").unwrap(),
    )
    .await
    .expect("parent opened");
    let parent_id = opened.active_session_id.clone();
    prompt(&client, &parent_id, "lance un sous-agent")
        .await
        .expect("prompt admitted");
    let mut seen = Vec::new();
    // La première construction du venv peut prendre quelques minutes.
    let reply = tokio::time::timeout(std::time::Duration::from_secs(600), async {
        while let Some(event) = events.recv().await {
            let DaemonClientEvent::SessionEvent {
                active_session_id,
                event,
                ..
            } = event
            else {
                continue;
            };
            if active_session_id != parent_id {
                continue;
            }
            seen.push(event["type"].as_str().unwrap_or_default().to_string());
            let message = &event["message"];
            if event["type"] == "message_start"
                && message["role"] == "custom"
                && message["customType"] == "agent_message"
            {
                return message["details"].clone();
            }
        }
        serde_json::Value::Null
    })
    .await;
    let details = reply
        .unwrap_or_else(|_| panic!("no agent_message reached the parent; parent events: {seen:?}"));
    assert_eq!(details["message"], "fini du kid", "details: {details}");
    assert_eq!(details["from"]["sessionName"], "kid", "details: {details}");
    assert_eq!(
        details["from"]["runtimeKind"], "subagent",
        "details: {details}"
    );
    assert_eq!(details["fromRelationship"], "child", "details: {details}");

    let children = rlm_children(&client, &parent_id).await;
    assert_eq!(children.len(), 1, "children: {children:?}");
    assert_eq!(children[0]["sessionName"], "kid");
    let kid_id = children[0]["activeSessionId"]
        .as_str()
        .expect("child activeSessionId")
        .to_string();
    // Le noyau de l'enfant a démarré (sa cellule a répondu). Son client de
    // télémétrie écrit par lots toutes les 10 s
    // (pa-telemetry/src/client.rs:40) : on attend un lot pendant que
    // l'enfant vit encore, sinon l'absence de `telemetry.jsonl` ne prouve
    // rien. Sans `telemetry.localMirror: false`, la ligne « kernel
    // bootstrap » de l'enfant arrivait ici
    // (pa-core/src/session_engine/engine.rs:386-409).
    tokio::time::sleep(std::time::Duration::from_secs(12)).await;
    assert!(
        !agent_dir.join("telemetry.jsonl").exists(),
        "a live sub-agent records no telemetry event"
    );
    kill_session(&client, &parent_id)
        .await
        .expect("parent killed");
    assert!(
        wait_until_unlisted(&client, &kid_id).await,
        "the child died with its parent"
    );

    // 2. IDE disparu : le nettoyage tue le parent marqué, l'enfant suit.
    let mut gone = std::process::Command::new("true").spawn().unwrap();
    let gone_pid = gone.id();
    gone.wait().unwrap();
    // Un parent avec fichier : un parent en mémoire ne peut pas inscrire
    // d'enfant au registre RLM de Prime (« invalid spawn »), l'enfant est
    // aussitôt arrêté.
    let orphan_id = open_thread_with_metadata(
        &client,
        serde_json::json!({
            "cwd": workspace.to_string_lossy(),
            "script": orphan_parent.to_string_lossy(),
            "childScript": busy_child.to_string_lossy(),
        }),
        &thread_path(&agent_dir, "conv-orphan").unwrap(),
        serde_json::json!({ "yusai": { "idePid": gone_pid, "ideProcessStartId": "gone" } }),
    )
    .await
    .expect("orphan parent created")
    .active_session_id;
    prompt(&client, &orphan_id, "lance un sous-agent")
        .await
        .expect("prompt admitted");
    let mut busy_id = None;
    for _ in 0..600 {
        if let Some(id) = rlm_children(&client, &orphan_id)
            .await
            .first()
            .and_then(|child| child["activeSessionId"].as_str())
            .filter(|id| !id.is_empty())
        {
            busy_id = Some(id.to_string());
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    let busy_id = busy_id.expect("the orphan's child was spawned");
    assert!(listed_session_ids(&client).await.contains(&busy_id));
    let reaped = reap_orphaned_sessions(&client, &agent_dir, &socket_path)
        .await
        .expect("reap");
    assert_eq!(
        reaped,
        vec![orphan_id.clone()],
        "only the marked parent is reaped directly"
    );
    assert!(
        wait_until_unlisted(&client, &busy_id).await,
        "the unmarked child died with its reaped parent"
    );
    // Les enfants sont créés par Prime sans `telemetryDisabled`
    // (pa-daemon/src/rlm_children/lifecycle.rs:124-130) : leur worker crée
    // l'identifiant `telemetry.json` (pa-core/src/session_engine/telemetry.rs:1073-1079),
    // mais le miroir local est coupé par nos réglages et rien ne part sans
    // point d'envoi configuré (telemetry.rs:1084-1091).
    assert!(
        !agent_dir.join("telemetry.jsonl").exists(),
        "no telemetry event recorded with sub-agents and a live kernel"
    );

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
