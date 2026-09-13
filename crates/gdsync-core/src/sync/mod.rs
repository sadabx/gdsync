use crate::db::{Database, SyncRootRecord, TrackedFileRecord};
use crate::drive::DriveClient;
use crate::filter::{compute_file_md5, format_size, sanitize_filename_component, GitignoreFilter};
use crate::watcher::{FsChange, FsWatcher};
use anyhow::{Context, Result};
use indicatif::{ProgressBar, ProgressStyle};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::sync::{mpsc, Semaphore};
use tokio::task::JoinSet;
use tracing::{error, info, warn};

#[derive(Debug, Clone)]
pub struct SyncOptions {
    pub dry_run: bool,
    pub permanent_delete: bool,
    pub concurrency: usize,
    pub show_progress: bool,
}

impl Default for SyncOptions {
    fn default() -> Self {
        Self {
            dry_run: false,
            permanent_delete: false,
            concurrency: 4,
            show_progress: true,
        }
    }
}

pub struct SyncSummary {
    pub files_uploaded: usize,
    pub files_downloaded: usize,
    pub files_deleted: usize,
    pub files_unchanged: usize,
    pub files_failed: usize,
}

struct UploadItem {
    rel_path: PathBuf,
    abs_path: PathBuf,
    local_md5: String,
    modified_secs: i64,
    size_bytes: u64,
    existing_drive_id: Option<String>,
}

struct DownloadItem {
    child_id: String,
    child_rel_path: PathBuf,
    child_abs_path: PathBuf,
    remote_md5: String,
    size_bytes: u64,
}

pub struct SyncCoordinator {
    local_root: PathBuf,
    drive_root_id: String,
    drive: DriveClient,
    db: Database,
}

impl SyncCoordinator {
    pub fn new(
        local_root: &Path,
        drive_root_id: String,
        drive: DriveClient,
        db: Database,
    ) -> Result<Self> {
        let canonical = std::fs::canonicalize(local_root)
            .with_context(|| format!("Failed to canonicalize local path {:?}", local_root))?;

        // Ensure sync root is recorded in database
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;

        db.upsert_root(&SyncRootRecord {
            local_root: canonical.clone(),
            drive_root_id: drive_root_id.clone(),
            drive_root_name: None,
            created_at: now,
            last_sync_at: now,
        })?;

        Ok(Self {
            local_root: canonical,
            drive_root_id,
            drive,
            db,
        })
    }

    /// Runs a full push-and-pull reconciliation pass with default options.
    pub async fn reconcile(&self) -> Result<SyncSummary> {
        self.reconcile_with_options(&SyncOptions::default()).await
    }

    /// Runs a full push-and-pull reconciliation pass with custom options.
    pub async fn reconcile_with_options(&self, options: &SyncOptions) -> Result<SyncSummary> {
        if options.dry_run {
            println!("=== Dry-Run Reconciliation Plan for {:?} ===", self.local_root);
        } else {
            info!(
                "Starting sync reconciliation for {:?} -> Drive root {}",
                self.local_root, self.drive_root_id
            );
        }

        let push_stats = self.push_reconcile_with_options(options).await?;
        let pull_stats = self.pull_reconcile_with_options(options).await?;

        if !options.dry_run {
            let now = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_secs() as i64;
            self.db.update_last_sync(&self.local_root, now)?;
        }

        Ok(SyncSummary {
            files_uploaded: push_stats.files_uploaded,
            files_downloaded: pull_stats.files_downloaded,
            files_deleted: push_stats.files_deleted,
            files_unchanged: push_stats.files_unchanged + pull_stats.files_unchanged,
            files_failed: push_stats.files_failed + pull_stats.files_failed,
        })
    }

    /// Scans local files, respecting .gitignore, and pushes new/modified files to Drive with default options.
    pub async fn push_reconcile(&self) -> Result<SyncSummary> {
        self.push_reconcile_with_options(&SyncOptions::default()).await
    }

    /// Scans local files, respecting .gitignore, and pushes new/modified files to Drive.
    pub async fn push_reconcile_with_options(&self, options: &SyncOptions) -> Result<SyncSummary> {
        let filter = GitignoreFilter::new(&self.local_root)?;
        let local_files = filter.walk_unignored();

        let mut upload_items = Vec::new();
        let mut uploaded = 0;
        let mut unchanged = 0;
        let mut deleted = 0;
        let mut failed = 0;

        // 1. Inspect local files
        for file in &local_files {
            if file.is_directory {
                continue;
            }

            let rel_path = &file.relative_path;
            let abs_path = &file.absolute_path;

            let local_md5 = match compute_file_md5(abs_path) {
                Ok(hash) => hash,
                Err(err) => {
                    warn!("Failed to compute MD5 for {:?}: {}", abs_path, err);
                    failed += 1;
                    continue;
                }
            };

            let existing_db_record = self.db.get_file(&self.local_root, rel_path)?;

            if let Some(record) = existing_db_record {
                if record.md5_checksum == local_md5 {
                    unchanged += 1;
                    continue;
                }

                if options.dry_run {
                    println!("  [UPLOAD:MODIFIED] {:?} ({})", rel_path, format_size(file.size_bytes));
                    uploaded += 1;
                } else {
                    upload_items.push(UploadItem {
                        rel_path: rel_path.clone(),
                        abs_path: abs_path.clone(),
                        local_md5,
                        modified_secs: file.modified_secs,
                        size_bytes: file.size_bytes,
                        existing_drive_id: Some(record.drive_id),
                    });
                }
            } else {
                if options.dry_run {
                    println!("  [UPLOAD:NEW] {:?} ({})", rel_path, format_size(file.size_bytes));
                    uploaded += 1;
                } else {
                    upload_items.push(UploadItem {
                        rel_path: rel_path.clone(),
                        abs_path: abs_path.clone(),
                        local_md5,
                        modified_secs: file.modified_secs,
                        size_bytes: file.size_bytes,
                        existing_drive_id: None,
                    });
                }
            }
        }

        // 2. Detect locally deleted files
        let all_tracked = self.db.list_files_for_root(&self.local_root)?;
        for record in all_tracked {
            let local_abs = self.local_root.join(&record.rel_path);
            if !local_abs.exists() {
                if options.dry_run {
                    let action = if options.permanent_delete { "DELETE" } else { "TRASH" };
                    println!("  [{}] {:?}", action, record.rel_path);
                    deleted += 1;
                } else {
                    let action_res = if options.permanent_delete {
                        info!("Permanently deleting remote file for missing local file: {:?}", record.rel_path);
                        self.drive.delete_file(&record.drive_id).await
                    } else {
                        info!("Moving remote file to trash for missing local file: {:?}", record.rel_path);
                        self.drive.trash_file(&record.drive_id).await
                    };

                    if let Err(err) = action_res {
                        warn!("Failed to delete/trash remote Drive file {}: {}", record.drive_id, err);
                    }
                    self.db.delete_file(&self.local_root, &record.rel_path)?;
                    deleted += 1;
                }
            }
        }

        // 3. Perform uploads concurrently (if not dry-run)
        if !options.dry_run && !upload_items.is_empty() {
            let mut parent_map = std::collections::HashMap::new();
            parent_map.insert(PathBuf::new(), self.drive_root_id.clone());

            for item in &upload_items {
                let p = item.rel_path.parent().unwrap_or_else(|| Path::new("")).to_path_buf();
                if !parent_map.contains_key(&p) {
                    match self.drive.ensure_remote_dir_path(&self.drive_root_id, &p).await {
                        Ok(id) => { parent_map.insert(p, id); }
                        Err(err) => {
                            error!("Failed to create remote directory for {:?}: {:#}", p, err);
                        }
                    }
                }
            }

            let pb = if options.show_progress {
                let bar = ProgressBar::new(upload_items.len() as u64);
                bar.set_style(
                    ProgressStyle::default_bar()
                        .template("{spinner:.green} Uploading [{bar:30.cyan/blue}] {pos}/{len} ({eta}) {msg}")
                        .unwrap_or_else(|_| ProgressStyle::default_bar())
                        .progress_chars("#>-"),
                );
                Some(bar)
            } else {
                None
            };

            let concurrency = options.concurrency.max(1);
            let semaphore = Arc::new(Semaphore::new(concurrency));
            let mut join_set = JoinSet::new();

            for item in upload_items {
                let sem = Arc::clone(&semaphore);
                let drive = self.drive.clone();
                let db = self.db.clone();
                let local_root = self.local_root.clone();
                let parent_dir = item.rel_path.parent().unwrap_or_else(|| Path::new("")).to_path_buf();
                let target_parent_id = parent_map.get(&parent_dir).cloned().unwrap_or_else(|| self.drive_root_id.clone());
                let pb_clone = pb.clone();

                join_set.spawn(async move {
                    let _permit = sem.acquire().await.unwrap();
                    if let Some(ref bar) = pb_clone {
                        bar.set_message(format!("{}", item.rel_path.display()));
                    }

                    let file_name = item.rel_path.file_name().unwrap_or_default().to_string_lossy().to_string();

                    let res = if let Some(drive_id) = item.existing_drive_id {
                        drive.upload_resumable(&target_parent_id, &file_name, &item.abs_path, Some(&drive_id)).await
                    } else {
                        let existing = drive.find_child_by_name(&target_parent_id, &file_name).await.ok().flatten();
                        if let Some(remote) = existing {
                            drive.upload_resumable(&target_parent_id, &file_name, &item.abs_path, Some(&remote.id)).await
                        } else {
                            drive.upload_resumable(&target_parent_id, &file_name, &item.abs_path, None).await
                        }
                    };

                    if let Some(ref bar) = pb_clone {
                        bar.inc(1);
                    }

                    match res {
                        Ok(drive_file) => {
                            let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs() as i64;
                            let _ = db.upsert_file(&TrackedFileRecord {
                                local_root,
                                rel_path: item.rel_path,
                                drive_id: drive_file.id,
                                md5_checksum: drive_file.md5_checksum.unwrap_or(item.local_md5),
                                modified_secs: item.modified_secs,
                                size_bytes: item.size_bytes,
                                is_directory: false,
                                last_sync_timestamp: now,
                            });
                            Ok(())
                        }
                        Err(err) => Err((item.rel_path, err)),
                    }
                });
            }

            while let Some(res) = join_set.join_next().await {
                match res {
                    Ok(Ok(())) => uploaded += 1,
                    Ok(Err((path, err))) => {
                        error!("Failed to upload {:?}: {:#}", path, err);
                        failed += 1;
                    }
                    Err(err) => {
                        error!("Task join error: {}", err);
                        failed += 1;
                    }
                }
            }

            if let Some(bar) = pb {
                bar.finish_with_message("Uploads completed");
            }
        }

        Ok(SyncSummary {
            files_uploaded: uploaded,
            files_downloaded: 0,
            files_deleted: deleted,
            files_unchanged: unchanged,
            files_failed: failed,
        })
    }

    /// Pulls remote files from Google Drive if missing locally or remotely modified with default options.
    pub async fn pull_reconcile(&self) -> Result<SyncSummary> {
        self.pull_reconcile_with_options(&SyncOptions::default()).await
    }

    /// Pulls remote files from Google Drive if missing locally or remotely modified.
    pub async fn pull_reconcile_with_options(&self, options: &SyncOptions) -> Result<SyncSummary> {
        let mut download_items = Vec::new();
        let mut downloaded = 0;
        let mut unchanged = 0;
        let mut failed = 0;

        let filter = GitignoreFilter::new(&self.local_root)?;
        let mut queue: std::collections::VecDeque<(String, PathBuf)> = std::collections::VecDeque::new();
        queue.push_back((self.drive_root_id.clone(), PathBuf::new()));

        while let Some((remote_folder_id, relative_dir)) = queue.pop_front() {
            let children = match self.drive.list_children(&remote_folder_id).await {
                Ok(c) => c,
                Err(err) => {
                    error!("Failed to list children for remote folder {}: {:#}", remote_folder_id, err);
                    failed += 1;
                    continue;
                }
            };

            for child in children {
                let sanitized_name = sanitize_filename_component(&child.name);
                let child_rel_path = relative_dir.join(&sanitized_name);
                let child_abs_path = self.local_root.join(&child_rel_path);

                if filter.is_ignored(&child_abs_path, child.is_folder()) {
                    continue;
                }

                if child.is_folder() {
                    if !options.dry_run && !child_abs_path.exists() {
                        if let Err(e) = std::fs::create_dir_all(&child_abs_path) {
                            error!("Failed to create local directory {:?}: {}", child_abs_path, e);
                            failed += 1;
                            continue;
                        }
                    }
                    queue.push_back((child.id, child_rel_path));
                } else {
                    let remote_md5 = child.md5_checksum.clone().unwrap_or_default();
                    let existing_record = self.db.get_file(&self.local_root, &child_rel_path)?;

                    let needs_download = if !child_abs_path.exists() {
                        true
                    } else if let Some(rec) = existing_record {
                        rec.md5_checksum != remote_md5
                    } else {
                        let local_md5 = compute_file_md5(&child_abs_path).unwrap_or_default();
                        local_md5 != remote_md5
                    };

                    if needs_download {
                        let size_bytes = child.size_bytes();
                        if options.dry_run {
                            println!("  [DOWNLOAD] {:?} ({})", child_rel_path, format_size(size_bytes));
                            downloaded += 1;
                        } else {
                            download_items.push(DownloadItem {
                                child_id: child.id,
                                child_rel_path,
                                child_abs_path,
                                remote_md5,
                                size_bytes,
                            });
                        }
                    } else {
                        unchanged += 1;
                    }
                }
            }
        }

        // Perform downloads concurrently (if not dry-run)
        if !options.dry_run && !download_items.is_empty() {
            let pb = if options.show_progress {
                let bar = ProgressBar::new(download_items.len() as u64);
                bar.set_style(
                    ProgressStyle::default_bar()
                        .template("{spinner:.green} Downloading [{bar:30.cyan/blue}] {pos}/{len} ({eta}) {msg}")
                        .unwrap_or_else(|_| ProgressStyle::default_bar())
                        .progress_chars("#>-"),
                );
                Some(bar)
            } else {
                None
            };

            let concurrency = options.concurrency.max(1);
            let semaphore = Arc::new(Semaphore::new(concurrency));
            let mut join_set = JoinSet::new();

            for item in download_items {
                let sem = Arc::clone(&semaphore);
                let drive = self.drive.clone();
                let db = self.db.clone();
                let local_root = self.local_root.clone();
                let pb_clone = pb.clone();

                join_set.spawn(async move {
                    let _permit = sem.acquire().await.unwrap();
                    if let Some(ref bar) = pb_clone {
                        bar.set_message(format!("{}", item.child_rel_path.display()));
                    }

                    let download_res = drive.download_file(&item.child_id, &item.child_abs_path).await;

                    if let Some(ref bar) = pb_clone {
                        bar.inc(1);
                    }

                    match download_res {
                        Ok(()) => {
                            let mtime = std::fs::metadata(&item.child_abs_path)
                                .ok()
                                .and_then(|m| m.modified().ok())
                                .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                                .map(|d| d.as_secs() as i64)
                                .unwrap_or(0);

                            let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs() as i64;
                            let _ = db.upsert_file(&TrackedFileRecord {
                                local_root,
                                rel_path: item.child_rel_path.clone(),
                                drive_id: item.child_id,
                                md5_checksum: item.remote_md5,
                                modified_secs: mtime,
                                size_bytes: item.size_bytes,
                                is_directory: false,
                                last_sync_timestamp: now,
                            });
                            Ok(())
                        }
                        Err(err) => Err((item.child_rel_path, err)),
                    }
                });
            }

            while let Some(res) = join_set.join_next().await {
                match res {
                    Ok(Ok(())) => downloaded += 1,
                    Ok(Err((path, err))) => {
                        error!("Failed to download {:?}: {:#}", path, err);
                        failed += 1;
                    }
                    Err(err) => {
                        error!("Task join error: {}", err);
                        failed += 1;
                    }
                }
            }

            if let Some(bar) = pb {
                bar.finish_with_message("Downloads completed");
            }
        }

        Ok(SyncSummary {
            files_uploaded: 0,
            files_downloaded: downloaded,
            files_deleted: 0,
            files_unchanged: unchanged,
            files_failed: failed,
        })
    }

    /// Handles a single debounced filesystem change from the watcher.
    pub async fn handle_fs_change(&self, change: FsChange) -> Result<()> {
        match change {
            FsChange::CreateOrModify(abs_path) => {
                let rel_path = match abs_path.strip_prefix(&self.local_root) {
                    Ok(p) => p.to_path_buf(),
                    Err(_) => return Ok(()),
                };

                if abs_path.is_dir() {
                    self.drive
                        .ensure_remote_dir_path(&self.drive_root_id, &rel_path)
                        .await?;
                    return Ok(());
                }

                if !abs_path.exists() {
                    return Ok(());
                }

                let local_md5 = compute_file_md5(&abs_path)?;
                let existing_record = self.db.get_file(&self.local_root, &rel_path)?;

                if let Some(ref record) = existing_record {
                    if record.md5_checksum == local_md5 {
                        return Ok(());
                    }
                }

                let parent_dir = rel_path.parent().unwrap_or_else(|| Path::new(""));
                let target_parent_id = self
                    .drive
                    .ensure_remote_dir_path(&self.drive_root_id, parent_dir)
                    .await?;

                let file_name = rel_path.file_name().unwrap().to_string_lossy();
                let existing_id = existing_record.as_ref().map(|r| r.drive_id.as_str());

                info!("Watcher triggered upload for: {:?}", rel_path);
                let drive_file = self
                    .drive
                    .upload_resumable(&target_parent_id, &file_name, &abs_path, existing_id)
                    .await?;

                let metadata = std::fs::metadata(&abs_path)?;
                let mtime = metadata
                    .modified()
                    .ok()
                    .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                    .map(|d| d.as_secs() as i64)
                    .unwrap_or(0);

                let now = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap()
                    .as_secs() as i64;

                self.db.upsert_file(&TrackedFileRecord {
                    local_root: self.local_root.clone(),
                    rel_path,
                    drive_id: drive_file.id,
                    md5_checksum: drive_file.md5_checksum.unwrap_or(local_md5),
                    modified_secs: mtime,
                    size_bytes: metadata.len(),
                    is_directory: false,
                    last_sync_timestamp: now,
                })?;
            }
            FsChange::Delete(abs_path) => {
                let rel_path = match abs_path.strip_prefix(&self.local_root) {
                    Ok(p) => p.to_path_buf(),
                    Err(_) => return Ok(()),
                };

                if let Some(record) = self.db.get_file(&self.local_root, &rel_path)? {
                    info!("Watcher moving file to trash for: {:?}", rel_path);
                    if let Err(err) = self.drive.trash_file(&record.drive_id).await {
                        warn!("Failed to trash Drive file {}: {}", record.drive_id, err);
                    }
                    self.db.delete_file(&self.local_root, &rel_path)?;
                }
            }
        }

        Ok(())
    }

    /// Starts the continuous inotify watcher daemon.
    pub async fn start_daemon(&self, debounce_ms: u64) -> Result<()> {
        info!("Running initial reconciliation before starting watcher daemon...");
        let summary = self.reconcile().await?;
        info!(
            "Initial sync complete: {} uploaded, {} downloaded, {} deleted, {} unchanged",
            summary.files_uploaded,
            summary.files_downloaded,
            summary.files_deleted,
            summary.files_unchanged
        );

        let watcher = FsWatcher::new(&self.local_root, Duration::from_millis(debounce_ms))?;
        let (tx, mut rx) = mpsc::channel(100);

        let _debouncer = watcher.start_watching(tx)?;
        info!("Inotify daemon running. Monitoring for changes in {:?}...", self.local_root);

        while let Some(change) = rx.recv().await {
            if let Err(err) = self.handle_fs_change(change).await {
                error!("Error handling filesystem change: {:?}", err);
            }
        }

        Ok(())
    }
}
