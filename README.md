# gdsync

A high-performance Linux CLI daemon for Arch Linux that two-way syncs local directories with Google Drive while natively respecting root and nested `.gitignore` rules.

Built with a modular Cargo workspace:
- **`crates/gdsync-core`**: Headless sync engine, SQLite state cache, Linux inotify file watcher, and Google Drive v3 REST API client.
- **`crates/gdsync-cli`**: Terminal CLI interface (`gdsync`).

---

## Features

- **Strict Gitignore Engine**: Uses the `ignore` crate (`WalkBuilder` / `GitignoreBuilder`) to honor root and nested `.gitignore` rules. Hardcoded safeguards ensure `.git/`, `node_modules/`, `.venv/`, `target/`, and build artifacts are never uploaded.
- **Embedded SQLite State Engine**: Fast, local state database at `~/.config/gdsync/state.db` using `rusqlite` in WAL mode. Tracks relative file paths, remote Google Drive file IDs, MD5 checksums, and sync timestamps.
- **Kernel inotify Watcher**: Real-time Linux filesystem monitoring with debouncing via `notify-debouncer-full` to buffer rapid editor saves and write bursts before syncing.
- **Google Drive v3 REST API**:
  - OAuth2 PKCE login with local loopback callback receiver on port 8085 and browser launch (`xdg-open`).
  - Automatic token refresh stored in `~/.config/gdsync/token.json`.
  - 5 MiB chunked resumable uploads with HTTP 308 resume handling.
  - Automatic remote directory hierarchy creation and synchronization.
- **Two-Way Sync & Conflict Reconciliation**:
  - Full push/pull reconciliation passes.
  - Continuous daemon background watch mode.

---

## Installation & Build

Requires Rust toolchain (Rust 1.75+):

```bash
# Build debug binary
cargo build --bin gdsync

# Build optimized release binary
cargo build --release --bin gdsync

# Optional: Install to user PATH
cargo install --path crates/gdsync-cli
```

---

## CLI Usage

### 1. Authenticate with Google Drive
```bash
gdsync auth
```
Opens your browser for OAuth2 PKCE authorization. Tokens are saved to `~/.config/gdsync/token.json`.

You can also provide custom Google Cloud project OAuth credentials:
```bash
gdsync auth --client-id <CLIENT_ID> --client-secret <CLIENT_SECRET>
```

### 2. Initialize Directory Sync
```bash
gdsync init /path/to/my-project --drive-folder "my-project"
```
Links the local directory with a Google Drive folder (creates the remote folder if it doesn't exist). Stores mapping in `~/.config/gdsync/config.toml` and `state.db`.

### 3. Dry-Run Gitignore Scan
```bash
gdsync scan
```
Traverses the directory honoring `.gitignore` rules and safety blacklists, displaying all files that would be synced, their sizes, and payload summary.

### 4. Run One-Time Reconciliation
```bash
gdsync sync
```
Performs a two-way push/pull sync pass between local files and Google Drive.

### 5. Start Background Inotify Daemon
```bash
gdsync watch
```
Runs an initial reconciliation and starts watching the filesystem with debounced inotify events, automatically syncing changes to Google Drive in real-time.

### 6. Inspect Status & Database Health
```bash
gdsync status
```
Displays tracked directories, SQLite statistics, total tracked files, and payload size.

---

## Architecture & Storage

All configuration, token caches, and databases are stored in `~/.config/gdsync/`:
- `config.toml`: Watched directories, remote folder IDs, and debouncer parameters.
- `state.db`: SQLite database mapping local relative paths to remote Drive IDs and MD5 hashes.
- `token.json`: Stored OAuth2 tokens with expiration and refresh credentials.

For full technical specifications, see [ARCHITECTURE.md](ARCHITECTURE.md).

---

## License

MIT OR Apache-2.0
