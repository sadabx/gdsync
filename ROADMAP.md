# gdsync Development Roadmap & Feature Parity Plan

This roadmap tracks missing features from **Google Drive for Desktop (Windows/macOS)** to bring complete, best-in-class Google Drive functionality to Linux while preserving `gdsync`'s core principles: **lightning speed, native Rust architecture, developer-first `.gitignore` awareness, and zero telemetry bloat.**

---

## Roadmap Overview

```
Phase 1: Daemon Controls & Usability
├── ⏸ Real-Time Pause & Resume (Unix Domain Socket IPC)
├── ⚡ Bandwidth Throttling / Rate Limiting (Token-Bucket)
└── 👤 Multi-Account Profiles (`--profile work|personal`)

Phase 2: Google Workspace & Cloud Scope
├── 🏢 Google Workspace Shared Drives (Team Drives) Support
├── 🤝 "Shared with Me" Folder Ingestion & Syncing
└── 📁 Selective Subdirectory Exclusion Tree

Phase 3: Linux Desktop & File Manager Integration
├── 🏷 File Manager Badge Overlays (Nautilus, Nemo, Dolphin)
├── 📋 Right-Click Context Menu ("Copy Drive Link", "Sync Now")
└── 🖥 Optional Lightweight Desktop Tray Applet

Phase 4: Advanced Filesystem Architecture
└── 💽 Virtual Filesystem / Streaming Mode (`gdsync mount` via pure Rust `fuse3`)
```

---

## Phase 1: Daemon Controls & Usability

### 1. Real-Time Pause & Resume (`gdsync pause` / `gdsync resume`)
* **Goal**: Allow users to temporarily pause sync activity without killing the background daemon or stopping the systemd service.
* **Architecture**:
  * Implement an IPC control listener using a **Unix Domain Socket** located at `~/.config/gdsync/daemon.sock`.
  * The watcher daemon runs with an `Arc<AtomicBool>` pause state.
  * When paused:
    - Kernel inotify filesystem events continue buffering in an in-memory queue.
    - Active network uploads and reconciliation passes are frozen.
  * When resumed:
    - The pending event queue is drained, and a rapid delta-check is performed.
* **CLI Surface**:
  * `gdsync pause` - Pauses active syncing.
  * `gdsync resume` - Resumes syncing and flushes pending changes.
  * `gdsync status` - Displays `● Active (Syncing)` vs `⏸ Paused (Queue: N files)`.

---

### 2. Bandwidth Throttling / Rate Limiter
* **Goal**: Prevent background synchronization from saturating home Wi-Fi or metered connections.
* **Architecture**:
  * Implement a **Token Bucket** rate limiter in `DriveClient` for upload and download byte streams.
  * Wrap chunked upload and download byte bodies with a metered stream adapter that clamps transfer rates to configured byte-per-second thresholds.
* **CLI Surface**:
  * `gdsync sync --max-upload-speed 5M --max-download-speed 10M`
  * `gdsync watch --max-upload-speed 2M`
  * Configurable globally in `~/.config/gdsync/config.toml` under `[network]`.

---

### 3. Multi-Account Support (Profiles)
* **Goal**: Enable simultaneous or switchable syncing for multiple Google accounts (e.g., personal `@gmail.com` and enterprise Google Workspace `@company.com`).
* **Architecture**:
  * Introduce profile namespaces:
    - Tokens: `~/.config/gdsync/profiles/<profile_name>/token.json`
    - Databases: `~/.config/gdsync/profiles/<profile_name>/state.db`
    - Configurations: `~/.config/gdsync/profiles/<profile_name>/config.toml`
  * Default profile remains `default` for backward compatibility.
* **CLI Surface**:
  * `gdsync auth --profile work`
  * `gdsync init ~/Work -d "WorkDrive" --profile work`
  * `gdsync watch --profile work`

---

## Phase 2: Google Workspace & Cloud Scope

### 4. Shared Drives (Team Drives) & "Shared with Me"
* **Goal**: Allow users to link and sync folders from corporate Google Workspace Shared Drives and shared documents.
* **Architecture**:
  * Enable `supportsAllDrives=true` and `includeItemsFromAllDrives=true` across all Google Drive v3 REST API endpoints.
  * Add Drive listing method `list_shared_drives()` to discover and map available Shared Drives.
* **CLI Surface**:
  * `gdsync list-drives` - Lists personal Drive and all accessible Shared Drives.
  * `gdsync init <local_path> -d <folder_id> --shared-drive <drive_id>`

---

### 5. Selective Subtree Sync
* **Goal**: Choose specific remote subfolders to omit from local synchronization when full directory cloning is undesirable.
* **Architecture**:
  * Add a `[selective_sync]` section in directory config mapping paths to sync or skip.
  * Filter during `pull_reconcile` before recursive traversal.
* **CLI Surface**:
  * `gdsync init <path> --exclude "Archive/*" --exclude "LargeMedia/*"`

---

## Phase 3: Linux Desktop Integration

### 6. File Manager Shell Integration (Nautilus, Nemo, Dolphin)
* **Goal**: Provide visual sync status overlays and context menus in native Linux file managers.
* **Architecture**:
  * Develop lightweight extensions for GNOME Files (`nautilus-python`), Nemo, and KDE Dolphin.
  * Badges:
    - Synchronized (clean checkmark).
    - Syncing (progress indicator).
    - Ignored (dimmed icon for `.gitignore` exclusions).
    - Conflict (warning emblem for unresolved conflict copies).
  * Context Menu Actions:
    - "Copy Google Drive Link"
    - "Force Re-sync Now"

---

### 7. Optional System Tray Applet
* **Goal**: Minimalist desktop status icon in the GNOME / KDE / Waybar system tray.
* **Architecture**:
  * Standalone lightweight client communicating with the `gdsync` daemon socket.
  * Shows current transfer progress, quick pause/resume toggle, and recent sync activity.

---

## Phase 4: Virtual Filesystem / Streaming Mode (`gdsync mount`)

### 8. Virtual Filesystem / Files On-Demand via Pure Rust FUSE
* **Goal**: Allow users to mount their entire Google Drive library as a virtual filesystem (e.g. `/mnt/gdrive`), streaming files on-demand without consuming local SSD storage.
* **Architecture Specification**:
  * **Pure Rust & Zero Garbage Collection**:
    - Use the async [`fuse3`](https://crates.io/crates/fuse3) crate running directly on Tokio.
    - Zero Go dependency, zero GC stutter during 4K media playback.
  * **Chunked Read-Ahead Streaming**:
    - When an application executes `read(offset, size)`, issue HTTP `Range: bytes=X-Y` queries directly to Google Drive.
    - Read-ahead window: dynamically buffer the next 16 MiB chunk for sequential reads.
  * **Sparse Local LRU Disk Cache**:
    - Store recently read blocks in `~/.cache/gdsync/vfs/<file_id>/`.
    - Automatically evict least-recently-used chunks when disk cache exceeds a user-configured limit (e.g., 10 GB).
  * **Write-Back Journal**:
    - Support staging modified blocks to local disk with an asynchronous write-back worker that handles resumable uploads upon file `flush()` or `release()`.
* **CLI Surface**:
  * `gdsync mount <remote_folder_or_id> <mountpoint> [options]`
    - `--vfs-cache-mode full|writes|off`
    - `--vfs-cache-max-size 10G`
    - `--vfs-read-chunk-size 16M`
    - `--allow-other`
  * `gdsync unmount <mountpoint>`

---

## Principles Maintained Throughout Roadmap

1. **Pure Native Rust**: Zero polyglot runtime overhead, single self-contained binary distribution.
2. **Developer-First**: Native `.gitignore` and `.gdsyncignore` rules remain first-class citizens across all features.
3. **Zero Data Loss**: Conflict preservation, deletion guards, and atomic SQLite state transactions are strictly enforced.
4. **Clean Unix Philosophy**: Modular commands, scriptable JSON output options, and headless server compatibility.
