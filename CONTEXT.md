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
  ignorés, sans durée). Sous 1 s, pas de durée (« Thought ») : un bloc
  entier peut arriver en une trame regroupée, la durée mesurée côté client
  ne veut rien dire (`thinkingDuration`, `src/lib/primeHistory.ts`).
  Anthropic renvoie une réflexion résumée
  (`display: "summarized"`, `pa-ai/src/providers/anthropic/params.rs:107-128`),
  rien au niveau `off`. Vérifié dans l'app avec un vrai modèle.
- **Sous-agents (vus depuis le parent)** : une cellule `rlm.spawn(…)` est
  titrée « Agent · nom » (« Agent » si `name=` n'est pas un littéral) avec
  l'état de l'enfant (`get_rlm_children`, interrogé toutes les 2 s tant
  qu'un enfant tourne, `prime_rlm_children`) ; les lignes `custom`
  `agent_message` (réponse de l'enfant, `details.from.sessionName`),
  `rlm_child_terminal_notice` et `rlm_child_failure` s'affichent en direct
  et à la restauration. Les enfants ne portent pas le marquage yusAi mais
  meurent avec leur parent (`Kill` : `pa-daemon/src/worker/commands.rs:659` ;
  mort du worker : `pa-daemon/src/supervisor_parent_death.rs`), donc aussi
  au nettoyage des orphelins ; test e2e avec vrai noyau.
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

## Décisions (2026-10-02, pas encore codées)

- **Rétention : couche yusAi.** On garde `/refine` et l'auto-refine de Prime
  pour apprendre, mais on range nous-mêmes. Prime n'a que deux portées
  (session, global) et ses chemins d'écriture ne tombent pas là où le
  modèle relit (voir Pièges, « Harness de Prime »).
  - Leçons : nouvelle table de `desktop-state.sqlite3`, avec niveau
    (projet, type, global), source et historique ; réinjectées par
    `appendSystemPrompt` au `Create` (rejoué à la relance du worker,
    vérifié par test e2e).
  - Skills : dossiers dans les données de yusAi, passés par `config.skills`
    au `Create` (priorité maximale, `pa-core/src/resources/mod.rs:141-154`).
  - Une leçon arrive au niveau projet ; elle ne monte au niveau type ou
    global qu'avec la validation de Yusuf.
- **Type de projet** : sélecteur dans l'en-tête du chat Prime, prérempli par
  une suggestion tirée des fichiers du projet ; types en noms libres ; choix
  retenu par projet.
- **Déclencheur des refines** : refine locale automatique à la « fermeture »
  d'une conversation, plus un bouton « Retenir » qui la force à la demande.
  L'auto-refine de Prime seule est trop rare : elle ne part qu'après une
  compaction (`pa-daemon/src/compact_autorefine.rs:6-25`).

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
- **Réflexion** : `7d99351` (test e2e), `0941095` (affichage, restauration),
  `24ca115` (pas de durée sous 1 s).
- **Sous-agents** : `edcaaee` (venv du noyau isolé), `f496017` (tests e2e :
  `scratch_dir` unique), `60ddba1` (test e2e avec vrai noyau), `49bcfe4`
  (affichage). Non poussés : attendre que l'utilisateur ait testé dans
  l'app (dernier commit poussé : `e31abaf`).
- **Diagnostics clos** : pas de bloc de réflexion avec Sonnet 5.5 = mode
  adaptatif (le modèle décide ; niveau bien enregistré, requête bien
  envoyée, aucun jeton de réflexion facturé) ; carte « (no output yet —
  cell ran) » = le modèle avait envoyé cette phrase comme code (SyntaxError),
  pas un bug d'affichage.

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
  le fichier qu'il y envoie. Le dossier des sous-agents,
  `session-artifacts/<id de session>/` (id lu dans l'en-tête du fil, distinct
  du nom de fichier), part aussi à la Corbeille (`delete_thread`) ; Prime ne
  le touchait pas. En revanche `session-artifacts/<conversationId>/`
  (instantané du noyau) est effacé définitivement par Prime
  (`remove_dir_all`, saved_session_commands.rs:142-154).
- **Diffs perdus au redémarrage** (limite acceptée) : les fichiers modifiés
  viennent de nos photos, pas du fichier de session ; après un redémarrage
  les cartes restaurées n'ont plus leurs diffs, et une cellule `edit`
  reprend sa première ligne comme titre.
- **`*_start` n'arrive jamais au client** : le coalesceur du worker remplace
  une trame sans delta (`thinking_start`, `text_start`) par le delta suivant
  (`pa-daemon/src/streaming.rs:118-127`). Ouvrir un bloc au premier delta,
  le fermer sur `*_end`. Les deltas d'un même type s'additionnent sans
  perte (`streaming.rs:1-20`).
- **Parent sans fichier = pas d'enfant** : un parent en mémoire ne peut pas
  inscrire son enfant au registre RLM (« invalid spawn »), l'enfant est
  arrêté aussitôt. Les fils yusAi ont un fichier, ce n'est un piège que
  pour les tests.
- **Télémétrie des enfants** : Prime crée les enfants sans
  `telemetryDisabled` (`pa-daemon/src/rlm_children/lifecycle.rs:124-127`) et
  le worker ignore `PRIME_AGENT_TELEMETRY` (`pa-daemon/src/agent_engine/lifecycle.rs:1071-1085`) :
  un enfant crée `telemetry.json` (identifiant d'installation) et un client
  de télémétrie avec le miroir local `telemetry.jsonl`, actif par défaut
  (`pa-core/src/session_engine/telemetry.rs:1092-1100`). Prime compte sur la
  porte « profondeur 0 » (commentaire de `rlm_children/lifecycle.rs:127-130`),
  qui ne coupe que la télémétrie de session (`engine.rs:771`) : le crochet
  « kernel bootstrap » n'y est pas soumis (`engine.rs:386-409`). Chaque
  démarrage du noyau d'un enfant écrit donc une ligne dans
  `telemetry.jsonl`. Diagnostic du 2026-10-02 : les deux lignes de 10:16Z
  et 10:19Z suivent de 4 s la création des enfants `sub-e6bb6e0c` et
  `sub-5a60fe37` de la conversation `942e9be0…`. Les cinq lignes du 01/10
  (18:49Z) précèdent la coupure (`e81f5f3`, 20:58 heure locale). Rien ne
  part : puits nul sans point d'envoi (`telemetry.rs:1084-1091`), `telemetry`
  à `null` dans nos réglages. Le test e2e des sous-agents ne le voit pas,
  probablement parce que le client écrit par lots toutes les 10 s
  (`pa-telemetry/src/client.rs:40`) et que ses enfants vivent moins
  longtemps : son assertion « pas de `telemetry.jsonl` » est un faux
  négatif. Inévitable sans patch vendor (ou en coupant le miroir par
  `telemetry.localMirror: false` dans les réglages de Prime).
- **`uv` et le PATH** : le venv du noyau se construit avec `uv`, cherché dans
  le PATH ou `~/.local/bin` (`pa-core/src/kernel/bootstrap/venv/uv.rs:85-101`).
  En dev (lancé du terminal) il est trouvé ; une app empaquetée lancée du
  Finder a un PATH minimal et ne trouverait pas un `uv` Homebrew. À traiter
  au packaging.
- **Tests e2e en parallèle** : `scratch_dir` doit rester unique (compteur
  atomique) ; deux tests au même dossier partagent socket et daemon. Le test
  sous-agents garde son venv dans `CARGO_TARGET_TMPDIR` (première
  construction : quelques minutes) et se saute sans `uv`.
- **Test qui panique = daemon orphelin** : un test e2e en échec n'atteint
  pas son `Shutdown` ; vérifier `pgrep -fl yusai-prime-test` après un échec.
- **Harness de Prime (`/refine`) : chemins incohérents dans le worker**
  (test e2e `refine_runs_scripted_and_append_system_prompt_survives_a_worker_restart`,
  planificateur scripté par le moteur `faux`) :
  - refine locale → `<agent_dir>/yusai-threads/harness/harness_state.json`,
    commun à tous nos fils (`pa-core/src/session_engine/refine.rs:237-240`,
    `pa-daemon/src/agent_engine/lifecycle.rs:1044-1063`) ; le digest du
    prompt lit `session-artifacts/<fil>/harness/`
    (`pa-core/src/session_engine/engine.rs:363-377`) ;
  - refine globale → `<agent_dir>/harness_state.json` et
    `refinement_history.jsonl`, sans `harness/`
    (`pa-daemon/src/agent_engine/session_engine_impl.rs:1289`) ; le digest et
    le noyau lisent `<agent_dir>/harness/` (`engine.rs:548`,
    `prime-agent-runtime/src/rlm/harness.py:166`) ;
  - `rlm.harness.*` local depuis le noyau échoue (aucun `RLM_SESSION_DIR`,
    `pa-core/src/session_engine/runtime_wiring.rs:242-246`) ;
  - le fil ne garde que `refinement_outcome` / `refinement_notice` ; l'audit
    `prime-agent.refinement` (base du retour arrière) reste en mémoire :
    l'historique local meurt avec le worker.
  D'où la décision de ranger nous-mêmes. `appendSystemPrompt` est, lui,
  rejoué à la relance d'un worker tué (`Create` durable,
  `pa-daemon/src/supervisor/worker_lifecycle.rs:235-252`).
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
3. **Sous-agents, étape C** : vue détaillée d'un enfant (Attach à sa
   session, son fil et ses appels d'outils, Detach à la fermeture). Les
   enfants meurent avec le parent : après un redémarrage, il faudrait
   rouvrir leur fichier (`<agent_dir>/session-artifacts/<session du
   parent>/<enfant>/`, `pa-daemon/src/rlm_children.rs:869-882`).
   Renommer « Sinew » en « yusAi ».
4. Fermer une seule fenêtre ne tue pas ses sessions avant la sortie de l'app.
   Inversement, deux fenêtres sur la même conversation partagent la session :
   en fermer une la tue, l'autre la rouvre depuis le fichier.
