use anyhow::Result;
use dialoguer::{theme::ColorfulTheme, Confirm, Input, Select};
use gdsync_core::config::{load_config, token_path};
use gdsync_core::db::Database;
use gdsync_core::filter::format_size;
use std::path::PathBuf;

const C_BLUE: &str = "\x1b[38;2;66;133;244m";
const C_ORANGE: &str = "\x1b[38;2;241;80;47m";
const C_GREEN: &str = "\x1b[38;2;15;157;88m";
const C_GRAY: &str = "\x1b[38;2;140;150;165m";
const C_DIM: &str = "\x1b[38;2;90;100;115m";
const C_BOLD: &str = "\x1b[1m";
const C_YELLOW: &str = "\x1b[38;2;251;188;4m";
const C_WHITE: &str = "\x1b[38;2;240;240;245m";
const C_RESET: &str = "\x1b[0m";

fn get_auth_status() -> String {
    if let Ok(path) = token_path() {
        if path.exists() {
            format!("{}● Connected{}", C_GREEN, C_RESET)
        } else {
            format!("{}○ Not Authenticated{}", C_ORANGE, C_RESET)
        }
    } else {
        format!("{}Unknown{}", C_GRAY, C_RESET)
    }
}

fn get_systemd_status() -> String {
    let output = std::process::Command::new("systemctl")
        .args(["--user", "is-active", "gdsync.service"])
        .output();
    match output {
        Ok(out) => {
            let status = String::from_utf8_lossy(&out.stdout).trim().to_string();
            match status.as_str() {
                "active" => format!("{}● Active (running){}", C_GREEN, C_RESET),
                "inactive" => format!("{}○ Inactive (stopped){}", C_GRAY, C_RESET),
                "failed" => format!("{}● Failed{}", C_ORANGE, C_RESET),
                _ => format!("{}Not installed{}", C_GRAY, C_RESET),
            }
        }
        Err(_) => format!("{}Not available{}", C_GRAY, C_RESET),
    }
}

fn get_db_summary() -> String {
    if let Ok(db) = Database::open_default() {
        if let Ok(stats) = db.get_db_stats() {
            if stats.total_files > 0 {
                return format!("{} files ({})", stats.total_files, format_size(stats.total_size_bytes));
            }
        }
    }
    "0 files (Empty)".to_string()
}

pub fn print_dashboard_header() {
    let auth_status = get_auth_status();
    let service_status = get_systemd_status();
    let db_status = get_db_summary();
    let config = load_config().unwrap_or_default();
    let dirs_info = match config.directories.len() {
        0 => "None configured".to_string(),
        1 => {
            let p = &config.directories[0].local_path;
            let display = p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| p.display().to_string());
            format!("1 configured ({})", display)
        }
        n => format!("{} configured", n),
    };

    println!();
    println!("  {C_BOLD}Welcome to gdsync v{}{C_RESET}", env!("CARGO_PKG_VERSION"));
    println!("  {C_DIM}Git-Aware Realtime Google Drive Sync Environment{C_RESET}");
    println!("  {C_DIM}─────────────────────────────────────────────────────────────────────────────{C_RESET}");
    println!("     {C_WHITE}.--.{C_RESET}                {C_BLUE}_{C_RESET}                        ");
    println!("    {C_WHITE}|{C_RESET}{C_YELLOW}o{C_RESET}{C_WHITE}_{C_RESET}{C_YELLOW}o{C_RESET}{C_WHITE} |{C_RESET}      {C_BLUE}__ _  __| |___  _   _ _ __   ___{C_RESET}     Google Drive:  {auth_status}");
    println!("    {C_WHITE}|{C_RESET}{C_YELLOW}:_/{C_RESET}{C_WHITE} |{C_RESET}     {C_BLUE}/ _` |/ _` (_-< | | | | '_ \\ / __|{C_RESET}    Directories:   {dirs_info}");
    println!("   {C_WHITE}//   \\ \\{C_RESET}    {C_ORANGE}\\__, |\\__,_/__/  \\__, |_| |_|\\___|{C_RESET}    Local State:   {db_status}");
    println!("  {C_WHITE}(|     |){C_RESET}   {C_ORANGE}|___/            |___/            {C_RESET}    Daemon:        {service_status}");
    println!("  {C_YELLOW}/'_   _/'\\{C_RESET}");
    println!("  {C_DIM}─────────────────────────────────────────────────────────────────────────────{C_RESET}");
    println!("  {C_BOLD}Let's get started.{C_RESET}\n");
}

pub async fn run_interactive_mode() -> Result<()> {
    let theme = ColorfulTheme::default();

    loop {
        print_dashboard_header();

        let menu_items = vec![
            "1. Run Two-Way Sync          Reconcile changes between local & Drive",
            "2. Start Watch Daemon        Monitor and sync in real time (inotify)",
            "3. Preview Scan (.gitignore) Dry-run inspect payload and ignored files",
            "4. Status & Statistics       View tracked directories and SQLite health",
            "5. Link New Directory        Map a local folder to Google Drive",
            "6. Compare Remote Folders    MD5 recursive diff between two Drive folders",
            "7. Consolidate Folders       Non-destructively merge remote folders",
            "8. Systemd Service Manager   Manage background user daemon",
            "9. Authenticate              Login with Google Drive via OAuth2 PKCE",
            "0. Exit",
        ];

        let selection = match Select::with_theme(&theme)
            .with_prompt("Select an action")
            .items(&menu_items)
            .default(0)
            .interact_opt()?
        {
            Some(idx) => idx,
            None => {
                println!("\nGoodbye!");
                break;
            }
        };

        match selection {
            0 => {
                // Two-Way Sync
                if let Some(target_dir) = select_or_prompt_directory("sync")? {
                    let dry_run = Confirm::with_theme(&theme)
                        .with_prompt("Run as dry-run preview first?")
                        .default(false)
                        .interact()?;

                    crate::handle_sync(target_dir, dry_run, 4, false).await?;
                }
            }
            1 => {
                // Watch Daemon
                if let Some(target_dir) = select_or_prompt_directory("watch")? {
                    let notify = Confirm::with_theme(&theme)
                        .with_prompt("Enable desktop notifications (notify-send)?")
                        .default(true)
                        .interact()?;

                    println!("\nStarting real-time watcher daemon. Press Ctrl+C to stop.\n");
                    crate::handle_watch(target_dir, None, 4, false, notify).await?;
                }
            }
            2 => {
                // Preview Scan
                if let Some(target_dir) = select_or_prompt_directory("scan")? {
                    crate::handle_scan(target_dir)?;
                }
            }
            3 => {
                // Status
                crate::handle_status(PathBuf::from("."))?;
            }
            4 => {
                // Link New Directory
                let local_path_str: String = Input::with_theme(&theme)
                    .with_prompt("Local directory path to sync")
                    .interact_text()?;
                let folder_name_or_id: String = Input::with_theme(&theme)
                    .with_prompt("Google Drive folder name or ID (leave blank to use directory name)")
                    .allow_empty(true)
                    .interact_text()?;

                let drive_folder = if folder_name_or_id.trim().is_empty() {
                    None
                } else {
                    Some(folder_name_or_id.trim().to_string())
                };

                crate::handle_init(PathBuf::from(local_path_str), drive_folder).await?;
            }
            5 => {
                // Diff Remote Folders
                let folder1: String = Input::with_theme(&theme)
                    .with_prompt("First Google Drive Folder ID")
                    .interact_text()?;
                let folder2: String = Input::with_theme(&theme)
                    .with_prompt("Second Google Drive Folder ID")
                    .interact_text()?;

                crate::handle_diff(folder1.trim().to_string(), folder2.trim().to_string()).await?;
            }
            6 => {
                // Merge Remote Folders
                let src: String = Input::with_theme(&theme)
                    .with_prompt("Source Google Drive Folder ID (copy from)")
                    .interact_text()?;
                let dest: String = Input::with_theme(&theme)
                    .with_prompt("Destination Google Drive Folder ID (copy to)")
                    .interact_text()?;

                crate::handle_merge(src.trim().to_string(), dest.trim().to_string(), false).await?;
            }
            7 => {
                // Systemd Service Manager
                run_service_submenu(&theme)?;
            }
            8 => {
                // Authenticate
                crate::handle_auth(None, None).await?;
            }
            9 => {
                // Exit
                println!("\nGoodbye!");
                break;
            }
            _ => unreachable!(),
        }

        println!();
        let _ = Input::<String>::with_theme(&theme)
            .with_prompt("Press Enter to return to main menu")
            .allow_empty(true)
            .interact_text();
        println!();
    }

    Ok(())
}

fn select_or_prompt_directory(action_name: &str) -> Result<Option<PathBuf>> {
    let theme = ColorfulTheme::default();
    let config = load_config().unwrap_or_default();
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));

    if config.directories.is_empty() {
        println!("No directories have been initialized yet.");
        let init_now = Confirm::with_theme(&theme)
            .with_prompt("Would you like to link a directory now?")
            .default(true)
            .interact()?;

        if init_now {
            let path_str: String = Input::with_theme(&theme)
                .with_prompt("Enter local directory path")
                .default(cwd.display().to_string())
                .interact_text()?;
            return Ok(Some(PathBuf::from(path_str)));
        } else {
            return Ok(None);
        }
    }

    let mut options: Vec<String> = Vec::new();

    for (i, d) in config.directories.iter().enumerate() {
        options.push(format!("{}. {} (Drive ID: {})", i + 1, d.local_path.display(), d.drive_folder_id));
    }

    // If cwd is not already in the list, offer it as an option
    let cwd_canonical = std::fs::canonicalize(&cwd).unwrap_or_else(|_| cwd.clone());
    let cwd_already_in_config = config.directories.iter().any(|d| {
        std::fs::canonicalize(&d.local_path).map(|p| p == cwd_canonical).unwrap_or(false)
    });

    if !cwd_already_in_config {
        options.push(format!("Current directory ({})", cwd.display()));
    }

    options.push("Enter custom path...".to_string());
    options.push("Cancel".to_string());

    let selection = Select::with_theme(&theme)
        .with_prompt(format!("Choose directory to {}", action_name))
        .items(&options)
        .default(0)
        .interact_opt()?;

    match selection {
        Some(idx) if idx < config.directories.len() => {
            Ok(Some(config.directories[idx].local_path.clone()))
        }
        Some(idx) if !cwd_already_in_config && idx == config.directories.len() => {
            Ok(Some(cwd))
        }
        Some(idx) => {
            let custom_idx = if !cwd_already_in_config { config.directories.len() + 1 } else { config.directories.len() };
            if idx == custom_idx {
                let custom: String = Input::with_theme(&theme)
                    .with_prompt("Enter local directory path")
                    .interact_text()?;
                Ok(Some(PathBuf::from(custom)))
            } else {
                Ok(None)
            }
        }
        None => Ok(None),
    }
}

fn run_service_submenu(theme: &ColorfulTheme) -> Result<()> {
    let service_options = vec![
        "1. Install & Start Systemd Service (on boot)",
        "2. Check Service Status",
        "3. View Live Logs (journalctl)",
        "4. Restart Service",
        "5. Stop Service",
        "0. Back to Main Menu",
    ];

    let choice = match Select::with_theme(theme)
        .with_prompt("Systemd Daemon Management")
        .items(&service_options)
        .default(0)
        .interact_opt()?
    {
        Some(idx) => idx,
        None => return Ok(()),
    };

    match choice {
        0 => crate::handle_service(crate::ServiceCommands::Install)?,
        1 => crate::handle_service(crate::ServiceCommands::Status)?,
        2 => crate::handle_service(crate::ServiceCommands::Logs)?,
        3 => crate::handle_service(crate::ServiceCommands::Restart)?,
        4 => crate::handle_service(crate::ServiceCommands::Stop)?,
        _ => return Ok(()),
    }

    Ok(())
}
