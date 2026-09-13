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
use gdsync_core::sync::SyncCoordinator;
use std::collections::HashMap;
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

    /// Compare two Google Drive folders to inspect unique, duplicate, and modified files
    Diff {
        /// Google Drive folder ID of first folder (e.g. from browser URL)
        folder1: String,

        /// Google Drive folder ID of second folder
        folder2: String,
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
        Commands::Diff { folder1, folder2 } => handle_diff(folder1, folder2).await?,
    }

    Ok(())
}

async fn handle_auth(
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

async fn handle_diff(folder1: String, folder2: String) -> Result<()> {
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

