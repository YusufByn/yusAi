## Intégration Prime Agent (branche prime-integration)

- Prime est vendored dans `vendor/prime-agent/` via git subtree. Ne jamais rien y modifier.
- Un seul binaire : le superviseur et les workers Prime tournent dans notre binaire
  (multi-rôle, `src-tauri/src/prime.rs`), jamais en sidecar.
- `pa-cli` n'est jamais compilé. `pa-tui` sert uniquement pour `DaemonClient`.
- Le chat parle au daemon en protocole natif (`DaemonClient` + `pa_types::daemon::DaemonCommand`),
  pas en ACP.
- Le `[patch]` crossterm du `Cargo.toml` racine doit rester identique à celui de
  `vendor/prime-agent/Cargo.toml`.
- Toute affirmation sur le code de Prime : donner chemin et ligne.
- Petits commits, ne pas push sans demande.
