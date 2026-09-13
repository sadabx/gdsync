<div align="center">
  <img src="assets/logo.svg" alt="gdsync Logo" width="160"/>
  <h1>gdsync</h1>
  <p><b>Git-Aware Realtime Google Drive Sync Daemon for Linux</b></p>
  <p>
    <img src="https://img.shields.io/badge/Rust-1.75%2B-orange?logo=rust&logoColor=white" alt="Rust">
    <img src="https://img.shields.io/badge/Platform-Linux%20(Arch)-blue?logo=archlinux&logoColor=white" alt="Arch Linux">
    <img src="https://img.shields.io/badge/Google%20Drive-v3%20REST%20API-4285F4?logo=googledrive&logoColor=white" alt="Google Drive">
    <img src="https://img.shields.io/badge/Storage-SQLite%20(WAL)-003B57?logo=sqlite&logoColor=white" alt="SQLite">
    <img src="https://img.shields.io/badge/Inotify-Debounced-0F9D58" alt="Inotify">
    <a href="LICENSE"><img src="https://img.shields.io/badge/License-GPL--3.0-blue.svg" alt="GPL-3.0 License"></a>
  </p>
</div>

## Index

1. [What is gdsync?](#what-is-gdsync)
2. [Features](#features)
3. [Getting Started](#getting-started)
4. [CLI Command Reference](#cli-command-reference)
5. [Architecture & Storage](#architecture--storage)
6. [Building from Source](#building-from-source)
7. [Visual Style & Terminal Banner](#visual-style--terminal-banner)
8. [Support](#support)
9. [License](#license)

---

## What is gdsync?

**gdsync** is a high-performance, headless Linux daemon and CLI tool written in Rust that synchronizes local directories with Google Drive in real time while strictly respecting root and nested `.gitignore` rules.

Traditional cloud sync clients often indiscriminately upload multi-gigabyte build folders like `node_modules/`, `target/`, `.venv/`, or compiled artifacts. **gdsync** solves this by integrating ripgrep's battle-tested `ignore` engine directly into the synchronization pipeline. With kernel-level `inotify` file watching, an embedded SQLite state cache, and resilient Google Drive v3 chunked uploads, **gdsync** keeps your remote backups clean, fast, and light.

---

## Features

### Git-Aware Intelligence
- **Nested `.gitignore` Traversal**: Uses the `ignore` crate (`WalkBuilder` and `GitignoreBuilder`) to evaluate `.gitignore` patterns dynamically across deeply nested directories.
- **Fail-Safe Blacklists**: Hardcoded safety filters ensure `.git/`, `.cargo/`, `node_modules/`, `target/`, `venv/`, and transient editor lockfiles are never synchronized.
- **Dry-Run Inspection**: Preview exactly what files will be synced and calculate remote payload sizes before committing to any network operations.

### High-Performance Linux Daemon
- **Kernel `inotify` Monitoring**: Watches filesystem operations in real time using `notify-debouncer-full`.
- **Intelligent Debouncing**: Consolidates rapid disk writes (such as IDE auto-saves or batch compilers) before triggering uploads.
- **Embedded SQLite Engine (WAL Mode)**: Fast local state database at `~/.config/gdsync/state.db` that tracks paths, remote Google Drive IDs, MD5 checksums, and sync timestamps without locks.

### Resilient Cloud Synchronization
- **Google Drive v3 REST API**: Built on direct asynchronous HTTP requests (`reqwest` + `rustls`) with zero heavy SDK bloat.
- **Resumable Chunked Uploads**: Splits large files into 5 MiB chunks with HTTP 308 resume recovery for rock-solid network stability.
- **Exponential Backoff & Jitter**: Automatically handles Google API rate limits (`403 rateLimitExceeded` / `Queries per minute`) with graceful backoff and retry.
- **Headless OAuth2 PKCE**: Easy terminal login with a local loopback server (`http://localhost:8085/callback`) and automatic token refresh.

### Remote Inspection & Conflict Resolution
- **Remote Folder Diffing (`gdsync diff`)**: Recursively inspects and compares two Google Drive folders, reporting identical files (MD5 verified), unique items, and conflicts.
- **Non-Destructive Remote Merging (`gdsync merge`)**: Safely merges content between remote folders without duplicating existing files.

---

## Getting Started

### Prerequisites
- **Linux** (Optimized for Arch Linux / systemd distributions)
- **Google Account** (Personal Google Cloud OAuth2 Client ID/Secret or built-in credentials)
- **Rust 1.75+** (if compiling from source)

### Installation

#### Quick Install (Cargo)
```bash
cargo install --path crates/gdsync-cli
```
This installs the `gdsync` binary directly to `~/.cargo/bin/gdsync`. Ensure `~/.cargo/bin` is in your `$PATH`.

#### Arch Linux / Local Binary
You can also copy the compiled release binary directly to your local user path:
```bash
cp target/release/gdsync ~/.local/bin/
```

---

### Basic Usage

#### 1. Authenticate with Google Drive
```bash
gdsync auth
```
This spins up a temporary loopback receiver at `http://localhost:8085` and opens your default browser via `xdg-open` for OAuth2 consent. Tokens are stored securely in `~/.config/gdsync/token.json`.

> [!TIP]
> To use your own Google Cloud Project credentials, pass `--client-id` and `--client-secret`:
> ```bash
> gdsync auth --client-id "<YOUR_ID>.apps.googleusercontent.com" --client-secret "<YOUR_SECRET>"
> ```

#### 2. Initialize Directory Mapping
You can map a local directory to a named Drive folder or an existing Google Drive Folder ID:
```bash
# Link by folder name (creates remote folder if missing)
gdsync init /home/user/MyProject -d "MyProject"

# Or link directly to an existing Google Drive Folder ID
gdsync init /home/user/Pictures -d "1nmuidhjfNhT6CoFwaUO6L3y6b3c2lmzh"
```

#### 3. Preview Files (`scan`)
Verify that `.gitignore` rules are honored:
```bash
gdsync scan
```

#### 4. Run One-Time Synchronization (`sync`)
Perform a full two-way reconciliation pass:
```bash
gdsync sync
```

#### 5. Start Background Realtime Watcher (`watch`)
Start the continuous sync daemon:
```bash
gdsync watch
```

---

## CLI Command Reference

| Command | Arguments | Description |
| :--- | :--- | :--- |
| `auth` | `[--client-id] [--client-secret]` | Authenticate with Google Drive via OAuth2 PKCE |
| `init` | `<path> -d <folder_name_or_id>` | Map a local directory to a Google Drive folder |
| `scan` | `[path]` | Dry-run scan of local files respecting `.gitignore` |
| `sync` | `[path]` | Perform a one-time two-way synchronization pass |
| `watch` | `[path]` | Run inotify daemon for continuous real-time sync |
| `status` | — | Display tracked directories, SQLite records, and sync status |
| `diff` | `<folder1> <folder2>` | Compare two Google Drive folders recursively (MD5 comparison) |
| `merge` | `<folder1> <folder2>` | Non-destructively merge files from folder 1 into folder 2 |

---

## Architecture & Storage

All local configuration and cache files reside under `~/.config/gdsync/`:

```text
~/.config/gdsync/
├── config.toml    # Directory mappings, remote folder IDs, debounce intervals
├── state.db       # SQLite WAL database (paths, remote IDs, MD5 hashes, mtime)
└── token.json     # Encrypted/secured OAuth2 refresh and access tokens
```

### Modular Crates
- **`crates/gdsync-core`**: Reusable core library containing the Drive client, filesystem scanner, inotify listener, and SQLite sync coordinator.
- **`crates/gdsync-cli`**: Terminal application binary powered by `clap`.

For in-depth internals, see [ARCHITECTURE.md](ARCHITECTURE.md).

---

## Building from Source

```bash
# Clone the repository
git clone https://github.com/sadabx/gdsync.git
cd gdsync

# Build debug binaries
cargo build

# Build optimized release binary
cargo build --release

# Run internal integration tests
cargo test
```

---

## Visual Style & Terminal Banner

```text
       _                      
  __ _| |___ _  _ _ _  __     
 / _` | (_-< || | ' \/ _|     
 \__, |_/__/\_, |_||_\__|  v0.1.0 (CLI)
 |___/      |__/              
 [Git-Aware Realtime Google Drive Sync]
```

### Color Palette & Design Language
- **Drive Primary**: `#4285F4` (Google Drive Blue)
- **Git Accent**: `#F1502F` (Official Git Orange)
- **Engine Active**: `#0F9D58` (Sync Success / Watcher Active)
- **Terminal Base**: `#14171C` (Dark slate / CLI backdrop)

> **Icon Metaphor**: The triangular flow echoes the iconic Google Drive loop, while the embedded branch nodes signify Git tree inspection and commit-level `.gitignore` awareness.

---

## Support

Please ensure you are running the latest version of `gdsync` before reporting issues. Bug reports, discussions, and feature requests are welcome at the [GitHub Issue Tracker](https://github.com/sadabx/gdsync/issues).

---

## License

Distributed under the **GNU General Public License v3.0**. See the [LICENSE](LICENSE) file for details.
