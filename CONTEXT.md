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
  (`ensure_daemon_running`, calqué sur pa-cli). Exception acceptée par
  Yusuf (2026-10-02) : lancé dès le démarrage s'il y a des refines à faire
  (voir « Couche de rétention »).
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
  fichier). Une session fermée par un `Kill` (`sessionClosed`) est rouverte
  depuis son fichier. La mise en veille de Prime (90 min) ne touche pas nos
  fils tant que yusAi tourne : voir Pièges, « Mise en veille ». Supprimer une conversation tue le worker du fil puis appelle
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
  au `Create` + `telemetry.localMirror: false` écrit dans
  `<agent_dir>/settings.json` sous le verrou de Prime à chaque
  `ensure_daemon_running` (`disable_telemetry_mirror`, pour les sous-agents).
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
- **Fermeture d'une conversation** (2026-10-02, délai choisi par Claude à la
  demande de Yusuf) : 10 min sans affichage dans aucune fenêtre ET au moins
  3 nouveaux tours utilisateur depuis la dernière refine ; ou mise en veille
  du worker (90 min) ET au moins 1 nouveau tour. Après Cmd+Q, `pending = 1` ;
  la refine part au prochain démarrage de l'app, en arrière-plan, une à la
  fois (pas à la réouverture de la conversation).
- **Mise en veille = notre propre minuteur** (2026-10-02, validé par
  Yusuf), car celle de Prime ne touche jamais nos fils attachés et ne
  prévient pas le client (Pièges, « Mise en veille ») : conversation non
  affichée et sans activité depuis 90 min → refine si au moins 1 nouveau
  tour, puis `Kill` du worker par nous ; le fil se rouvre depuis son fichier
  au prochain affichage.
  - « Sans activité » = aucun tour en cours ET aucun sous-agent vivant.
  - Si la refine échoue, on tue quand même le worker et on garde
    `pending = 1`.
  - Test e2e attendu (commit 8) : après notre `Kill`, un `Create` avec
    `session_path` rouvre le fil (état `archived`) avec modèle et niveau de
    réflexion restaurés.
  - Effet de bord voulu : le minuteur évite de garder un worker et un noyau
    Python vivants par conversation ouverte depuis le démarrage de l'app.
- **Plan de la couche yusAi validé** (2026-10-02, détails tranchés par
  Claude à la demande de Yusuf) :
  - écritures globales du modèle (`rlm.harness.*(…, global_=True)`) :
    importées en niveau projet, plus une proposition de montée vers global ;
  - harness local partagé (`yusai-threads/harness/`) : on y amorce nos
    leçons avant une refine et on ne retire ensuite que nos entrées `yl_…`,
    sans vider le fichier ;
  - la garde contre les écritures globales vient juste après la capture des
    refines (commit 4 du plan) ;
  - consigne `skill-creator` dans `appendSystemPrompt` (où créer une skill) ;
  - projet identifié par son chemin (`workspace_id`) : limite acceptée, un
    dossier déplacé perd ses leçons et son type ;
  - injection limitée à 300 caractères par leçon et 4 000 au total.
  - Ordre des commits : 1 magasin (migration v10) ; 2 conversion des edits ;
    3 capture et rattrapage ; 4 garde globale ; 5 amorçage et file des
    refines ; 6 injection ; 7 « Retenir » ; 8 fermeture ; 9 type de projet ;
    10 validation ; 11 skills par niveau.

## Fait récemment (commits)

- **Couche de rétention, en cours** : magasin des leçons (`db14f96`,
  `crates/sinew-app/src/store/lessons.rs`, sqlite v10) ; conversion des
  refines (`41622b1`, `src-tauri/src/prime_lessons.rs`) ; capture : le
  relais importe chaque `refinement_outcome` en direct
  (`import_live_refinement`), l'ouverture d'un fil rattrape celles de son
  historique (`catch_up_refinements`). Import unique par `refinementId` ;
  une refine globale ajoute une proposition de montée ; après un import
  réussi, les entrées créées par la refine quittent le harness de Prime
  (local : `yusai-threads/harness/`, global : `agent_dir`). Une opération
  qui échoue garde son entrée dans le harness et est notée avec la refine
  (`ImportedRefinement::failures`, sqlite v11). Garde globale : après chaque
  cellule, les écritures directes du modèle dans `<agent_dir>/harness/`
  (`rlm.harness.*(…, global_=True)`, que Prime réinjecte partout)
  deviennent des leçons projet de la conversation de la cellule, proposées
  pour le global, puis quittent le fichier (`import_global_harness_writes`,
  test e2e avec vrai noyau). Limites : deux cellules finies au même moment
  dans deux conversations, la première arrivée prend les entrées ; une
  session qui démarre entre l'écriture et la garde voit l'entrée dans son
  digest. File des refines (`src-tauri/src/prime_refine.rs`, `run_refine`) :
  une refine à la fois dans toute l'app ; avant, les leçons applicables
  (projet, type, global) sont amorcées dans le harness local partagé
  (`seed_thread_lessons`, niveau lisible dans le `path` :
  `yusai/project`…), pour que le planificateur puisse les modifier ; après,
  import, retrait de nos `yl_…` même en cas d'échec, puis `mark_refined`
  si la refine a réussi (un échec ne touche pas `pending` : à l'appelant de
  décider). Pas encore d'appelant : « Retenir » (commit 7) et la fermeture
  (commit 8). Injection (`src-tauri/src/prime_guidance.rs`) : à chaque
  ouverture d'un fil, `appendSystemPrompt` porte une puce de consignes
  (retenir avec `await refine.run(…)` sans `global_=True`, jamais
  `rlm.harness.*` ; skills du projet dans
  `<données>/prime-skills/projects/<hash du chemin>/`, que `config.skills`
  ne charge qu'au commit 11), puis une puce par leçon
  (`[projet · fait] Titre : contenu`, 300 caractères au plus), 4 000 au
  total, et une puce qui compte les leçons restées dehors. Figé au
  `Create` : une leçon nouvelle n'arrive qu'à la réouverture du fil. Bouton
  « Remember » (« Retenir », en-tête du chat Prime, commande `prime_retain`) :
  refine locale par la file, instructions facultatives, résumé dans
  l'en-tête ; désactivé pendant un tour. L'historique des leçons note
  l'auteur `refine:retain` ; pendant une refine de la file, le relais
  n'importe rien pour la session (`refine_in_flight`), la file importe la
  sienne puis rattrape les autres. Vérifié dans un banc d'essai navigateur
  (IPC simulé), pas encore dans l'app. Fermeture (`src-tauri/src/prime_close.rs`) :
  le panneau signale son affichage par fenêtre (`prime_set_displayed`, une
  fenêtre détruite n'affiche plus rien), le relais suit les tours
  (`agent_start` / `agent_end`), `prime_prompt` compte les tours
  utilisateur (`note_user_turn`). Toutes les 60 s, un relevé décide
  (`close_action`) : 10 min cachée et 3 tours → refine `refine:close` ;
  90 min cachée et sans activité (ni tour, ni sous-agent `running`) →
  refine s'il y a un tour, puis `Kill` (sauf si la conversation a repris
  entre-temps). Refine ratée → `pending = 1`, et pas de nouvel essai de
  fermeture courte avant un prompt. Test e2e
  `closing_refines_then_puts_the_worker_to_sleep_and_the_thread_reopens`. Cmd+Q
  (`on_exit`) : les conversations ouvertes qui ont de nouveaux tours passent
  à `pending = 1` (`defer_refine_if_unrefined`) ; au démarrage suivant
  (`refine_pending_at_startup`), ces conversations et toute conversation
  qui a un tour non retenu (`refines_due_at_start` : couvre Ctrl+C et les
  plantages, sans `on_exit`) passent une à une en arrière-plan : refine sur la
  session de l'UI si elle a déjà rouvert la conversation, sinon fil rouvert,
  refiné puis tué (sauf si l'UI l'a rejoint entre-temps). Conversation, projet
  ou fil introuvable : état de refine oublié ; refine ratée : attente gardée, nouvel
  essai à chaque démarrage. Test e2e `deferred_refines_run_at_the_next_start`. Type de
  projet (`src-tauri/src/prime_project.rs`) : sélecteur dans l'en-tête du
  chat Prime (types connus, « No type », « Other type… » en texte libre),
  relu à chaque affichage du panneau. Suggestion tirée des fichiers à la
  racine (`tauri.conf.json`, `Cargo.toml`, `package.json` et son framework,
  `pyproject.toml`…), recalculée tant que l'utilisateur n'a pas choisi,
  affichée en italique ; la choisir la confirme. Seul un type confirmé
  compte pour les leçons (`confirmed_project_type` : injection, amorçage,
  import) ; il est pris en compte à la prochaine ouverture d'un fil et à la
  prochaine refine. Vue « Lessons » (commit 10, lecture :
  `src-tauri/src/prime_review.rs`) : sqlite v12 garde avec chaque refine
  importée son projet, son déclencheur, son résumé, ses edits écartées et
  son annulation, et le projet de chaque proposition ; les refines d'avant
  la v12 sont retrouvées par leur conversation, sans résumé. Vue (`src/components/chat/PrimeLessonsView.tsx`) :
  bouton « Lessons » de l'en-tête (badge : propositions en attente de tous
  les projets, relu toutes les 60 s et après « Remember »), qui remplace le
  fil dans la colonne ; onglets Review, Lessons (par niveau, historique
  dépliable, « Not injected », « Show archived ») et Refines (déclencheur,
  résumé, compteurs, détail dépliable). Actions (`prime_review.rs`, auteur
  `user`) : accepter (montée vers type ou global, changement, archivage) ou
  refuser une proposition (refus noté dans l'historique ; les skills
  attendent le commit 11) ; éditer, épingler, archiver, restaurer une
  leçon, changer son niveau (vaut validation ; le niveau type prend le
  type confirmé du projet de la leçon) ; une décision ferme les
  propositions qu'elle rend caduques. « Undo » sur une refine : archive
  ses leçons créées, remet l'ancien texte de celles qu'elle a modifiées
  (sauf retouchées depuis), restaure celles qu'elle a archivées, refuse
  ses propositions en attente, et la marque annulée. Vérifiée dans le
  banc d'essai, pas encore dans l'app.
- **Skills par niveau, 11a** (`src-tauri/src/prime_skills.rs`) : dossiers
  `<données>/prime-skills/{projects/<hash du chemin>, types/<hash du nom
  normalisé>, global}/<skill>/SKILL.md`, `archive/` hors du relevé ; un
  `.yusai-owner` nomme le projet ou le type d'un dossier. Au `Create`
  (`thread_guidance`), `config.skills` reçoit chaque `SKILL.md` un par un :
  projet, type confirmé, global (premier nom gagnant chez Prime, donc le
  projet masque le global). Conflit Python (venv partagé, installation
  éditable par nom d'import et chemin) : une skill Python de yusAi doit être
  seule sur son nom d'import et sa distribution (`[project] name`) dans
  tout `prime-skills/` et parmi les skills que Prime charge lui-même pour le
  projet (résolution de Prime sans installer de paquet : `<agent_dir>/skills/`,
  `~/.agents/skills/`, intégrées, `.prime/agent/skills/`, `.agents/skills/`
  du projet et de ses parents jusqu'à la racine git, tableaux `skills` des
  réglages, paquets). Prime gagne toujours ; entre les nôtres, niveau le
  plus haut puis dossier le plus ancien. Une perdante n'est pas passée et,
  au niveau projet, a sa puce dans `appendSystemPrompt` (raison, consigne
  de la renommer). Tests e2e : skills visibles dans la session
  (`get_resource_snapshot`, après une première lecture du prompt système :
  la session se construit paresseusement), skill Python importée par une
  cellule avec vrai noyau.
- **Skills par niveau, 11b** (`prime_review.rs`, `prime_skills.rs`) :
  accepter une proposition de skill (« To project / To type / To global »
  dans Review). L'entrée `skill` du harness n'a qu'une référence Python et
  un texte. Si l'import est une skill Python de yusAi visible du projet :
  gardée (déplacée si un niveau est choisi). Sinon : skill markdown écrite
  au niveau choisi, front matter `metadata.yusai-proposal: <id>`, avec la
  forme d'appel et les arguments si l'import existe chez Prime (nommée
  `<nom>-usage` si une skill de Prime porte déjà ce nom, pour ne pas la
  masquer), sinon la procédure seule et une note « module pas installé ».
  Proposition de suppression : archive la skill visée. Ce que l'acceptation
  a fait reste dans `payload.accepted` (`written`, `existing`, `movedTo`,
  `archived`), sans migration. Changer de niveau, archiver
  (`archive/<ms>-<nom>/` + `.yusai-archived`), restaurer : refusés si le nom
  est pris au niveau visé ou si un nom Python est pris ailleurs (archive
  exclue). « Undo » d'une refine : archive la skill écrite (retrouvée par
  son id de proposition, même déplacée), restaure une skill archivée,
  laisse une skill qui existait avant. Commandes `prime_set_skill_level`,
  `prime_archive_skill`, `prime_restore_skill` (vue : 11c).
- **Skills par niveau, 11c** : onglet « Skills » de la vue Lessons
  (`PrimeLessonsView.tsx`) : skills des niveaux du projet (projet, type
  confirmé, global) dans l'ordre de `config.skills`, Python ou Markdown,
  « Disabled » et la raison (conflit de nom Python, en anglais :
  `Conflict::english`), « From a refine » ; actions To project / To type /
  To global, Show folder, Archive ; « Show archived » : skills archivées
  de ces niveaux, Restore. Les messages de refus d'une action restent
  affichés après la relecture de la vue (avant : effacés aussitôt, pour
  toutes les actions de la vue). Vérifié dans le banc d'essai.
- **Skills : masquage** : une skill de yusAi (même markdown) qui porte le
  nom d'une skill Python chargée après elle dans la session (une skill de
  Prime, toujours après les nôtres, ou une des nôtres d'un niveau suivant)
  la masque : Prime garde le premier nom (`pa-core/src/skills/loader.rs:93-104`)
  et ne pré-importe que les skills Python restées
  (`pa-core/src/session_engine/engine.rs:318`), la fonction disparaît du
  noyau (`NameError`, vérifié par e2e). Traité comme un conflit
  (`SharedName::Skill`) : skill écartée, « Disabled », puce de renommage.
  Masquer une skill markdown reste permis. Ce que yusAi range lui-même ne
  prend jamais le nom d'une skill Python (refus ; une skill acceptée
  devient `<nom>-usage`).
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
  à `null` dans nos réglages. Corrigé : `disable_telemetry_mirror` écrit
  `telemetry.localMirror: false` (lu par `build_client` à chaque
  construction de session d'un worker). L'ancien test e2e ne voyait rien
  parce que le client écrit par lots toutes les 10 s
  (`pa-telemetry/src/client.rs:40`) ; il attend maintenant un lot pendant
  que l'enfant vit (vérifié : sans le correctif, il échoue). `telemetry.json`
  (identifiant d'installation) reste créé par les enfants.
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
  Par le superviseur, la requête `Refine` a la limite courte de 30 s
  (`pa-daemon/src/supervisor/routing.rs:648-661`) ; au-delà, le client
  reçoit « Session worker timed out » mais le worker poursuit la refine, et
  une refine qui échoue ensuite ne laisse aucune trace
  (`pa-daemon/src/session_custom.rs:298-306`). Notre file ouvre donc sa
  propre connexion et passe directement au worker (`upgrade_direct`,
  `pa-tui/src/daemon_client.rs:872`) : vraie réponse, réussie ou non, quelle
  que soit la durée, et erreur immédiate si le worker meurt (tests e2e
  `refines_longer_than_the_supervisor_route_get_their_answer`,
  `a_worker_killed_during_a_refine_frees_the_queue_at_once`, `delayMs` du
  moteur faux). Si le lien direct est refusé, repli par le superviseur puis
  relecture du fil (`GetMessages`) jusqu'à la nouvelle ligne
  `refinement_outcome`, dans la limite de 10 min.
  Refine et tour se chevauchent sans s'attendre, dans les deux sens (test
  e2e `a_refine_and_a_turn_can_overlap`) : un prompt envoyé pendant une
  refine est admis et son tour se joue aussitôt ; une refine lancée pendant
  un tour part tout de suite. Le planificateur ne voit que le fil d'avant
  son départ ; aucune ligne ne se perd.
  Une refine lancée par le modèle (`await refine.run(…)` dans une cellule)
  s'applique à la fin du tour et ne laisse dans le fil qu'une ligne
  `refinement_notice` (`source: "self"`), seulement si une edit s'applique
  (`pa-daemon/src/agent_engine/turn/boundary.rs:216-229`) : la capture lit
  donc aussi les notices (auteur `refine:agent`, « Model »), unique par
  `refinementId`. `rlm.harness.create_*` en local échoue (pas de
  `RLM_SESSION_DIR`) et l'erreur suggère `global_=True`. Au démarrage,
  avant les refines en attente, `import_all_thread_outcomes` importe les
  refines jamais importées de tous les fils (conversation = nom du fichier,
  projet = table des conversations, sinon `cwd` du fil), sans daemon.
  Une refine d'une autre conversation peut voir nos `yl_…` amorcés : son
  import ignore les edits sur des leçons qui ne s'appliquent pas à sa
  conversation (`LessonTarget::Elsewhere`).
  Une mise à jour sans titre est refusée par Prime
  (`pa-core/src/refinement/planner.rs:223-225`).
  Limite restante de notre file de refines : l'auto-refine de Prime (après
  compaction) et `refine.run()` appelé par le modèle passent hors de notre
  file. Ils peuvent lire les entrées `yl_…` amorcées pour une refine d'un
  autre projet en cours, ou écrire dans le fichier partagé en même temps.
- **Mise en veille des workers (idle passivation)** : vérifié par une
  expérience (veille réglée à 1 min, moteur faux), le 2026-10-02.
  - Elle n'arrive que si aucun client n'est attaché
    (`pa-daemon/src/worker/turn.rs:358-366`). Notre client fait `Attach` à
    l'ouverture et jamais `Detach` : tant que yusAi tourne, nos fils ne sont
    jamais mis en veille (session encore `live` après 80 s).
  - Après un `Detach`, la mise en veille arrive. Le client ne reçoit aucun
    `session_closed` (le `Shutdown` n'en émet pas ; seul `Kill` le fait,
    `pa-daemon/src/worker/commands.rs:738`) ; juste un `HeartbeatsChanged`.
    La session disparaît de `List`.
  - `Refine` ne réveille pas un worker en veille : seuls les prompts,
    `Attach`, `Reattach`, `WaitForIdle` le font
    (`pa-daemon/src/supervisor/routing.rs:422-441`). Et même `Attach`
    échoue sur nos fils (« Unknown active session ») : le réveil ne retrouve
    pas un fichier hors de `sessions/`. Pour refiner, il faut rouvrir le fil
    (`open_thread`, nouveau worker, nouvel id).
- **Skills de yusAi, limites acceptées** :
  - descendre une skill Python (global → projet) pendant qu'un noyau d'un
    autre projet tourne casse l'import dans ce noyau : son chemin éditable
    a disparu ; le fil retrouve la skill (ou la perd) à sa réouverture ;
  - un fil ouvert garde la liste de skills de son `Create` ;
  - les dépôts des autres projets ne sont pas relus : une skill Python de
    `.prime/agent/skills/` d'un autre dépôt peut encore déloger une des
    nôtres (même limite que Prime seul entre deux dépôts) ;
  - changer le type d'un projet ne renomme pas le type : leçons et skills
    de l'ancien type restent à ce type (pour les autres projets qui l'ont).
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
