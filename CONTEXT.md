# Intégration Prime Agent : état et suite

À lire en début de session sur la branche `prime-integration`. Les règles
permanentes sont dans `CLAUDE.md` ; ce fichier décrit l'état du travail.

## Ce qui marche (vérifié dans l'app)

- **Binaire multi-rôle** (`src-tauri/src/prime.rs`) : `main.rs` route
  `worker` (avec `PRIME_AGENT_INTERNAL_DAEMON_WORKER=1`) vers
  `pa_daemon::worker::run_worker`, `--mode daemon --daemon-socket <p>
  --agent-dir <d>` vers `pa_daemon::supervisor::run_supervisor`, sinon l'IDE.
- **Isolation** : dossier d'état `~/Library/Application Support/dev.hyrak.sinew/prime-agent`,
  socket `<TMPDIR>/yusai-prime-<hash>/daemon.sock`. Ces variables
  (`PRIME_AGENT_CODING_AGENT_DIR`, `PI_PACKAGE_DIR` en debug,
  `PRIME_AGENT_TELEMETRY=0`) ne sont posées que sur le `Command` du
  superviseur, jamais sur le processus IDE.
- **Daemon paresseux** : lancé à la première session Prime
  (`ensure_daemon_running`, calqué sur pa-cli).
- **Chat Prime** (`src-tauri/src/prime_session.rs`, `src/components/chat/PrimeChatPane.tsx`) :
  bascule Sinew / Prime dans l'en-tête du chat (Sinew par défaut), une
  session Prime par conversation yusAi, démarrée quand le panneau s'affiche.
  Protocole natif (`DaemonClient` + `DaemonCommand`), événements relayés en
  `prime-event`. Rendu : texte de l'assistant seulement.
- **Modèle et réflexion** : sélecteurs dans le composer (style Sinew,
  `ComposerPicker`), via `get_connection_state` / `get_available_models` /
  `set_model` / `set_thinking_level`. Opus 5.5 et Sonnet 5.5 disponibles.
  Le dernier choix devient le défaut des sessions suivantes (le worker
  l'enregistre).
- **Auth** (`src-tauri/src/prime_auth.rs`) : Prime réutilise la connexion
  Anthropic OAuth de yusAi. Seul le provider de yusAi rafraîchit (refresh
  token à usage unique) ; Prime reçoit dans son `auth.json` le token
  d'accès seul, sans refresh token. Le `Credential` est partagé via
  `set_anthropic_credential` aux points d'installation du provider
  (`lib.rs`, `providers.rs`).
- **Télémétrie Prime coupée** : env du superviseur + `telemetry_disabled`
  au `Create`.
- **Workers orphelins** : Cmd+Q tue les sessions de l'IDE puis arrête le
  daemon s'il est vide ; au démarrage, les sessions dont l'IDE (pid +
  identité de démarrage, dans `runtimeMetadata.yusai`) n'existe plus sont
  tuées. Le marquage se lit dans les descripteurs de workers (List ne le
  renvoie pas).

## Tests

- `cargo test --workspace` : dont `src-tauri/tests/prime_daemon.rs`
  (superviseur réel + worker réel avec le moteur `faux` de Prime).
- Front : `npx tsc --noEmit -p tsconfig.json`, `npx vite build`.
- En dev, réponse scriptée sans modèle : `YUSAI_PRIME_FAUX_SCRIPT=<faux.json>`.
- Arrêter un daemon resté actif : `pkill -f "Sinew --mode daemon"; pkill -f "Sinew worker"`.
- Vérifier qu'il ne reste rien après Cmd+Q : `pgrep -fl "Sinew (worker|--mode daemon)"`.

## Pièges rencontrés

- **rustfmt sur `lib.rs`** reformate aussi les modules déclarés
  (`turns.rs`, `swarm.rs`…) et retrie les imports : ne formater que les
  fichiers feuilles touchés, vérifier `git diff --stat` avant de commiter.
- **React StrictMode** (dev) monte, démonte et remonte : tout drapeau posé
  au démontage doit être remis à zéro au montage.
- **`RunEvent::Exit` sur macOS** s'exécute dans `applicationWillTerminate` :
  un panic y avorte le processus. Ne rien exécuter de Tokio sur le thread
  principal (le nettoyage tourne dans le runtime Tauri, attente sur canal std).
- **CSS** : `.composer__input` rend le texte transparent (calque Sinew) ;
  les surcharges Prime doivent être plus spécifiques.
- **Patch crossterm** : le `[patch]` du `Cargo.toml` racine doit rester
  identique à celui de `vendor/prime-agent/Cargo.toml` (pa-tui en dépend).
- Prime se présente avec une version Claude Code figée dans vendor
  (`claude-cli/2.1.281`) : un modèle qui exige plus récent serait refusé.

## Suite (par priorité)

1. **Afficher les appels d'outils de Prime** dans `PrimeChatPane`
   (aujourd'hui invisibles alors qu'il exécute bash, edit, etc.) :
   événements `tool_execution_start` / `tool_execution_end`, réutiliser
   `ToolCard.tsx`. Référence de mapping : `pa-daemon/src/acp/wire_events.rs`.
2. **Donner à Prime les outils de yusAi** (prévu, pas pour tout de suite) : exposer les outils de
   `crates/sinew-app` sous forme de serveur MCP et l'attacher à chaque
   session avec `DaemonCommand::ReplaceAcpMcpServers`
   (`pa-types/src/daemon/command.rs:676`, usage dans
   `pa-daemon/src/acp/daemon.rs:705-730`), sans patch vendor. Les outils
   interactifs (question, todo) demandent en plus un relais vers l'UI.
3. Persister les fils Prime (aujourd'hui `no_session: true`, perdus au
   redémarrage).
4. Rendu réflexion / sous-agents ; renommer « Sinew » en « yusAi ».
5. Fermer une seule fenêtre ne tue pas ses sessions avant la sortie de l'app.
