use crate::db::{Database, SyncRootRecord, TrackedFileRecord};
use crate::drive::DriveClient;
use crate::filter::{
    compute_file_md5_async, format_size, sanitize_filename_component, GitignoreFilter,
};
use crate::watcher::{FsChange, FsWatcher};
use anyhow::{bail, Context, Result};
use indicatif::{ProgressBar, ProgressStyle};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::sync::{mpsc, RwLock, Semaphore};
use tokio::task::JoinSet;
use tracing::{error, info, warn};

#[derive(Debug, Clone)]
pub struct SyncOptions {
    pub dry_run: bool,
    pub permanent_delete: bool,
    pub concurrency: usize,
    pub show_progress: bool,
    pub force: bool,
}

impl Default for SyncOptions {
    fn default() -> Self {
        Self {
            dry_run: false,
            permanent_delete: false,
            concurrency: 4,
            show_progress: true,
            force: false,
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
    export_mime: Option<String>,
}

pub struct SyncCoordinator {
    local_root: PathBuf,
    drive_root_id: String,
    drive: DriveClient,
    db: Database,
    dir_cache: Arc<RwLock<HashMap<PathBuf, String>>>,
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

        let mut initial_cache = HashMap::new();
        initial_cache.insert(PathBuf::new(), drive_root_id.clone());

        Ok(Self {
            local_root: canonical,
            drive_root_id,
            drive,
            db,
            dir_cache: Arc::new(RwLock::new(initial_cache)),
        })
    }

    /// Resolves and caches a remote directory ID for a relative path.
    pub async fn ensure_remote_dir_cached(&self, rel_dir: &Path) -> Result<String> {
        let rel_buf = rel_dir.to_path_buf();
        {
            let cache = self.dir_cache.read().await;
            if let Some(id) = cache.get(&rel_buf) {
                return Ok(id.clone());
            }
        }

        let id = self
            .drive
            .ensure_remote_dir_path(&self.drive_root_id, rel_dir)
            .await?;
        let mut cache = self.dir_cache.write().await;
        cache.insert(rel_buf, id.clone());
        Ok(id)
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
        if !self.local_root.exists() {
            bail!(
                "Sync root directory {:?} does not exist or is unmounted! Aborting sync to prevent cascading remote deletion.",
                self.local_root
            );
        }

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

            let local_md5 = match compute_file_md5_async(abs_path.clone()).await {
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

                // File was modified locally! Check for two-way conflict with remote
                let is_conflict = match self.drive.get_file_metadata(&record.drive_id).await {
                    Ok(remote) => {
                        if let Some(ref remote_md5) = remote.md5_checksum {
                            // If remote MD5 also differs from last sync record, both local and remote changed
                            remote_md5 != &record.md5_checksum
                        } else {
                            false
                        }
                    }
                    Err(_) => false,
                };

                if is_conflict {
                    let file_stem = abs_path
                        .file_stem()
                        .map(|s| s.to_string_lossy().to_string())
                        .unwrap_or_else(|| "file".to_string());
                    let extension = abs_path
                        .extension()
                        .map(|e| format!(".{}", e.to_string_lossy()))
                        .unwrap_or_default();
                    let now_secs = SystemTime::now()
                        .duration_since(UNIX_EPOCH)
                        .unwrap()
                        .as_secs();
                    let conflict_filename = format!("{}.conflict-{}{}", file_stem, now_secs, extension);
                    let conflict_rel = rel_path
                        .parent()
                        .map(|p| p.join(&conflict_filename))
                        .unwrap_or_else(|| PathBuf::from(&conflict_filename));
                    let conflict_abs = abs_path
                        .parent()
                        .map(|p| p.join(&conflict_filename))
                        .unwrap_or_else(|| PathBuf::from(&conflict_filename));

                    if options.dry_run {
                        println!(
                            "  [CONFLICT] {:?} modified both locally and remotely! Local copy will be saved as {:?}",
                            rel_path, conflict_rel
                        );
                        uploaded += 1;
                    } else {
                        warn!(
                            "Conflict detected on {:?}: modified both locally and on Google Drive. Renaming local copy to {:?}",
                            rel_path, conflict_rel
                        );
                        if let Err(e) = std::fs::rename(abs_path, &conflict_abs) {
                            error!(
                                "Failed to rename conflicting file {:?} to {:?}: {}",
                                abs_path, conflict_abs, e
                            );
                            failed += 1;
                            continue;
                        }

                        upload_items.push(UploadItem {
                            rel_path: conflict_rel,
                            abs_path: conflict_abs,
                            local_md5,
                            modified_secs: file.modified_secs,
                            size_bytes: file.size_bytes,
                            existing_drive_id: None,
                        });
                    }
                } else if options.dry_run {
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
            } else if options.dry_run {
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

        // 2. Detect locally deleted files
        let all_tracked = self.db.list_files_for_root(&self.local_root)?;
        let total_tracked = all_tracked.len();
        let mut missing_records = Vec::new();

        for record in all_tracked {
            let local_abs = self.local_root.join(&record.rel_path);
            if !local_abs.exists() {
                missing_records.push(record);
            }
        }

        let delete_count = missing_records.len();
        let exceeds_threshold = delete_count > 50 || (total_tracked >= 10 && delete_count * 5 > total_tracked);

        if exceeds_threshold {
            if options.dry_run {
                println!(
                    "  [SAFETY WARNING] Deletion of {} of {} files exceeds safe threshold (>20% or >50 files). Actual run will require `--force`.",
                    delete_count, total_tracked
                );
            } else if !options.force {
                bail!(
                    "Safety guard triggered: deletion of {} files exceeds safe threshold (total tracked: {}). \
                     This often occurs if an external drive was unmounted or folder moved. \
                     If this is intentional, re-run with `--force` to proceed.",
                    delete_count, total_tracked
                );
            }
        }

        for record in missing_records {
            if options.dry_run {
                let action = if options.permanent_delete { "DELETE" } else { "TRASH" };
                println!("  [{}] {:?}", action, record.rel_path);
                deleted += 1;
            } else {
                let action_res = if options.permanent_delete {
                    info!(
                        "Permanently deleting remote file for missing local file: {:?}",
                        record.rel_path
                    );
                    self.drive.delete_file(&record.drive_id).await
                } else {
                    info!(
                        "Moving remote file to trash for missing local file: {:?}",
                        record.rel_path
                    );
                    self.drive.trash_file(&record.drive_id).await
                };

                if let Err(err) = action_res {
                    warn!(
                        "Failed to delete/trash remote Drive file {}: {}",
                        record.drive_id, err
                    );
                }
                self.db.delete_file(&self.local_root, &record.rel_path)?;
                deleted += 1;
            }
        }

        // 3. Perform uploads concurrently (if not dry-run)
        if !options.dry_run && !upload_items.is_empty() {
            let mut parent_map = HashMap::new();
            parent_map.insert(PathBuf::new(), self.drive_root_id.clone());

            for item in &upload_items {
                let p = item
                    .rel_path
                    .parent()
                    .unwrap_or_else(|| Path::new(""))
                    .to_path_buf();
                if !parent_map.contains_key(&p) {
                    match self.ensure_remote_dir_cached(&p).await {
                        Ok(id) => {
                            parent_map.insert(p, id);
                        }
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
                let is_doc = child.is_google_doc();
                let export_info = child.get_export_mime_and_ext();

                let (export_mime, child_rel_path, child_abs_path) = if let Some((mime, ext)) = export_info {
                    let filename = if sanitized_name.ends_with(&format!(".{}", ext)) {
                        sanitized_name.clone()
                    } else {
                        format!("{}.{}", sanitized_name, ext)
                    };
                    let rel = relative_dir.join(&filename);
                    let abs = self.local_root.join(&rel);
                    (Some(mime.to_string()), rel, abs)
                } else if is_doc {
                    warn!("Skipping unsupported Google Workspace file: {:?} ({:?})", child.name, child.mime_type);
                    unchanged += 1;
                    continue;
                } else {
                    let rel = relative_dir.join(&sanitized_name);
                    let abs = self.local_root.join(&rel);
                    (None, rel, abs)
                };

                if filter.is_ignored(&child_abs_path, child.is_folder()) {
                    continue;
                }

                if child.is_folder() {
                    if !options.dry_run && !child_abs_path.exists() {
                        if let Err(e) = tokio::fs::create_dir_all(&child_abs_path).await {
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
                    } else if export_mime.is_some() {
                        existing_record.is_none()
                    } else if let Some(rec) = existing_record {
                        rec.md5_checksum != remote_md5
                    } else {
                        let local_md5 = compute_file_md5_async(child_abs_path.clone())
                            .await
                            .unwrap_or_default();
                        local_md5 != remote_md5
                    };

                    if needs_download {
                        let size_bytes = child.size_bytes();
                        if options.dry_run {
                            let action = if export_mime.is_some() { "EXPORT" } else { "DOWNLOAD" };
                            println!("  [{}] {:?} ({})", action, child_rel_path, format_size(size_bytes));
                            downloaded += 1;
                        } else {
                            download_items.push(DownloadItem {
                                child_id: child.id,
                                child_rel_path,
                                child_abs_path,
                                remote_md5,
                                size_bytes,
                                export_mime,
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

                    let download_res = if let Some(ref export_mime) = item.export_mime {
                        drive.export_file(&item.child_id, export_mime, &item.child_abs_path).await
                    } else {
                        drive.download_file(&item.child_id, &item.child_abs_path).await
                    };

                    if let Some(ref bar) = pb_clone {
                        bar.inc(1);
                    }

                    match download_res {
                        Ok(()) => {
                            let mtime = tokio::fs::metadata(&item.child_abs_path)
                                .await
                                .ok()
                                .and_then(|m| m.modified().ok())
                                .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                                .map(|d| d.as_secs() as i64)
                                .unwrap_or(0);

                            let final_md5 = if item.remote_md5.is_empty() {
                                compute_file_md5_async(item.child_abs_path.clone())
                                    .await
                                    .unwrap_or_default()
                            } else {
                                item.remote_md5
                            };

                            let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs() as i64;
                            let _ = db.upsert_file(&TrackedFileRecord {
                                local_root,
                                rel_path: item.child_rel_path.clone(),
                                drive_id: item.child_id,
                                md5_checksum: final_md5,
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
                    self.ensure_remote_dir_cached(&rel_path).await?;
                    return Ok(());
                }

                if !abs_path.exists() {
                    return Ok(());
                }

                let local_md5 = compute_file_md5_async(abs_path.clone()).await?;
                let existing_record = self.db.get_file(&self.local_root, &rel_path)?;

                if let Some(ref record) = existing_record {
                    if record.md5_checksum == local_md5 {
                        return Ok(());
                    }
                }

                let parent_dir = rel_path.parent().unwrap_or_else(|| Path::new(""));
                let target_parent_id = self.ensure_remote_dir_cached(parent_dir).await?;

                let file_name = rel_path.file_name().unwrap().to_string_lossy();
                let existing_id = existing_record.as_ref().map(|r| r.drive_id.as_str());

                info!("Watcher triggered upload for: {:?}", rel_path);
                let drive_file = self
                    .drive
                    .upload_resumable(&target_parent_id, &file_name, &abs_path, existing_id)
                    .await?;

                let metadata = tokio::fs::metadata(&abs_path).await?;
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_sync_options_defaults() {
        let options = SyncOptions::default();
        assert!(!options.dry_run);
        assert!(!options.permanent_delete);
        assert!(!options.force);
        assert_eq!(options.concurrency, 4);
        assert!(options.show_progress);
    }

    #[test]
    fn test_bulk_deletion_threshold_logic() {
        let is_exceeded = |delete_count: usize, total_tracked: usize| -> bool {
            delete_count > 50 || (total_tracked >= 10 && delete_count * 5 > total_tracked)
        };

        // Below 10 total: even if deleting all, does not exceed unless > 50
        assert!(!is_exceeded(3, 5));
        assert!(!is_exceeded(9, 9));

        // 10 or more: 20% rule
        // 2 out of 10 is 20% (not strictly > 20%)
        assert!(!is_exceeded(2, 10));
        // 3 out of 10 is 30% (> 20%)
        assert!(is_exceeded(3, 10));

        // 100 files: 21 deleted triggers guard
        assert!(is_exceeded(21, 100));
        assert!(!is_exceeded(20, 100));

        // Above 50 files: always triggers guard
        assert!(is_exceeded(51, 1000));
    }
}

