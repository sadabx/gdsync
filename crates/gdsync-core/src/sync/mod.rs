use crate::db::{Database, SyncRootRecord, TrackedFileRecord};
use crate::drive::DriveClient;
use crate::filter::{compute_file_md5, GitignoreFilter};
use crate::watcher::{FsChange, FsWatcher};
use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::sync::mpsc;
use tracing::{error, info, warn};

pub struct SyncSummary {
    pub files_uploaded: usize,
    pub files_downloaded: usize,
    pub files_deleted: usize,
    pub files_unchanged: usize,
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

    /// Runs a full push-and-pull reconciliation pass.
    pub async fn reconcile(&self) -> Result<SyncSummary> {
        info!(
            "Starting sync reconciliation for {:?} -> Drive root {}",
            self.local_root, self.drive_root_id
        );

        let push_stats = self.push_reconcile().await?;
        let pull_stats = self.pull_reconcile().await?;

        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        self.db.update_last_sync(&self.local_root, now)?;

        Ok(SyncSummary {
            files_uploaded: push_stats.files_uploaded,
            files_downloaded: pull_stats.files_downloaded,
            files_deleted: push_stats.files_deleted,
            files_unchanged: push_stats.files_unchanged,
        })
    }

    /// Scans local files, respecting .gitignore, and pushes new/modified files to Drive.
    pub async fn push_reconcile(&self) -> Result<SyncSummary> {
        let filter = GitignoreFilter::new(&self.local_root)?;
        let local_files = filter.walk_unignored();

        let mut uploaded = 0;
        let mut unchanged = 0;
        let mut deleted = 0;

        // 1. Process local files
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
                    continue;
                }
            };

            let existing_db_record = self.db.get_file(&self.local_root, rel_path)?;

            if let Some(record) = existing_db_record {
                if record.md5_checksum == local_md5 {
                    unchanged += 1;
                    continue;
                }

                // File content modified -> update existing Drive file
                info!("Uploading modified file: {:?}", rel_path);
                let parent_dir = rel_path.parent().unwrap_or_else(|| Path::new(""));
                let target_parent_id = self
                    .drive
                    .ensure_remote_dir_path(&self.drive_root_id, parent_dir)
                    .await?;

                let file_name = rel_path.file_name().unwrap().to_string_lossy();
                let drive_file = self
                    .drive
                    .upload_resumable(
                        &target_parent_id,
                        &file_name,
                        abs_path,
                        Some(&record.drive_id),
                    )
                    .await?;

                let now = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap()
                    .as_secs() as i64;

                self.db.upsert_file(&TrackedFileRecord {
                    local_root: self.local_root.clone(),
                    rel_path: rel_path.clone(),
                    drive_id: drive_file.id,
                    md5_checksum: drive_file.md5_checksum.unwrap_or(local_md5),
                    modified_secs: file.modified_secs,
                    size_bytes: file.size_bytes,
                    is_directory: false,
                    last_sync_timestamp: now,
                })?;

                uploaded += 1;
            } else {
                // New file -> check if already exists on Drive or upload new
                info!("Uploading new file: {:?}", rel_path);
                let parent_dir = rel_path.parent().unwrap_or_else(|| Path::new(""));
                let target_parent_id = self
                    .drive
                    .ensure_remote_dir_path(&self.drive_root_id, parent_dir)
                    .await?;

                let file_name = rel_path.file_name().unwrap().to_string_lossy();

                // Check if already on Drive
                let existing_remote = self
                    .drive
                    .find_child_by_name(&target_parent_id, &file_name)
                    .await?;

                let drive_file = if let Some(remote) = existing_remote {
                    self.drive
                        .upload_resumable(
                            &target_parent_id,
                            &file_name,
                            abs_path,
                            Some(&remote.id),
                        )
                        .await?
                } else {
                    self.drive
                        .upload_resumable(&target_parent_id, &file_name, abs_path, None)
                        .await?
                };

                let now = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap()
                    .as_secs() as i64;

                self.db.upsert_file(&TrackedFileRecord {
                    local_root: self.local_root.clone(),
                    rel_path: rel_path.clone(),
                    drive_id: drive_file.id,
                    md5_checksum: drive_file.md5_checksum.unwrap_or(local_md5),
                    modified_secs: file.modified_secs,
                    size_bytes: file.size_bytes,
                    is_directory: false,
                    last_sync_timestamp: now,
                })?;

                uploaded += 1;
            }
        }

        // 2. Detect locally deleted files
        let all_tracked = self.db.list_files_for_root(&self.local_root)?;
        for record in all_tracked {
            let local_abs = self.local_root.join(&record.rel_path);
            if !local_abs.exists() {
                info!("Deleting remote file for missing local file: {:?}", record.rel_path);
                if let Err(err) = self.drive.delete_file(&record.drive_id).await {
                    warn!("Failed to delete remote Drive file {}: {}", record.drive_id, err);
                }
                self.db.delete_file(&self.local_root, &record.rel_path)?;
                deleted += 1;
            }
        }

        Ok(SyncSummary {
            files_uploaded: uploaded,
            files_downloaded: 0,
            files_deleted: deleted,
            files_unchanged: unchanged,
        })
    }

    /// Pulls remote files from Google Drive if missing locally or remotely modified.
    pub async fn pull_reconcile(&self) -> Result<SyncSummary> {
        let mut downloaded = 0;
        let mut unchanged = 0;

        let filter = GitignoreFilter::new(&self.local_root)?;
        let mut queue: std::collections::VecDeque<(String, PathBuf)> = std::collections::VecDeque::new();
        queue.push_back((self.drive_root_id.clone(), PathBuf::new()));

        while let Some((remote_folder_id, relative_dir)) = queue.pop_front() {
            let children = self.drive.list_children(&remote_folder_id).await?;

            for child in children {
                let child_rel_path = relative_dir.join(&child.name);
                let child_abs_path = self.local_root.join(&child_rel_path);

                if filter.is_ignored(&child_abs_path, child.is_folder()) {
                    continue;
                }

                if child.is_folder() {
                    if !child_abs_path.exists() {
                        std::fs::create_dir_all(&child_abs_path)?;
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
                        info!("Downloading remote file: {:?}", child_rel_path);
                        self.drive.download_file(&child.id, &child_abs_path).await?;

                        let metadata = std::fs::metadata(&child_abs_path)?;
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
                            rel_path: child_rel_path,
                            drive_id: child.id,
                            md5_checksum: remote_md5,
                            modified_secs: mtime,
                            size_bytes: metadata.len(),
                            is_directory: false,
                            last_sync_timestamp: now,
                        })?;

                        downloaded += 1;
                    } else {
                        unchanged += 1;
                    }
                }
            }
        }

        Ok(SyncSummary {
            files_uploaded: 0,
            files_downloaded: downloaded,
            files_deleted: 0,
            files_unchanged: unchanged,
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
                    // Directory created -> ensure remote folder exists
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
                    info!("Watcher triggered deletion for: {:?}", rel_path);
                    if let Err(err) = self.drive.delete_file(&record.drive_id).await {
                        warn!("Failed to delete Drive file {}: {}", record.drive_id, err);
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
