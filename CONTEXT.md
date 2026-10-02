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
  `PRIME_AGENT_TELEMETRY=0`, `PRIME_AGENT_KERNEL_VENV=<agent_dir>/kernel-venv`)
  ne sont posées que sur le `Command` du superviseur, jamais sur le
  processus IDE ; les workers en héritent. Le venv du noyau n'est donc plus
  `~/.prime/agent/kernel-venv`, partagé avec une installation séparée de
  Prime : la première cellule le construit avec `uv` (un peu plus lente).
- **Daemon paresseux** : lancé à la première session Prime
  (`ensure_daemon_running`, calqué sur pa-cli).
- **Chat Prime** (`src-tauri/src/prime_session.rs`, `src/components/chat/PrimeChatPane.tsx`) :
  bascule Sinew / Prime dans l'en-tête du chat (Sinew par défaut), une
  session Prime par conversation yusAi, démarrée quand le panneau s'affiche.
  Protocole natif (`DaemonClient` + `DaemonCommand`), événements relayés en
  `prime-event`. Rendu : texte de l'assistant et appels d'outils.
- **Réflexion** : les `thinking_delta` des `message_update` remplissent un
  bloc `AIThinkingBlock` (ouvert pendant le flux, replié ensuite avec sa
  durée), fermé par `thinking_end` ou la fin du message / tour / session ;
  les blocs `thinking` de l'historique sont restaurés (vides et `redacted`
  ignorés, sans durée). Anthropic renvoie une réflexion résumée
  (`display: "summarized"`, `pa-ai/src/providers/anthropic/params.rs:107-128`),
  rien au niveau `off`.
- **Fils persistants** : chaque conversation a son fichier de session Prime,
  `<agent_dir>/yusai-threads/<conversationId>.jsonl` (`thread_path`), hors de
  `sessions/` que le superviseur archive (déplace) après 30 jours / 200
  fichiers (`pa-daemon/src/session_archive.rs:1-17`). `open_thread` fait
  `Create` avec `session_path` : le worker rouvre le fichier avec son modèle
  et son niveau (`pa-daemon/src/worker/create.rs:208-299`), que les
  sélecteurs relisent après l'ouverture ; l'historique vient du
  `snapshot.messages` de l'attach et se convertit en messages et cartes
  (`src/lib/primeHistory.ts`). Un fichier déjà tenu (`SessionAlreadyActive`)
  est rattaché à sa session. Le `Kill` de sortie marque le fichier
  `archived` sans le déplacer ; la réouverture le remet `active`. Les
  orphelins sont tués avant toute ouverture (ils tiendraient le verrou du
  fichier). Une session fermée par le daemon (mise en veille après 90 min
  d'inactivité, `pa-core/src/settings/manager.rs:16`) est rouverte depuis
  son fichier. Supprimer une conversation tue le worker du fil puis appelle
  `delete_saved_session` (`delete_thread`).
- **Appels d'outils** : `tool_execution_start` / `_update` / `_end`
  (`pa-daemon/src/worker/turn.rs:905-932`) affichés avec `ToolCard`, repliés
  par défaut, sortie tronquée à 200 lignes / 20 000 caractères avec « Show
  all ». Le modèle de Prime n'a qu'un outil, `ipython` (`args.code`) : bash
  et edit tournent dans le noyau Python
  (`pa-daemon/src/agent_engine/lifecycle.rs:1126-1128`). Titre de la carte :
  la commande quand la cellule appelle `bash(...)` (icône terminal, sans
  `cd <projet> &&`, « +N » s'il y a d'autres appels ; `src/lib/primeBash.ts`) ;
  sinon, une fois les fichiers modifiés reçus, le chemin du premier (icône
  edit, « +N » ; `src/lib/primeToolTitle.ts`) ; sinon la première ligne de la
  cellule.
- **Fichiers modifiés par un appel** (`src-tauri/src/prime_diffs.rs`) : le
  noyau Prime perd les diffs d'`edit` avant le résultat
  (`pa-core/src/session_engine/runtime_wiring.rs:342-367` ne recopie pas
  `diffs`). On photographie donc le dossier de travail du worker (relu par
  `get_connection_state`, pas le cwd de l'IDE) à chaque `agent_start`, puis
  après chaque `tool_execution_end`, comme le bash de Sinew. L'événement
  Prime part d'abord ; les fichiers suivent dans un événement
  `toolFileChanges` rattaché au `toolCallId`, affichés dans la carte dépliée.
  Limites : plusieurs cellules d'une même réponse s'enchaînent sans pause
  (`pa-core/src/tools/ipython.rs:511-512`), un changement peut alors
  s'afficher sur la carte voisine ; une modification à la main pendant un
  appel est attribuée à Prime ; dossiers ignorés et plafonds de
  `crates/sinew-app/src/tool_run.rs:21-22, 736-753`.
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

## Fait récemment (commits)

- **Workers orphelins** : `8395689` (Exit + démarrage, marquage pid +
  identité de démarrage de l'IDE, test d'intégration), `5674bf8` (correctif :
  le nettoyage faisait avorter l'IDE à la fermeture). Vérifié dans l'app :
  rien ne reste après Cmd+Q, et un orphelin est tué au redémarrage.
- **Texte invisible dans la saisie Prime** : `ca46736`.
- **Style Prime aligné sur Sinew** : `b87fe91` (sélecteurs modèle /
  réflexion avec les classes `composer__picker*` / `composer__popover*`,
  libellés `MODELS` / `THINKING_LEVELS`, icônes `PROVIDERS`). Les messages et
  le composer réutilisaient déjà les classes Sinew (`msg`, `user-text`,
  `composer*`, `Markdown`). Vérifié dans un banc d'essai navigateur, pas dans
  l'app.
- **Appels d'outils de Prime** : `8488c9e` (rendu), `f78d16a` (troncature,
  `outputLimit` opt-in dans `ToolCard`, sans effet sur Sinew), `e29ff53`
  (test e2e). Vérifié dans un banc d'essai navigateur (IPC simulé), pas
  encore dans l'app avec un vrai modèle.
- **Fils persistants** : `26d148e` (Rust : fichier par conversation,
  réouverture, rattachement, nettoyage des orphelins attendu ; test e2e),
  `7c6ac74` (front : historique restauré, sélecteurs relus, réouverture après
  fermeture), commit suivant (suppression du fil avec la conversation, test
  e2e). Vérifié par les tests et le banc d'essai, pas encore dans l'app.

## Tests

- `cargo test --workspace` : dont `src-tauri/tests/prime_daemon.rs`
  (superviseur réel + worker réel avec le moteur `faux` de Prime).
- Front : `npx tsc --noEmit -p tsconfig.json`, `npx vite build`, `npm test`
  (tests unitaires dans `tests/`, lancés par `node --test` sans dépendance).
- En dev, réponse scriptée sans modèle : `YUSAI_PRIME_FAUX_SCRIPT=<faux.json>`.
  Avec `"engine": "faux"` : vrai moteur, texte seulement. Sans `engine` :
  moteur scripté, qui rejoue aussi des appels d'outils
  (`{"responses":[{"text":…,"toolCalls":[{"toolCallId","toolName","args","result","isError","delayMs"}]}]}`,
  `pa-daemon/src/engine/scripted.rs:16-23`).
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
- **Sortie en direct des outils** : `tool_execution_update` porte un
  morceau (incrémental) pour `details.status = "ok"`
  (`pa-core/src/tools/ipython.rs:402-408`), mais le runtime ne branche pas ce
  flux (`on_stream` abandonné, `pa-core/src/session_engine/runtime_wiring.rs:318-321`) :
  en pratique seuls les messages de démarrage du noyau (`"starting"`)
  arrivent pendant l'exécution, la sortie arrive au `tool_execution_end`.
- **Verrou de session et chemin canonique** : le dossier du fichier doit
  exister avant le `Create`, sinon le verrou retient un chemin non canonique
  et refuse ensuite d'écrire (« session lease does not own append target »,
  `pa-daemon/src/lease.rs:78-87, 392-396` ; lien `/var` -> `/private/var`).
- **Suppression = Corbeille (décision validée le 2026-10-02)** : supprimer
  une conversation envoie son fil Prime à la Corbeille, récupérable, plutôt
  que de l'effacer. C'est le comportement de `delete_saved_session`, qui
  passe par `/usr/bin/trash` quand il existe (macOS 26 l'a), sinon supprime
  le fichier (`pa-daemon/src/saved_session_commands.rs:116-138`). Ne pas
  remplacer par une suppression directe. Le test e2e retire de la Corbeille
  le fichier qu'il y envoie.
- **Diffs perdus au redémarrage** (limite acceptée) : les fichiers modifiés
  viennent de nos photos, pas du fichier de session ; après un redémarrage
  les cartes restaurées n'ont plus leurs diffs, et une cellule `edit`
  reprend sa première ligne comme titre.
- **`*_start` n'arrive jamais au client** : le coalesceur du worker remplace
  une trame sans delta (`thinking_start`, `text_start`) par le delta suivant
  (`pa-daemon/src/streaming.rs:118-127`). Ouvrir un bloc au premier delta,
  le fermer sur `*_end`. Les deltas d'un même type s'additionnent sans
  perte (`streaming.rs:1-20`).
- **Test qui panique = daemon orphelin** : un test e2e en échec n'atteint
  pas son `Shutdown` ; vérifier `pgrep -fl yusai-prime-test` après un échec.
- Prime se présente avec une version Claude Code figée dans vendor
  (`claude-cli/2.1.281`) : un modèle qui exige plus récent serait refusé.

## Suite (par priorité)

1. **Affiner les cartes d'outils** : si Prime corrige un jour la perte
   des diffs (`details.diffs`, que son TUI lit déjà,
   `pa-tui/src/tool_card/ipython_details.rs:100-104`), les utiliser à la place
   des photos.
2. **Donner à Prime les outils de yusAi** (prévu, pas pour tout de suite) : exposer les outils de
   `crates/sinew-app` sous forme de serveur MCP et l'attacher à chaque
   session avec `DaemonCommand::ReplaceAcpMcpServers`
   (`pa-types/src/daemon/command.rs:676`, usage dans
   `pa-daemon/src/acp/daemon.rs:705-730`), sans patch vendor. Les outils
   interactifs (question, todo) demandent en plus un relais vers l'UI.
3. **Sous-agents (étape B)**, vus depuis le parent : carte de lancement
   « Agent · nom » en reconnaissant `rlm.spawn(…, name="x")` dans la cellule
   (titre « Agent » générique si le nom n'est pas un littéral) ; lignes
   `custom` `agent_message` (`pa-core/src/session_engine/agent_messaging.rs:303-326`)
   et `rlm_child_terminal_notice` (`pa-core/src/session_engine/rlm_notices.rs:41-86`)
   en direct et dans l'historique ; état des enfants par `get_rlm_children`
   (`pa-daemon/src/state_getters.rs:38-64`) tant qu'un enfant tourne.
   À vérifier d'abord : que les sessions enfants (leur propre worker) sont
   couvertes par le nettoyage des orphelins (marquage `runtimeMetadata.yusai`
   absent chez elles ? tuées en cascade avec le parent ?), la forme exacte
   de l'appel écrit par le modèle, et comment `details.from` désigne
   l'enfant. Étape C plus tard : vue détaillée d'un enfant (Attach à sa
   session, ses appels d'outils).
   Renommer « Sinew » en « yusAi ».
4. Fermer une seule fenêtre ne tue pas ses sessions avant la sortie de l'app.
   Inversement, deux fenêtres sur la même conversation partagent la session :
   en fermer une la tue, l'autre la rouvre depuis le fichier.
