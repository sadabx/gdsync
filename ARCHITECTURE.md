# Architecture Specification: gdsync (CLI Daemon)

## 1. Workspace Layout
A modular Cargo workspace focused purely on the CLI and background sync daemon:

```text
gdsync/
├── Cargo.toml               # Workspace manifest
├── ARCHITECTURE.md          # System specification
└── crates/
    ├── gdsync-core/         # Core sync library
    │   ├── Cargo.toml
    │   └── src/
    │       ├── lib.rs
    │       ├── auth/        # OAuth2 token exchange & ~/.config/gdsync/token.json
    │       ├── drive/       # Google Drive v3 REST client (MD5 checksum, resumable uploads)
    │       ├── db/          # SQLite state database (local path <-> remote ID mapping)
    │       ├── filter/      # Gitignore engine (`ignore` crate traversal & matcher)
    │       ├── watcher/     # Inotify watcher via `notify-debouncer-full`
    │       └── sync/        # Two-way sync coordinator & conflict resolution
    └── gdsync-cli/          # Terminal interface
        ├── Cargo.toml
        └── src/
            └── main.rs      # `clap` CLI commands (auth, init, scan, status, watch)
```

## 2. CLI Subcommands

* `gdsync auth`: Run OAuth2 PKCE login in browser and save token.
* `gdsync init <path>`: Link a local directory to a remote Google Drive folder.
* `gdsync scan`: Traverse target folder using `ignore` and print all unignored files that would sync.
* `gdsync sync`: Run a one-time push/pull reconciliation pass.
* `gdsync watch`: Start the long-running background inotify daemon.
* `gdsync status`: Inspect sync queue and database health.

## 3. Storage & Config

All configuration and cache files reside in:
`~/.config/gdsync/`

* `config.toml`: Watched directories and remote root folder IDs.
* `state.db`: SQLite database for paths, hashes, and Drive IDs.
* `token.json`: Encrypted/restricted OAuth tokens.
