use anyhow::{Context, Result};
use rusqlite::{params, Connection};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use tracing::debug;

#[derive(Debug, Clone, PartialEq)]
pub struct SyncRootRecord {
    pub local_root: PathBuf,
    pub drive_root_id: String,
    pub drive_root_name: Option<String>,
    pub created_at: i64,
    pub last_sync_at: i64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct TrackedFileRecord {
    pub local_root: PathBuf,
    pub rel_path: PathBuf,
    pub drive_id: String,
    pub md5_checksum: String,
    pub modified_secs: i64,
    pub size_bytes: u64,
    pub is_directory: bool,
    pub last_sync_timestamp: i64,
}

#[derive(Debug, Clone, Default)]
pub struct DbStats {
    pub total_roots: u64,
    pub total_files: u64,
    pub total_size_bytes: u64,
}

#[derive(Clone)]
pub struct Database {
    conn: Arc<Mutex<Connection>>,
    db_path: PathBuf,
}

impl Database {
    /// Opens or creates SQLite database at the specified path and runs migrations.
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("Failed to create database directory {:?}", parent))?;
        }

        let conn = Connection::open(path)
            .with_context(|| format!("Failed to open SQLite database at {:?}", path))?;

        // Enable WAL mode and optimizations
        conn.execute_batch(
            "PRAGMA journal_mode = WAL;
             PRAGMA synchronous = NORMAL;
             PRAGMA foreign_keys = ON;
             PRAGMA busy_timeout = 5000;",
        )
        .context("Failed to set SQLite pragmas")?;

        let db = Self {
            conn: Arc::new(Mutex::new(conn)),
            db_path: path.to_path_buf(),
        };

        db.migrate()?;
        Ok(db)
    }

    /// Opens the default database at ~/.config/gdsync/state.db
    pub fn open_default() -> Result<Self> {
        let path = crate::config::database_path()?;
        Self::open(&path)
    }

    /// Runs database table creation and schema migrations.
    fn migrate(&self) -> Result<()> {
        let conn = self.conn.lock().unwrap();

        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS sync_roots (
                local_root TEXT PRIMARY KEY,
                drive_root_id TEXT NOT NULL,
                drive_root_name TEXT,
                created_at INTEGER NOT NULL,
                last_sync_at INTEGER NOT NULL
            );

            CREATE TABLE IF NOT EXISTS tracked_files (
                local_root TEXT NOT NULL,
                rel_path TEXT NOT NULL,
                drive_id TEXT NOT NULL,
                md5_checksum TEXT NOT NULL,
                modified_secs INTEGER NOT NULL,
                size_bytes INTEGER NOT NULL,
                is_directory INTEGER NOT NULL DEFAULT 0,
                last_sync_timestamp INTEGER NOT NULL,
                PRIMARY KEY (local_root, rel_path),
                FOREIGN KEY (local_root) REFERENCES sync_roots(local_root) ON DELETE CASCADE
            );

            CREATE INDEX IF NOT EXISTS idx_tracked_files_drive_id 
                ON tracked_files(local_root, drive_id);",
        )
        .context("Failed to run database migrations")?;

        debug!("Database migrations successfully applied at {:?}", self.db_path);
        Ok(())
    }

    pub fn upsert_root(&self, record: &SyncRootRecord) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO sync_roots (local_root, drive_root_id, drive_root_name, created_at, last_sync_at)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(local_root) DO UPDATE SET
                drive_root_id = excluded.drive_root_id,
                drive_root_name = excluded.drive_root_name,
                last_sync_at = excluded.last_sync_at;",
            params![
                record.local_root.to_string_lossy(),
                record.drive_root_id,
                record.drive_root_name,
                record.created_at,
                record.last_sync_at,
            ],
        )
        .context("Failed to upsert sync root")?;
        Ok(())
    }

    pub fn get_root(&self, local_root: &Path) -> Result<Option<SyncRootRecord>> {
        let conn = self.conn.lock().unwrap();
        let canonical = std::fs::canonicalize(local_root).unwrap_or_else(|_| local_root.to_path_buf());
        let path_str = canonical.to_string_lossy();

        let mut stmt = conn.prepare(
            "SELECT local_root, drive_root_id, drive_root_name, created_at, last_sync_at
             FROM sync_roots WHERE local_root = ?1",
        )?;

        let mut rows = stmt.query(params![path_str])?;
        if let Some(row) = rows.next()? {
            let root_str: String = row.get(0)?;
            Ok(Some(SyncRootRecord {
                local_root: PathBuf::from(root_str),
                drive_root_id: row.get(1)?,
                drive_root_name: row.get(2)?,
                created_at: row.get(3)?,
                last_sync_at: row.get(4)?,
            }))
        } else {
            Ok(None)
        }
    }

    pub fn list_roots(&self) -> Result<Vec<SyncRootRecord>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT local_root, drive_root_id, drive_root_name, created_at, last_sync_at
             FROM sync_roots ORDER BY created_at ASC",
        )?;

        let rows = stmt.query_map([], |row| {
            let root_str: String = row.get(0)?;
            Ok(SyncRootRecord {
                local_root: PathBuf::from(root_str),
                drive_root_id: row.get(1)?,
                drive_root_name: row.get(2)?,
                created_at: row.get(3)?,
                last_sync_at: row.get(4)?,
            })
        })?;

        let mut list = Vec::new();
        for item in rows {
            list.push(item?);
        }
        Ok(list)
    }

    pub fn update_last_sync(&self, local_root: &Path, timestamp: i64) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        let path_str = local_root.to_string_lossy();
        conn.execute(
            "UPDATE sync_roots SET last_sync_at = ?1 WHERE local_root = ?2",
            params![timestamp, path_str],
        )?;
        Ok(())
    }

    pub fn upsert_file(&self, file: &TrackedFileRecord) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO tracked_files (
                local_root, rel_path, drive_id, md5_checksum,
                modified_secs, size_bytes, is_directory, last_sync_timestamp
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
             ON CONFLICT(local_root, rel_path) DO UPDATE SET
                drive_id = excluded.drive_id,
                md5_checksum = excluded.md5_checksum,
                modified_secs = excluded.modified_secs,
                size_bytes = excluded.size_bytes,
                is_directory = excluded.is_directory,
                last_sync_timestamp = excluded.last_sync_timestamp;",
            params![
                file.local_root.to_string_lossy(),
                file.rel_path.to_string_lossy(),
                file.drive_id,
                file.md5_checksum,
                file.modified_secs,
                file.size_bytes,
                if file.is_directory { 1 } else { 0 },
                file.last_sync_timestamp,
            ],
        )
        .context("Failed to upsert tracked file")?;
        Ok(())
    }

    pub fn get_file(&self, local_root: &Path, rel_path: &Path) -> Result<Option<TrackedFileRecord>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT local_root, rel_path, drive_id, md5_checksum, modified_secs,
                    size_bytes, is_directory, last_sync_timestamp
             FROM tracked_files
             WHERE local_root = ?1 AND rel_path = ?2",
        )?;

        let mut rows = stmt.query(params![
            local_root.to_string_lossy(),
            rel_path.to_string_lossy(),
        ])?;

        if let Some(row) = rows.next()? {
            let root_str: String = row.get(0)?;
            let rel_str: String = row.get(1)?;
            let is_dir: i32 = row.get(6)?;

            Ok(Some(TrackedFileRecord {
                local_root: PathBuf::from(root_str),
                rel_path: PathBuf::from(rel_str),
                drive_id: row.get(2)?,
                md5_checksum: row.get(3)?,
                modified_secs: row.get(4)?,
                size_bytes: row.get(5)?,
                is_directory: is_dir != 0,
                last_sync_timestamp: row.get(7)?,
            }))
        } else {
            Ok(None)
        }
    }

    pub fn get_file_by_drive_id(
        &self,
        local_root: &Path,
        drive_id: &str,
    ) -> Result<Option<TrackedFileRecord>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT local_root, rel_path, drive_id, md5_checksum, modified_secs,
                    size_bytes, is_directory, last_sync_timestamp
             FROM tracked_files
             WHERE local_root = ?1 AND drive_id = ?2",
        )?;

        let mut rows = stmt.query(params![local_root.to_string_lossy(), drive_id])?;

        if let Some(row) = rows.next()? {
            let root_str: String = row.get(0)?;
            let rel_str: String = row.get(1)?;
            let is_dir: i32 = row.get(6)?;

            Ok(Some(TrackedFileRecord {
                local_root: PathBuf::from(root_str),
                rel_path: PathBuf::from(rel_str),
                drive_id: row.get(2)?,
                md5_checksum: row.get(3)?,
                modified_secs: row.get(4)?,
                size_bytes: row.get(5)?,
                is_directory: is_dir != 0,
                last_sync_timestamp: row.get(7)?,
            }))
        } else {
            Ok(None)
        }
    }

    pub fn list_files_for_root(&self, local_root: &Path) -> Result<Vec<TrackedFileRecord>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT local_root, rel_path, drive_id, md5_checksum, modified_secs,
                    size_bytes, is_directory, last_sync_timestamp
             FROM tracked_files
             WHERE local_root = ?1
             ORDER BY rel_path ASC",
        )?;

        let rows = stmt.query_map(params![local_root.to_string_lossy()], |row| {
            let root_str: String = row.get(0)?;
            let rel_str: String = row.get(1)?;
            let is_dir: i32 = row.get(6)?;

            Ok(TrackedFileRecord {
                local_root: PathBuf::from(root_str),
                rel_path: PathBuf::from(rel_str),
                drive_id: row.get(2)?,
                md5_checksum: row.get(3)?,
                modified_secs: row.get(4)?,
                size_bytes: row.get(5)?,
                is_directory: is_dir != 0,
                last_sync_timestamp: row.get(7)?,
            })
        })?;

        let mut list = Vec::new();
        for item in rows {
            list.push(item?);
        }
        Ok(list)
    }

    pub fn delete_file(&self, local_root: &Path, rel_path: &Path) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "DELETE FROM tracked_files WHERE local_root = ?1 AND rel_path = ?2",
            params![local_root.to_string_lossy(), rel_path.to_string_lossy()],
        )?;
        Ok(())
    }

    pub fn get_db_stats(&self) -> Result<DbStats> {
        let conn = self.conn.lock().unwrap();

        let total_roots: u64 = conn.query_row(
            "SELECT COUNT(*) FROM sync_roots",
            [],
            |r| r.get(0),
        ).unwrap_or(0);

        let (total_files, total_size_bytes): (u64, u64) = conn.query_row(
            "SELECT COUNT(*), COALESCE(SUM(size_bytes), 0) FROM tracked_files WHERE is_directory = 0",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        ).unwrap_or((0, 0));

        Ok(DbStats {
            total_roots,
            total_files,
            total_size_bytes,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn test_db_operations() -> Result<()> {
        let dir = tempdir()?;
        let db_path = dir.path().join("test_state.db");
        let db = Database::open(&db_path)?;

        let root_path = PathBuf::from("/home/user/project");
        let root = SyncRootRecord {
            local_root: root_path.clone(),
            drive_root_id: "drive_folder_123".to_string(),
            drive_root_name: Some("project".to_string()),
            created_at: 1000,
            last_sync_at: 1000,
        };
        db.upsert_root(&root)?;

        let fetched_root = db.get_root(&root_path)?.expect("Root should exist");
        assert_eq!(fetched_root.drive_root_id, "drive_folder_123");

        let file = TrackedFileRecord {
            local_root: root_path.clone(),
            rel_path: PathBuf::from("src/main.rs"),
            drive_id: "file_456".to_string(),
            md5_checksum: "abcd1234ef".to_string(),
            modified_secs: 1050,
            size_bytes: 42,
            is_directory: false,
            last_sync_timestamp: 1050,
        };
        db.upsert_file(&file)?;

        let fetched_file = db.get_file(&root_path, &PathBuf::from("src/main.rs"))?
            .expect("File should exist");
        assert_eq!(fetched_file.drive_id, "file_456");
        assert_eq!(fetched_file.md5_checksum, "abcd1234ef");

        let stats = db.get_db_stats()?;
        assert_eq!(stats.total_roots, 1);
        assert_eq!(stats.total_files, 1);
        assert_eq!(stats.total_size_bytes, 42);

        db.delete_file(&root_path, &PathBuf::from("src/main.rs"))?;
        assert!(db.get_file(&root_path, &PathBuf::from("src/main.rs"))?.is_none());

        Ok(())
    }
}
