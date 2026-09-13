<div align="center">
  <img src="assets/logo.svg" alt="gdsync Logo" width="120"/>
  <h1>gdsync</h1>
  <p><b>Git-aware real-time Google Drive sync daemon for Linux.</b></p>
  <p>
    <a href="https://github.com/sadabx/gdsync/releases"><img src="https://img.shields.io/badge/Release-v0.1.0-blue?style=flat-square" alt="Release"></a>
    <img src="https://img.shields.io/badge/Rust-1.75%2B-orange?style=flat-square&logo=rust&logoColor=white" alt="Rust">
    <img src="https://img.shields.io/badge/Platform-Linux-green?style=flat-square&logo=linux&logoColor=white" alt="Linux">
    <a href="LICENSE"><img src="https://img.shields.io/badge/License-GPL--3.0-blue.svg?style=flat-square" alt="License"></a>
  </p>
</div>

Google Drive for Desktop syncs everything indiscriminately—uploading `node_modules`, `.venv`, and build targets until your cloud storage runs out of space.

`gdsync` is a background Linux daemon written in Rust. It syncs changes to Google Drive in real time using kernel `inotify` events while evaluating your project's `.gitignore` rules via `ripgrep`'s `ignore` engine.

---

## Features

- **Respects `.gitignore`**: Automatically ignores build bloat (`node_modules/`, `target/`, `.venv/`, `.cxx/`) across root and nested subdirectories.
- **Kernel-Level Watching**: Debounces rapid file changes with `inotify` before uploading.
- **SQLite State Tracking**: Tracks paths, MD5 checksums, and Google Drive file IDs locally in `~/.config/gdsync/state.db`.
- **Resumable Chunked Uploads**: Direct asynchronous Google Drive v3 REST API implementation with automatic retry and backoff.

---

## Quick Start

### 1. Installation

```bash
# Clone & Install
git clone https://github.com/sadabx/gdsync.git
cd gdsync
cargo install --path crates/gdsync-cli
```

### 2. Authenticate

```bash
gdsync auth
```

*Opens your browser to complete Google OAuth2 PKCE login. Tokens are cached locally in `~/.config/gdsync/token.json`.*

### 3. Link & Watch a Folder

```bash
# Link your workspace to a Drive folder
gdsync init ~/Codes -d "Codes_Backup"

# (Optional) Dry-run scan to check what will sync
gdsync scan

# Run an initial two-way reconciliation
gdsync sync

# Start the real-time background watcher daemon
gdsync watch
```

---

## CLI Reference

| Command | Usage | Description |
| --- | --- | --- |
| `auth` | `gdsync auth [--client-id <ID> --client-secret <SEC>]` | Google Drive OAuth2 login |
| `init` | `gdsync init <local_path> -d <remote_folder_or_id>` | Map a local directory to Google Drive |
| `scan` | `gdsync scan [path]` | Dry-run list of files to sync (respecting `.gitignore`) |
| `sync` | `gdsync sync [path]` | One-time two-way synchronization pass |
| `watch` | `gdsync watch [path]` | Start real-time inotify watcher daemon |
| `status` | `gdsync status` | Show tracked paths, database records, and sync queue |
| `diff` | `gdsync diff <remote_folder1> <remote_folder2>` | Compare two remote Drive folders via MD5 |
| `merge` | `gdsync merge <source_folder> <target_folder>` | Non-destructively merge remote folders |

---

## Run as a Systemd Daemon

To keep `gdsync` watching your directory in the background on boot:

1. Create `~/.config/systemd/user/gdsync.service`:

```ini
[Unit]
Description=gdsync background daemon
After=network-online.target

[Service]
Type=simple
ExecStart=%h/.cargo/bin/gdsync watch
Restart=on-failure
RestartSec=5

[Install]
WantedBy=default.target
```

2. Enable and start:

```bash
systemctl --user daemon-reload
systemctl --user enable --now gdsync.service
```

---

## Storage Layout

```text
~/.config/gdsync/
├── config.toml    # Directory mappings and remote IDs
├── state.db       # SQLite WAL database (file hashes & IDs)
└── token.json     # Stored OAuth2 tokens
```

---

## License

GPL-3.0 License. See [LICENSE](LICENSE) for details.
