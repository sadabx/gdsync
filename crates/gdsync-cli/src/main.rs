use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use gdsync_core::auth::execute_oauth_login;
use gdsync_core::config::{
    add_or_update_directory, config_path, database_path, find_directory_config,
    load_config, save_config, token_path, WatchedDirectory,
};
use std::io::Write;
use gdsync_core::db::Database;
use gdsync_core::drive::DriveClient;
use gdsync_core::filter::GitignoreFilter;
use gdsync_core::sync::SyncCoordinator;
use std::path::{Path, PathBuf};
use tracing::Level;
use tracing_subscriber::FmtSubscriber;

#[derive(Parser)]
#[command(
    name = "gdsync",
    author = "gdsync team",
    version = "0.1.0",
    about = "High-performance Linux CLI daemon that syncs local directories with Google Drive respecting .gitignore rules",
    long_about = None
)]
struct Cli {
    #[command(subcommand)]
    command: Commands,

    /// Verbosity level (-v for debug, -vv for trace)
    #[arg(short, long, action = clap::ArgAction::Count, global = true)]
    verbose: u8,
}

#[derive(Subcommand)]
enum Commands {
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
    },

    /// Start the long-running inotify background daemon
    Watch {
        /// Target directory to watch (defaults to current directory)
        #[arg(default_value = ".")]
        path: PathBuf,

        /// Debounce buffer time in milliseconds (default: 500ms)
        #[arg(long)]
        debounce_ms: Option<u64>,
    },

    /// Inspect sync state, tracked files, and database statistics
    Status {
        /// Target directory to check (defaults to current directory)
        #[arg(default_value = ".")]
        path: PathBuf,
    },
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
        Commands::Auth {
            client_id,
            client_secret,
        } => handle_auth(client_id, client_secret).await?,
        Commands::Init { path, drive_folder } => handle_init(path, drive_folder).await?,
        Commands::Scan { path } => handle_scan(path)?,
        Commands::Sync { path } => handle_sync(path).await?,
        Commands::Watch { path, debounce_ms } => handle_watch(path, debounce_ms).await?,
        Commands::Status { path } => handle_status(path)?,
    }

    Ok(())
}

async fn handle_auth(
    client_id: Option<String>,
    client_secret: Option<String>,
) -> Result<()> {
    let mut cfg = load_config().unwrap_or_default();

    let cid = match client_id.or(cfg.client_id.clone()) {
        Some(id) if !id.trim().is_empty() => id.trim().to_string(),
        _ => {
            println!("=== Google Drive OAuth Setup ===");
            println!("Google Drive requires an OAuth 2.0 Client ID (Desktop app).");
            println!("If you don't have one yet:");
            println!("  1. Go to https://console.cloud.google.com/apis/credentials");
            println!("  2. Create an OAuth client ID with Application type: 'Desktop app'");
            println!("  3. Enable 'Google Drive API' under APIs & Services > Library");
            println!("  4. Add your email as a Test User under OAuth consent screen\n");

            print!("Enter your Google OAuth Client ID: ");
            std::io::stdout().flush()?;
            let mut input_id = String::new();
            std::io::stdin().read_line(&mut input_id)?;
            let trimmed = input_id.trim().to_string();
            if trimmed.is_empty() {
                bail!("OAuth authentication cancelled: Client ID cannot be empty.");
            }
            trimmed
        }
    };

    let csec = match client_secret.or(cfg.client_secret.clone()) {
        Some(s) if !s.trim().is_empty() => Some(s.trim().to_string()),
        _ => {
            print!("Enter your Google OAuth Client Secret (optional, press Enter to skip): ");
            std::io::stdout().flush()?;
            let mut input_sec = String::new();
            std::io::stdin().read_line(&mut input_sec)?;
            let trimmed = input_sec.trim().to_string();
            if trimmed.is_empty() {
                None
            } else {
                Some(trimmed)
            }
        }
    };

    // Persist credentials in config so user doesn't need to re-type them
    cfg.client_id = Some(cid.clone());
    cfg.client_secret = csec.clone();
    save_config(&cfg)?;

    execute_oauth_login(&cid, csec).await?;
    Ok(())
}

async fn handle_init(path: PathBuf, drive_folder: Option<String>) -> Result<()> {
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

fn handle_scan(path: PathBuf) -> Result<()> {
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

async fn handle_sync(path: PathBuf) -> Result<()> {
    let dir_cfg = resolve_directory_config(&path)?;

    println!("Starting reconciliation pass for {:?}", dir_cfg.local_path);
    println!("Remote Drive Root ID: {}", dir_cfg.drive_folder_id);

    let drive = DriveClient::from_auth().await?;
    let db = Database::open_default()?;
    let coordinator = SyncCoordinator::new(
        &dir_cfg.local_path,
        dir_cfg.drive_folder_id.clone(),
        drive,
        db,
    )?;

    let summary = coordinator.reconcile().await?;

    println!("\nReconciliation Completed Successfully!");
    println!("  Uploaded:   {} files", summary.files_uploaded);
    println!("  Downloaded: {} files", summary.files_downloaded);
    println!("  Deleted:    {} files", summary.files_deleted);
    println!("  Unchanged:  {} files", summary.files_unchanged);

    Ok(())
}

async fn handle_watch(path: PathBuf, debounce_override: Option<u64>) -> Result<()> {
    let dir_cfg = resolve_directory_config(&path)?;
    let debounce_ms = debounce_override.unwrap_or(dir_cfg.debounce_ms);

    println!("Starting gdsync daemon in watch mode...");
    println!("  Directory:    {:?}", dir_cfg.local_path);
    println!("  Drive Folder: {}", dir_cfg.drive_folder_id);
    println!("  Debounce:     {} ms", debounce_ms);

    let drive = DriveClient::from_auth().await?;
    let db = Database::open_default()?;
    let coordinator = SyncCoordinator::new(
        &dir_cfg.local_path,
        dir_cfg.drive_folder_id.clone(),
        drive,
        db,
    )?;

    coordinator.start_daemon(debounce_ms).await?;
    Ok(())
}

fn handle_status(path: PathBuf) -> Result<()> {
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
