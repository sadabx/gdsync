mod interactive;

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use gdsync_core::auth::execute_oauth_login;
use gdsync_core::config::{
    add_or_update_directory, config_path, database_path, find_directory_config,
    load_config, save_config, token_path, WatchedDirectory,
};
use gdsync_core::db::Database;
use gdsync_core::drive::{DriveClient, DriveFile};
use gdsync_core::filter::GitignoreFilter;
use gdsync_core::sync::{SyncCoordinator, SyncOptions};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use tracing::Level;
use tracing_subscriber::FmtSubscriber;

const BANNER: &str = r#"   .--.                _                        
  |o_o |      __ _  __| |___  _   _ _ __   ___  
  |:_/ |     / _` |/ _` (_-< | | | | '_ \ / __| 
 //   \ \    \__, |\__,_/__/  \__, |_| |_|\___|  v0.2.0 (CLI)
(|     |)    |___/            |___/             
/'_   _/'\   [Git-Aware Realtime Google Drive Sync]
"#;

#[derive(Parser)]
#[command(
    name = "gdsync",
    author = "Sadab Hafiz <sadabhfiz@gmail.com>",
    version = env!("CARGO_PKG_VERSION"),
    about = "High-performance Linux CLI daemon that syncs local directories with Google Drive respecting .gitignore rules",
    before_help = BANNER
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Commands>,

    /// Verbosity level (-v for debug, -vv for trace)
    #[arg(short, long, action = clap::ArgAction::Count, global = true)]
    verbose: u8,
}

#[derive(Subcommand)]
pub(crate) enum Commands {
    /// Authenticate with Google Drive via OAuth2 PKCE in browser
    Auth {
        /// Custom Google OAuth2 Client ID (overrides default/config)
        #[arg(long)]
        client_id: Option<String>,

        /// Custom Google OAuth2 Client Secret
        #[arg(long)]
        client_secret: Option<String>,
    },

    /// Link a local directory to a remote Google Drive folder
    Init {
        /// Local directory to sync
        path: PathBuf,

        /// Google Drive folder ID or name (creates new remote folder if not found)
        #[arg(short = 'd', long = "drive-folder")]
        drive_folder: Option<String>,
    },

    /// Traverse directory using .gitignore rules and print all files that will be synced
    Scan {
        /// Target directory to scan (defaults to current directory)
        #[arg(default_value = ".")]
        path: PathBuf,
    },

    /// Run a one-time two-way push/pull reconciliation pass
    Sync {
        /// Target directory to sync (defaults to current directory)
        #[arg(default_value = ".")]
        path: PathBuf,

        /// Preview files that will be uploaded/downloaded/trashed without modifying disk or Google Drive
        #[arg(long)]
        dry_run: bool,

        /// Number of concurrent transfer threads (default: 4)
        #[arg(long, default_value = "4")]
        concurrency: usize,

        /// Permanently delete remote files instead of moving to Google Drive Trash
        #[arg(long)]
        permanent_delete: bool,
    },

    /// Start the long-running inotify background daemon
    Watch {
        /// Target directory to watch (defaults to current directory)
        #[arg(default_value = ".")]
        path: PathBuf,

        /// Debounce buffer time in milliseconds (default: 500ms)
        #[arg(long)]
        debounce_ms: Option<u64>,

        /// Number of concurrent transfer threads (default: 4)
        #[arg(long, default_value = "4")]
        concurrency: usize,

        /// Permanently delete remote files instead of moving to Google Drive Trash
        #[arg(long)]
        permanent_delete: bool,

        /// Send desktop notifications on sync events (requires notify-send)
        #[arg(long)]
        notify: bool,
    },

    /// Inspect sync state, tracked files, and database statistics
    Status {
        /// Target directory to check (defaults to current directory)
        #[arg(default_value = ".")]
        path: PathBuf,
    },

    /// Compare two Google Drive folders to inspect unique, duplicate, and modified files
    Diff {
        /// Google Drive folder ID of first folder (e.g. from browser URL)
        folder1: String,

        /// Google Drive folder ID of second folder
        folder2: String,
    },

    /// Server-side merge unique files from source Google Drive folder into destination folder
    Merge {
        /// Source Google Drive folder ID (files will be copied from here)
        source: String,

        /// Destination Google Drive folder ID (files will be copied into here)
        destination: String,

        /// Automatically proceed without confirmation prompt
        #[arg(short = 'y', long = "yes")]
        yes: bool,
    },

    /// Generate shell auto-completions (bash, zsh, fish, powershell, elvish)
    Completions {
        /// Target shell to generate completions for
        #[arg(value_enum)]
        shell: clap_complete::Shell,
    },

    /// Manage gdsync background systemd user daemon
    Service {
        #[command(subcommand)]
        command: ServiceCommands,
    },
}

#[derive(Subcommand)]
pub(crate) enum ServiceCommands {
    /// Install, enable, and start gdsync as a systemd user service
    Install,
    /// Check status of gdsync systemd user service
    Status,
    /// Start gdsync systemd user service
    Start,
    /// Stop gdsync systemd user service
    Stop,
    /// Restart gdsync systemd user service
    Restart,
    /// View live logs from gdsync systemd service
    Logs,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    let log_level = match cli.verbose {
        0 => Level::INFO,
        1 => Level::DEBUG,
        _ => Level::TRACE,
    };

    let subscriber = FmtSubscriber::builder()
        .with_max_level(log_level)
        .with_target(false)
        .finish();
    let _ = tracing::subscriber::set_global_default(subscriber);

    match cli.command {
        None => interactive::run_interactive_mode().await?,
        Some(cmd) => match cmd {
            Commands::Auth {
                client_id,
                client_secret,
            } => handle_auth(client_id, client_secret).await?,
            Commands::Init { path, drive_folder } => handle_init(path, drive_folder).await?,
            Commands::Scan { path } => handle_scan(path)?,
            Commands::Sync {
                path,
                dry_run,
                concurrency,
                permanent_delete,
            } => handle_sync(path, dry_run, concurrency, permanent_delete).await?,
            Commands::Watch {
                path,
                debounce_ms,
                concurrency,
                permanent_delete,
                notify,
            } => handle_watch(path, debounce_ms, concurrency, permanent_delete, notify).await?,
            Commands::Status { path } => handle_status(path)?,
            Commands::Diff { folder1, folder2 } => handle_diff(folder1, folder2).await?,
            Commands::Merge {
                source,
                destination,
                yes,
            } => handle_merge(source, destination, yes).await?,
            Commands::Completions { shell } => handle_completions(shell)?,
            Commands::Service { command } => handle_service(command)?,
        },
    }

    Ok(())
}

pub(crate) async fn handle_auth(
    client_id: Option<String>,
    client_secret: Option<String>,
) -> Result<()> {
    let mut cfg = load_config().unwrap_or_default();

    let (cid, csec) = if let Some(id) = client_id {
        let sec = client_secret.or(cfg.client_secret.clone());
        cfg.client_id = Some(id.clone());
        cfg.client_secret = sec.clone();
        save_config(&cfg)?;
        (id, sec)
    } else if let Some(ref id) = cfg.client_id {
        (id.clone(), cfg.client_secret.clone())
    } else {
        (
            gdsync_core::auth::DEFAULT_CLIENT_ID.to_string(),
            Some(gdsync_core::auth::DEFAULT_CLIENT_SECRET.to_string()),
        )
    };

    println!("Starting Google Drive authentication...");
    if cid == gdsync_core::auth::DEFAULT_CLIENT_ID {
        println!("Using default verified client credentials.");
        println!("(Tip: You can pass --client-id / --client-secret to use your own Google Cloud project for dedicated quotas).\n");
    }

    execute_oauth_login(&cid, csec).await?;
    Ok(())
}

pub(crate) async fn handle_init(path: PathBuf, drive_folder: Option<String>) -> Result<()> {
    let canonical = std::fs::canonicalize(&path)
        .with_context(|| format!("Directory does not exist: {:?}", path))?;

    if !canonical.is_dir() {
        bail!("Path must be a directory: {:?}", canonical);
    }

    let folder_spec = drive_folder.unwrap_or_else(|| {
        canonical
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .to_string()
    });

    println!("Initializing gdsync for {:?}...", canonical);
    println!("Connecting to Google Drive...");

    let drive = DriveClient::from_auth().await?;
    let db = Database::open_default()?;

    // Check if folder_spec is an existing Drive folder ID or name
    let drive_folder_id = if let Ok(metadata) = drive.get_file_metadata(&folder_spec).await {
        if metadata.is_folder() {
            println!("Linked to existing Drive folder ID: {}", folder_spec);
            folder_spec.clone()
        } else {
            bail!("Remote ID {} is a file, not a folder", folder_spec);
        }
    } else {
        // Search in 'root' of user's Google Drive by name
        println!("Searching for folder '{}' in Google Drive root...", folder_spec);
        if let Some(existing) = drive.find_child_by_name("root", &folder_spec).await? {
            if existing.is_folder() {
                println!("Found existing remote folder '{}' (ID: {})", folder_spec, existing.id);
                existing.id
            } else {
                bail!("Remote item '{}' exists but is not a folder", folder_spec);
            }
        } else {
            println!("Creating new remote folder '{}' on Google Drive...", folder_spec);
            let created = drive.create_folder("root", &folder_spec).await?;
            println!("Created remote folder with ID: {}", created.id);
            created.id
        }
    };

    // Save to config.toml
    add_or_update_directory(&canonical, &drive_folder_id, Some(folder_spec.clone()))?;

    // Record in database
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;

    db.upsert_root(&gdsync_core::db::SyncRootRecord {
        local_root: canonical.clone(),
        drive_root_id: drive_folder_id.clone(),
        drive_root_name: Some(folder_spec),
        created_at: now,
        last_sync_at: now,
    })?;

    println!("\nSuccessfully initialized directory mapping!");
    println!("  Local path:  {:?}", canonical);
    println!("  Drive Root:  {}", drive_folder_id);
    println!("  Config:      {:?}", config_path()?);
    println!("  Database:    {:?}", database_path()?);
    println!("\nYou can now run `gdsync scan` or `gdsync sync`.");
    Ok(())
}

pub(crate) fn handle_scan(path: PathBuf) -> Result<()> {
    let canonical = std::fs::canonicalize(&path)
        .with_context(|| format!("Directory does not exist: {:?}", path))?;

    println!("Scanning directory respecting .gitignore rules: {:?}\n", canonical);

    let filter = GitignoreFilter::new(&canonical)?;
    let files = filter.walk_unignored();

    let mut total_size: u64 = 0;
    let mut file_count: usize = 0;
    let mut dir_count: usize = 0;

    println!("{:<6}  {:<12}  {}", "TYPE", "SIZE", "RELATIVE PATH");
    println!("{:-<6}  {:-<12}  {:-<40}", "", "", "");

    for file in &files {
        if file.is_directory {
            dir_count += 1;
            println!("{:<6}  {:<12}  {}/", "DIR", "-", file.relative_path.display());
        } else {
            file_count += 1;
            total_size += file.size_bytes;
            println!(
                "{:<6}  {:<12}  {}",
                "FILE",
                format_size(file.size_bytes),
                file.relative_path.display()
            );
        }
    }

    println!("\nScan Summary:");
    println!("  Unignored Files:       {}", file_count);
    println!("  Unignored Directories: {}", dir_count);
    println!("  Total Sync Payload:    {}", format_size(total_size));
    println!("  Rules applied:         .gitignore (root & nested) + mandatory safety excludes (.git, node_modules, target, .venv, etc.)");

    Ok(())
}

pub(crate) async fn handle_sync(
    path: PathBuf,
    dry_run: bool,
    concurrency: usize,
    permanent_delete: bool,
) -> Result<()> {
    let dir_cfg = resolve_directory_config(&path)?;

    println!("Starting reconciliation pass for {:?}", dir_cfg.local_path);
    println!("Remote Drive Root ID: {}", dir_cfg.drive_folder_id);
    if dry_run {
        println!("Mode:         DRY RUN (previewing actions only, no changes will be made)\n");
    } else {
        println!("Concurrency:  {} worker threads", concurrency);
        println!("Deletion:     {}\n", if permanent_delete { "Permanent Purge" } else { "Safe Cloud Trash (30-day recovery)" });
    }

    let drive = DriveClient::from_auth().await?;
    let db = Database::open_default()?;
    let coordinator = SyncCoordinator::new(
        &dir_cfg.local_path,
        dir_cfg.drive_folder_id.clone(),
        drive,
        db,
    )?;

    let options = SyncOptions {
        dry_run,
        permanent_delete,
        concurrency,
        show_progress: !dry_run,
    };

    let summary = coordinator.reconcile_with_options(&options).await?;

    if dry_run {
        println!("\nDry-Run Reconciliation Plan Summary:");
    } else {
        println!("\nReconciliation Completed!");
    }
    println!("  Uploaded:   {} files", summary.files_uploaded);
    println!("  Downloaded: {} files", summary.files_downloaded);
    println!("  Deleted:    {} files", summary.files_deleted);
    println!("  Unchanged:  {} files", summary.files_unchanged);
    if summary.files_failed > 0 {
        println!("  Errors:     {} files (see logs above)", summary.files_failed);
    }

    Ok(())
}

pub(crate) async fn handle_watch(
    path: PathBuf,
    debounce_override: Option<u64>,
    concurrency: usize,
    permanent_delete: bool,
    notify: bool,
) -> Result<()> {
    let dir_cfg = resolve_directory_config(&path)?;
    let debounce_ms = debounce_override.unwrap_or(dir_cfg.debounce_ms);

    println!("{}", BANNER);
    println!("Starting gdsync daemon in watch mode...");
    println!("  Directory:     {:?}", dir_cfg.local_path);
    println!("  Drive Folder:  {}", dir_cfg.drive_folder_id);
    println!("  Debounce:      {} ms", debounce_ms);
    println!("  Concurrency:   {} threads", concurrency);
    println!("  Notifications: {}", if notify { "Enabled" } else { "Disabled" });

    if notify {
        notify_user("gdsync", &format!("Monitoring {:?} in background", dir_cfg.local_path.file_name().unwrap_or_default().to_string_lossy()));
    }

    let drive = DriveClient::from_auth().await?;
    let db = Database::open_default()?;
    let coordinator = SyncCoordinator::new(
        &dir_cfg.local_path,
        dir_cfg.drive_folder_id.clone(),
        drive,
        db,
    )?;

    // Run initial reconciliation with options
    let options = SyncOptions {
        dry_run: false,
        permanent_delete,
        concurrency,
        show_progress: true,
    };
    let summary = coordinator.reconcile_with_options(&options).await?;
    if notify && (summary.files_uploaded > 0 || summary.files_downloaded > 0) {
        notify_user("gdsync: Initial Sync Complete", &format!("Synced {} files", summary.files_uploaded + summary.files_downloaded));
    }

    coordinator.start_daemon(debounce_ms).await?;
    Ok(())
}

pub(crate) fn handle_completions(shell: clap_complete::Shell) -> Result<()> {
    use clap::CommandFactory;
    let mut cmd = Cli::command();
    clap_complete::generate(shell, &mut cmd, "gdsync", &mut std::io::stdout());
    Ok(())
}

pub(crate) fn handle_service(cmd: ServiceCommands) -> Result<()> {
    match cmd {
        ServiceCommands::Install => {
            let exe_path = std::env::current_exe()
                .context("Failed to determine current executable path")?;
            let service_dir = dirs::config_dir()
                .context("Could not find user config dir")?
                .join("systemd/user");
            std::fs::create_dir_all(&service_dir)?;

            let service_file = service_dir.join("gdsync.service");
            let service_content = format!(
                r#"[Unit]
Description=gdsync background Google Drive sync daemon
After=network-online.target

[Service]
Type=simple
ExecStart={} watch
Restart=on-failure
RestartSec=5

[Install]
WantedBy=default.target
"#,
                exe_path.display()
            );

            std::fs::write(&service_file, service_content)?;
            println!("Wrote systemd unit file to {:?}", service_file);

            println!("Reloading systemd user daemon...");
            let _ = std::process::Command::new("systemctl")
                .args(["--user", "daemon-reload"])
                .status();

            println!("Enabling and starting gdsync.service...");
            let status = std::process::Command::new("systemctl")
                .args(["--user", "enable", "--now", "gdsync.service"])
                .status();

            if let Ok(s) = status {
                if s.success() {
                    println!("\ngdsync.service has been successfully installed and started!");
                    println!("Check status with: gdsync service status");
                    println!("View live logs with: gdsync service logs");
                } else {
                    println!("\nWarning: systemctl command exited with code {:?}", s.code());
                }
            }
        }
        ServiceCommands::Status => {
            let _ = std::process::Command::new("systemctl")
                .args(["--user", "status", "gdsync.service"])
                .status();
        }
        ServiceCommands::Start => {
            let _ = std::process::Command::new("systemctl")
                .args(["--user", "start", "gdsync.service"])
                .status();
            println!("Started gdsync.service");
        }
        ServiceCommands::Stop => {
            let _ = std::process::Command::new("systemctl")
                .args(["--user", "stop", "gdsync.service"])
                .status();
            println!("Stopped gdsync.service");
        }
        ServiceCommands::Restart => {
            let _ = std::process::Command::new("systemctl")
                .args(["--user", "restart", "gdsync.service"])
                .status();
            println!("Restarted gdsync.service");
        }
        ServiceCommands::Logs => {
            let _ = std::process::Command::new("journalctl")
                .args(["--user", "-u", "gdsync.service", "-n", "50", "-f"])
                .status();
        }
    }
    Ok(())
}

fn notify_user(title: &str, message: &str) {
    let _ = std::process::Command::new("notify-send")
        .arg("-a")
        .arg("gdsync")
        .arg("-i")
        .arg("emblem-synchronized")
        .arg(title)
        .arg(message)
        .spawn();
}

pub(crate) fn handle_status(path: PathBuf) -> Result<()> {
    let canonical = std::fs::canonicalize(&path).unwrap_or_else(|_| path.clone());
    let db = Database::open_default()?;
    let stats = db.get_db_stats()?;
    let config = load_config()?;

    println!("=== gdsync Status ===");
    println!("Config Path:   {:?}", config_path()?);
    println!("Database Path: {:?}", database_path()?);
    println!("Token Path:    {:?} (exists: {})", token_path()?, token_path()?.exists());
    println!("\n--- Database Stats ---");
    println!("Total Tracked Roots: {}", stats.total_roots);
    println!("Total Tracked Files: {}", stats.total_files);
    println!("Total Tracked Size:  {}", format_size(stats.total_size_bytes));

    println!("\n--- Watched Directories ---");
    if config.directories.is_empty() {
        println!("  (No directories initialized yet. Run `gdsync init <path>`)");
    } else {
        for dir in &config.directories {
            let marker = if dir.local_path == canonical { "* " } else { "  " };
            println!(
                "{}{:?} -> Drive ID: {} (debounce: {}ms)",
                marker, dir.local_path, dir.drive_folder_id, dir.debounce_ms
            );

            if dir.local_path == canonical {
                let tracked = db.list_files_for_root(&dir.local_path)?;
                println!("    Tracked files in this root: {}", tracked.len());
                if let Some(root_rec) = db.get_root(&dir.local_path)? {
                    let sync_time = chrono::DateTime::from_timestamp(root_rec.last_sync_at, 0)
                        .map(|t| t.to_rfc2822())
                        .unwrap_or_else(|| "Never".to_string());
                    println!("    Last sync timestamp:        {}", sync_time);
                }
            }
        }
    }

    Ok(())
}

fn resolve_directory_config(path: &Path) -> Result<WatchedDirectory> {
    let canonical = std::fs::canonicalize(path)
        .with_context(|| format!("Directory does not exist: {:?}", path))?;

    if let Some(cfg) = find_directory_config(&canonical)? {
        return Ok(cfg);
    }

    // If not found in config, check if there is exactly 1 configured directory
    let config = load_config()?;
    if config.directories.len() == 1 {
        return Ok(config.directories[0].clone());
    }

    bail!(
        "Directory {:?} is not initialized. Run `gdsync init {:?}` first.",
        canonical,
        canonical
    );
}

fn format_size(bytes: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = KB * 1024;
    const GB: u64 = MB * 1024;

    if bytes >= GB {
        format!("{:.2} GB", bytes as f64 / GB as f64)
    } else if bytes >= MB {
        format!("{:.2} MB", bytes as f64 / MB as f64)
    } else if bytes >= KB {
        format!("{:.2} KB", bytes as f64 / KB as f64)
    } else {
        format!("{} B", bytes)
    }
}

pub(crate) async fn handle_diff(folder1: String, folder2: String) -> Result<()> {
    println!("Connecting to Google Drive...");
    let drive = DriveClient::from_auth().await?;

    let meta1 = drive.get_file_metadata(&folder1).await
        .with_context(|| format!("Failed to access Folder 1 (ID: {})", folder1))?;
    let meta2 = drive.get_file_metadata(&folder2).await
        .with_context(|| format!("Failed to access Folder 2 (ID: {})", folder2))?;

    println!("Scanning Folder 1: '{}' (ID: {})...", meta1.name, folder1);
    let files1 = drive.list_files_recursive(&folder1).await?;
    println!("  Found {} files in Folder 1.", files1.len());

    println!("Scanning Folder 2: '{}' (ID: {})...", meta2.name, folder2);
    let files2 = drive.list_files_recursive(&folder2).await?;
    println!("  Found {} files in Folder 2.\n", files2.len());

    let map1: HashMap<PathBuf, &DriveFile> = files1.iter().map(|(p, f)| (p.clone(), f)).collect();
    let map2: HashMap<PathBuf, &DriveFile> = files2.iter().map(|(p, f)| (p.clone(), f)).collect();

    let mut identical = Vec::new();
    let mut modified = Vec::new();
    let mut only_in_1 = Vec::new();
    let mut only_in_2 = Vec::new();

    for (path, f1) in &map1 {
        if let Some(f2) = map2.get(path) {
            let md5_1 = f1.md5_checksum.as_deref().unwrap_or("");
            let md5_2 = f2.md5_checksum.as_deref().unwrap_or("");
            if !md5_1.is_empty() && md5_1 == md5_2 {
                identical.push((path, f1.size_bytes()));
            } else {
                modified.push((path, f1, f2));
            }
        } else {
            only_in_1.push((path, f1));
        }
    }

    for (path, f2) in &map2 {
        if !map1.contains_key(path) {
            only_in_2.push((path, f2));
        }
    }

    println!("============================================================");
    println!("                  COMPARISON SUMMARY                        ");
    println!("============================================================");
    println!("  Folder 1: '{}' (Total: {} files)", meta1.name, files1.len());
    println!("  Folder 2: '{}' (Total: {} files)", meta2.name, files2.len());
    println!("------------------------------------------------------------");
    println!("  Identical files (same MD5):       {}", identical.len());
    println!("  Unique to Folder 1 (not in 2):    {}", only_in_1.len());
    println!("  Unique to Folder 2 (not in 1):    {}", only_in_2.len());
    println!("  Same name, different content:     {}", modified.len());
    println!("============================================================\n");

    if !only_in_1.is_empty() {
        println!("--- Files UNIQUE to Folder 1 ({}) ---", folder1);
        for (path, file) in only_in_1.iter().take(25) {
            println!("  + {} ({})", path.display(), format_size(file.size_bytes()));
        }
        if only_in_1.len() > 25 {
            println!("  ... and {} more files", only_in_1.len() - 25);
        }
        println!();
    }

    if !only_in_2.is_empty() {
        println!("--- Files UNIQUE to Folder 2 ({}) ---", folder2);
        for (path, file) in only_in_2.iter().take(25) {
            println!("  + {} ({})", path.display(), format_size(file.size_bytes()));
        }
        if only_in_2.len() > 25 {
            println!("  ... and {} more files", only_in_2.len() - 25);
        }
        println!();
    }

    if !modified.is_empty() {
        println!("--- Files with SAME NAME but DIFFERENT CONTENT ---");
        for (path, f1, f2) in modified.iter().take(25) {
            println!(
                "  * {} (F1: {}, F2: {})",
                path.display(),
                format_size(f1.size_bytes()),
                format_size(f2.size_bytes())
            );
        }
        if modified.len() > 25 {
            println!("  ... and {} more files", modified.len() - 25);
        }
        println!();
    }

    if only_in_1.is_empty() && only_in_2.is_empty() && modified.is_empty() {
        println!("Conclusion: Both folders are 100% IDENTICAL!");
    } else if only_in_1.is_empty() && modified.is_empty() {
        println!("Conclusion: Folder 1 is a complete SUBSET of Folder 2. Folder 2 contains all files from Folder 1 plus {} extra files.", only_in_2.len());
    } else if only_in_2.is_empty() && modified.is_empty() {
        println!("Conclusion: Folder 2 is a complete SUBSET of Folder 1. Folder 1 contains all files from Folder 2 plus {} extra files.", only_in_1.len());
    } else {
        println!("Conclusion: Both folders have unique files or modifications that differ.");
    }

    Ok(())
}

pub(crate) async fn handle_merge(source: String, destination: String, yes: bool) -> Result<()> {
    println!("Connecting to Google Drive...");
    let drive = DriveClient::from_auth().await?;

    let meta_src = drive
        .get_file_metadata(&source)
        .await
        .with_context(|| format!("Failed to access source folder (ID: {})", source))?;
    let meta_dest = drive
        .get_file_metadata(&destination)
        .await
        .with_context(|| format!("Failed to access destination folder (ID: {})", destination))?;

    println!("Analyzing folders on Google Drive...");
    println!("  Source:      '{}' (ID: {})", meta_src.name, source);
    println!("  Destination: '{}' (ID: {})", meta_dest.name, destination);

    let src_files = drive.list_files_recursive(&source).await?;
    let dest_files = drive.list_files_recursive(&destination).await?;

    let dest_map: HashMap<PathBuf, &DriveFile> =
        dest_files.iter().map(|(p, f)| (p.clone(), f)).collect();

    let mut files_to_merge = Vec::new();
    for (path, src_file) in &src_files {
        if !dest_map.contains_key(path) {
            files_to_merge.push((path, src_file));
        }
    }

    if files_to_merge.is_empty() {
        println!(
            "\nNothing to merge! All {} files from '{}' already exist in '{}'.",
            src_files.len(),
            meta_src.name,
            meta_dest.name
        );
        return Ok(());
    }

    println!(
        "\nFound {} unique files to merge from '{}' into '{}'.",
        files_to_merge.len(),
        meta_src.name,
        meta_dest.name
    );

    if !yes {
        print!(
            "Proceed with server-side copy into '{}'? [y/N]: ",
            meta_dest.name
        );
        use std::io::Write;
        std::io::stdout().flush()?;
        let mut confirm = String::new();
        std::io::stdin().read_line(&mut confirm)?;
        if !confirm.trim().eq_ignore_ascii_case("y") {
            println!("Merge cancelled.");
            return Ok(());
        }
    }

    println!("\nStarting server-side copy on Google Drive (instant, 0 upload bandwidth)...");
    let total = files_to_merge.len();
    let mut success_count = 0;

    for (idx, (rel_path, src_file)) in files_to_merge.iter().enumerate() {
        let parent_dir = rel_path.parent().unwrap_or_else(|| Path::new(""));
        let target_parent_id = drive
            .ensure_remote_dir_path(&destination, parent_dir)
            .await?;

        match drive
            .copy_file(&src_file.id, &target_parent_id, &src_file.name)
            .await
        {
            Ok(_) => {
                success_count += 1;
                println!("[{}/{}] Copied: {}", idx + 1, total, rel_path.display());
            }
            Err(err) => {
                eprintln!(
                    "[{}/{}] FAILED {}: {}",
                    idx + 1,
                    total,
                    rel_path.display(),
                    err
                );
            }
        }
    }

    println!("\n============================================================");
    println!("                     MERGE COMPLETED                        ");
    println!("============================================================");
    println!(
        "  Successfully copied {} of {} files into '{}' (ID: {})",
        success_count, total, meta_dest.name, destination
    );
    println!("  Destination now has all files consolidated!");
    println!(
        "  You can now safely delete the redundant folder '{}' in Google Drive.",
        meta_src.name
    );
    println!("============================================================");

    Ok(())
}

