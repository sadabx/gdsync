<div align="center">
  <img src="assets/logo.svg" alt="gdsync Logo" width="120"/>
  <h1>gdsync</h1>
  <p><b>Git-aware real-time Google Drive sync daemon for Linux.</b></p>
  <p>
    <a href="https://github.com/sadabx/gdsync/releases"><img src="https://img.shields.io/badge/Release-v0.2.0-blue?style=flat-square" alt="Release"></a>
    <img src="https://img.shields.io/badge/Rust-1.75%2B-orange?style=flat-square&logo=rust&logoColor=white" alt="Rust">
    <img src="https://img.shields.io/badge/Platform-Linux-green?style=flat-square&logo=linux&logoColor=white" alt="Linux">
    <a href="LICENSE"><img src="https://img.shields.io/badge/License-GPL--3.0-blue.svg?style=flat-square" alt="License"></a>
  </p>
</div>

Google Drive for Desktop syncs everything indiscriminately—uploading `node_modules`, `.venv`, and build targets until your cloud storage runs out of space.

`gdsync` is a background Linux daemon written in Rust. It syncs changes to Google Drive in real time using kernel `inotify` events while evaluating your project's `.gitignore` and `.gdsyncignore` rules via `ripgrep`'s `ignore` engine.

---

## Features

- **Interactive TUI Environment**: Run bare `gdsync` for a Claude Code-inspired terminal dashboard with live Google Drive connection status, tracked directories, and arrow-key action menus.
- **Respects `.gitignore` & `.gdsyncignore`**: Automatically ignores build bloat (`node_modules/`, `target/`, `.venv/`, `.cxx/`) across root and nested subdirectories. Custom `.gdsyncignore` works in non-Git folders.
- **Concurrent Transfers & Progress Bars**: Multi-threaded uploads/downloads with bounded worker pools and real-time progress bars (`indicatif`).
- **Zero-Data-Loss Safe Trashing**: Local deletions move remote files to Google Drive's Trash (recoverable for 30 days) instead of permanently purging them.
- **Kernel-Level Watching**: Debounces rapid file changes with `inotify` before uploading.
- **SQLite State Tracking**: Tracks paths, MD5 checksums, and Google Drive file IDs locally in `~/.config/gdsync/state.db`.
- **Automatic Conflict Preservation**: Detects offline two-way edits and preserves local modifications as `<file>.conflict-<timestamp>.<ext>` while pulling remote changes with zero data loss.
- **Cascading Deletion Safety Guard**: Automatically blocks mass deletions (>20% or >50 files) if an external drive or network share unmounts unexpectedly (bypassable with `--force`).
- **Resumable Chunked Uploads**: Direct asynchronous Google Drive v3 REST API implementation with automatic exponential backoff retry and chunk recovery.
- **Built-in Systemd Daemon & Shell Completions**: Integrated service helper and auto-completions for Bash, Zsh, and Fish.

---

## Quick Start

### 1. Installation

```bash
# Clone & Install
git clone https://github.com/sadabx/gdsync.git
cd gdsync
cargo install --path crates/gdsync-cli
```

### 2. Interactive Environment or Direct CLI

You can run `gdsync` by itself at any time to enter the interactive TUI menu:

```bash
gdsync
```

Or execute commands directly:

### 3. Authenticate

```bash
gdsync auth
```

*Opens your browser to complete Google OAuth2 PKCE login. Tokens are cached locally in `~/.config/gdsync/token.json`.*

<details>
<summary><b>(Optional) Using your own Google Cloud credentials for dedicated quota</b></summary>
<br/>

`gdsync` works out of the box with zero setup using pre-registered public desktop credentials. If you perform massive initial syncs or want private, unthrottled API quota (12,000 requests/min):

1. Open [Google Cloud Console](https://console.cloud.google.com/) and click **New Project** (name it `gdsync`).
2. Go to **APIs & Services > Library**, search for **Google Drive API**, and click **Enable**.
3. Go to **APIs & Services > OAuth consent screen**:
   - Select **External**, enter an App name (`gdsync`) and your email.
   - Under **Test users**, add your Google email address.
4. Go to **APIs & Services > Credentials**:
   - Click **Create Credentials > OAuth client ID**.
   - Choose Application type: **Desktop app**.
   - Copy your **Client ID** and **Client Secret**.
5. Run authentication with your keys:
   ```bash
   gdsync auth --client-id "YOUR_CLIENT_ID" --client-secret "YOUR_CLIENT_SECRET"
   ```
Your credentials will be stored in `~/.config/gdsync/config.toml` so you never need to re-enter them.
</details>

### 4. Link & Watch a Folder

```bash
# Link your workspace to a Drive folder
gdsync init ~/Codes -d "Codes_Backup"

# (Optional) Dry-run scan to preview what will sync
gdsync scan

# (Optional) Preview sync plan without modifying anything
gdsync sync --dry-run

# Run an initial two-way reconciliation (with 4 concurrent workers)
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
| `sync` | `gdsync sync [path] [--dry-run] [--concurrency <N>] [--force]` | Two-way reconciliation with progress bars & safety guards |
| `watch` | `gdsync watch [path] [--notify] [--force]` | Start real-time inotify watcher daemon |
| `status` | `gdsync status` | Show tracked paths, database records, and sync queue |
| `diff` | `gdsync diff <remote_folder1> <remote_folder2>` | Compare two remote Drive folders via MD5 |
| `merge` | `gdsync merge <source_folder> <target_folder>` | Non-destructively merge remote folders |
| `service` | `gdsync service <install\|status\|start\|stop\|logs>` | Manage background systemd user service |
| `completions` | `gdsync completions <bash\|zsh\|fish>` | Generate shell auto-completions |

---

## Run as a Background Service

### Option A: Automatic (Built-in Helper)

Install, enable, and start `gdsync` with a single command:

```bash
# Install and start systemd user service
gdsync service install

# Check service status
gdsync service status

# Follow live daemon logs
gdsync service logs
```

### Option B: Manual Systemd Unit

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

## Shell Completions

Generate shell completions for your shell:

```bash
# Zsh
gdsync completions zsh > ~/.zfunc/_gdsync

# Bash
gdsync completions bash > ~/.local/share/bash-completion/completions/gdsync

# Fish
gdsync completions fish > ~/.config/fish/completions/gdsync.fish
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