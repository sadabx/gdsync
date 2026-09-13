use crate::auth::TokenManager;
use anyhow::{bail, Context, Result};
use reqwest::{header, StatusCode};
use serde::{Deserialize, Serialize};
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;
use std::sync::Arc;
use tokio::sync::Mutex;
use tracing::debug;

const DRIVE_FILES_API: &str = "https://www.googleapis.com/drive/v3/files";
const DRIVE_UPLOAD_API: &str = "https://www.googleapis.com/upload/drive/v3/files";
const FOLDER_MIME_TYPE: &str = "application/vnd.google-apps.folder";

// 5 MiB chunk size for resumable uploads (must be multiple of 256 KiB)
const UPLOAD_CHUNK_SIZE: usize = 5 * 1024 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DriveFile {
    pub id: String,
    pub name: String,
    #[serde(rename = "mimeType", default)]
    pub mime_type: String,
    #[serde(rename = "md5Checksum", default)]
    pub md5_checksum: Option<String>,
    #[serde(rename = "modifiedTime", default)]
    pub modified_time: Option<String>,
    #[serde(default)]
    pub size: Option<String>,
    #[serde(default)]
    pub trashed: Option<bool>,
    #[serde(default)]
    pub parents: Option<Vec<String>>,
}

impl DriveFile {
    pub fn is_folder(&self) -> bool {
        self.mime_type == FOLDER_MIME_TYPE
    }

    pub fn size_bytes(&self) -> u64 {
        self.size
            .as_ref()
            .and_then(|s| s.parse::<u64>().ok())
            .unwrap_or(0)
    }
}

#[derive(Debug, Deserialize)]
struct FileListResponse {
    #[serde(default)]
    files: Vec<DriveFile>,
    #[serde(rename = "nextPageToken", default)]
    next_page_token: Option<String>,
}

#[derive(Clone)]
pub struct DriveClient {
    token_manager: Arc<Mutex<TokenManager>>,
    client: reqwest::Client,
}

impl DriveClient {
    pub fn new(token_manager: TokenManager) -> Self {
        Self {
            token_manager: Arc::new(Mutex::new(token_manager)),
            client: reqwest::Client::new(),
        }
    }

    pub async fn from_auth() -> Result<Self> {
        let tm = TokenManager::load_or_init().await?;
        Ok(Self::new(tm))
    }

    async fn auth_header(&self) -> Result<String> {
        let mut tm = self.token_manager.lock().await;
        let token = tm.get_access_token().await?;
        Ok(format!("Bearer {}", token))
    }

    /// Fetches file or folder metadata by ID.
    pub async fn get_file_metadata(&self, file_id: &str) -> Result<DriveFile> {
        let auth = self.auth_header().await?;
        let url = format!(
            "{}/{}?fields=id,name,mimeType,md5Checksum,modifiedTime,size,trashed,parents",
            DRIVE_FILES_API, file_id
        );

        let resp = self
            .client
            .get(&url)
            .header(header::AUTHORIZATION, &auth)
            .send()
            .await
            .context("Failed to request Drive file metadata")?;

        if !resp.status().is_success() {
            let err = resp.text().await.unwrap_or_default();
            bail!("Drive get_file_metadata failed for {}: {}", file_id, err);
        }

        let file: DriveFile = resp.json().await.context("Failed to parse DriveFile JSON")?;
        Ok(file)
    }

    /// Finds a child file/folder by name inside a parent folder.
    pub async fn find_child_by_name(
        &self,
        parent_id: &str,
        name: &str,
    ) -> Result<Option<DriveFile>> {
        let auth = self.auth_header().await?;
        // Escape single quotes for Google Drive query syntax
        let escaped_name = name.replace('\\', "\\\\").replace('\'', "\\'");
        let q = format!(
            "'{}' in parents and name = '{}' and trashed = false",
            parent_id, escaped_name
        );

        let resp = self
            .client
            .get(DRIVE_FILES_API)
            .header(header::AUTHORIZATION, &auth)
            .query(&[
                ("q", q.as_str()),
                (
                    "fields",
                    "files(id,name,mimeType,md5Checksum,modifiedTime,size,trashed,parents)",
                ),
                ("pageSize", "10"),
            ])
            .send()
            .await
            .context("Failed to search Drive files")?;

        if !resp.status().is_success() {
            let err = resp.text().await.unwrap_or_default();
            bail!("Drive find_child_by_name failed: {}", err);
        }

        let list: FileListResponse = resp.json().await.context("Failed to parse file list")?;
        Ok(list.files.into_iter().next())
    }

    /// Lists all active (non-trashed) children in a parent folder.
    pub async fn list_children(&self, parent_id: &str) -> Result<Vec<DriveFile>> {
        let mut all_files = Vec::new();
        let mut page_token: Option<String> = None;

        loop {
            let auth = self.auth_header().await?;
            let q = format!("'{}' in parents and trashed = false", parent_id);

            let mut req = self
                .client
                .get(DRIVE_FILES_API)
                .header(header::AUTHORIZATION, &auth)
                .query(&[
                    ("q", q.as_str()),
                    (
                        "fields",
                        "nextPageToken,files(id,name,mimeType,md5Checksum,modifiedTime,size,trashed,parents)",
                    ),
                    ("pageSize", "1000"),
                ]);

            if let Some(token) = &page_token {
                req = req.query(&[("pageToken", token.as_str())]);
            }

            let resp = req.send().await.context("Failed to list files in folder")?;
            if !resp.status().is_success() {
                let err = resp.text().await.unwrap_or_default();
                bail!("Drive list_children failed: {}", err);
            }

            let list: FileListResponse = resp.json().await.context("Failed to parse file list")?;
            all_files.extend(list.files);

            page_token = list.next_page_token;
            if page_token.is_none() {
                break;
            }
        }

        Ok(all_files)
    }

    /// Creates a new directory in Google Drive under the specified parent.
    pub async fn create_folder(&self, parent_id: &str, folder_name: &str) -> Result<DriveFile> {
        let auth = self.auth_header().await?;

        let body = serde_json::json!({
            "name": folder_name,
            "mimeType": FOLDER_MIME_TYPE,
            "parents": [parent_id]
        });

        let resp = self
            .client
            .post(DRIVE_FILES_API)
            .header(header::AUTHORIZATION, &auth)
            .header(header::CONTENT_TYPE, "application/json")
            .query(&[(
                "fields",
                "id,name,mimeType,md5Checksum,modifiedTime,size,trashed,parents",
            )])
            .json(&body)
            .send()
            .await
            .context("Failed to send create folder request")?;

        if !resp.status().is_success() {
            let err = resp.text().await.unwrap_or_default();
            bail!("Failed to create Drive folder '{}': {}", folder_name, err);
        }

        let folder: DriveFile = resp.json().await.context("Failed to parse created folder")?;
        debug!("Created remote folder '{}' (id: {})", folder_name, folder.id);
        Ok(folder)
    }

    /// Recursively ensures all directories along relative_dir exist in Google Drive,
    /// returning the target folder's Drive ID.
    pub async fn ensure_remote_dir_path(
        &self,
        drive_root_id: &str,
        relative_dir: &Path,
    ) -> Result<String> {
        let mut current_id = drive_root_id.to_string();

        for component in relative_dir.components() {
            if let std::path::Component::Normal(c) = component {
                let name = c.to_string_lossy();
                if name.is_empty() || name == "." {
                    continue;
                }

                // Check if folder already exists under current_id
                if let Some(child) = self.find_child_by_name(&current_id, &name).await? {
                    if child.is_folder() {
                        current_id = child.id;
                        continue;
                    } else {
                        bail!(
                            "Path collision: '{}' exists under parent {} but is not a folder",
                            name,
                            current_id
                        );
                    }
                }

                // Create folder if not found
                let new_folder = self.create_folder(&current_id, &name).await?;
                current_id = new_folder.id;
            }
        }

        Ok(current_id)
    }

    /// Uploads a file to Google Drive using chunked resumable upload.
    /// If `existing_file_id` is specified, updates the existing file;
    /// otherwise creates a new file in `parent_id`.
    pub async fn upload_resumable(
        &self,
        parent_id: &str,
        file_name: &str,
        local_path: &Path,
        existing_file_id: Option<&str>,
    ) -> Result<DriveFile> {
        let mut file = File::open(local_path)
            .with_context(|| format!("Failed to open file for upload: {:?}", local_path))?;
        let total_size = file.metadata()?.len();

        let auth = self.auth_header().await?;

        // Step 1: Initiate Resumable Upload Session
        let session_resp = if let Some(file_id) = existing_file_id {
            let session_url = format!("{}/{}?uploadType=resumable", DRIVE_UPLOAD_API, file_id);
            self.client
                .patch(&session_url)
                .header(header::AUTHORIZATION, &auth)
                .header(header::CONTENT_TYPE, "application/json; charset=UTF-8")
                .header("X-Upload-Content-Type", "application/octet-stream")
                .header("X-Upload-Content-Length", total_size.to_string())
                .json(&serde_json::json!({}))
                .send()
                .await?
        } else {
            let session_url = format!("{}?uploadType=resumable", DRIVE_UPLOAD_API);
            let metadata = serde_json::json!({
                "name": file_name,
                "parents": [parent_id]
            });

            self.client
                .post(&session_url)
                .header(header::AUTHORIZATION, &auth)
                .header(header::CONTENT_TYPE, "application/json; charset=UTF-8")
                .header("X-Upload-Content-Type", "application/octet-stream")
                .header("X-Upload-Content-Length", total_size.to_string())
                .json(&metadata)
                .send()
                .await?
        };

        if !session_resp.status().is_success() {
            let err = session_resp.text().await.unwrap_or_default();
            bail!("Failed to initiate resumable upload session: {}", err);
        }

        let upload_url = session_resp
            .headers()
            .get(header::LOCATION)
            .context("Google Drive did not return Location header for resumable upload")?
            .to_str()?
            .to_string();

        debug!(
            "Initiated resumable upload session for '{}' (size: {} bytes)",
            file_name, total_size
        );

        // Special case: 0-byte file
        if total_size == 0 {
            let resp = self
                .client
                .put(&upload_url)
                .header(header::CONTENT_LENGTH, "0")
                .header(header::CONTENT_RANGE, "bytes */0")
                .send()
                .await
                .context("Failed to upload 0-byte file")?;

            if !resp.status().is_success() {
                let err = resp.text().await.unwrap_or_default();
                bail!("Failed to upload empty file: {}", err);
            }

            let drive_file: DriveFile = resp.json().await?;
            return Ok(drive_file);
        }

        // Step 2: Upload in chunks
        let mut uploaded_bytes: u64 = 0;
        let mut buffer = vec![0u8; UPLOAD_CHUNK_SIZE];

        while uploaded_bytes < total_size {
            let chunk_start = uploaded_bytes;
            let to_read = std::cmp::min(
                UPLOAD_CHUNK_SIZE as u64,
                total_size - uploaded_bytes,
            ) as usize;

            file.seek(SeekFrom::Start(chunk_start))?;
            file.read_exact(&mut buffer[..to_read])?;
            let chunk_end = chunk_start + to_read as u64 - 1;

            let content_range = format!("bytes {}-{}/{}", chunk_start, chunk_end, total_size);

            let chunk_resp = self
                .client
                .put(&upload_url)
                .header(header::CONTENT_LENGTH, to_read.to_string())
                .header(header::CONTENT_RANGE, &content_range)
                .body(buffer[..to_read].to_vec())
                .send()
                .await
                .with_context(|| format!("Failed to upload chunk {}", content_range))?;

            let status = chunk_resp.status();

            if status.is_success() {
                // Upload complete
                let drive_file: DriveFile = chunk_resp
                    .json()
                    .await
                    .context("Failed to parse uploaded DriveFile response")?;
                debug!(
                    "Successfully completed upload for '{}' (ID: {}, MD5: {:?})",
                    file_name, drive_file.id, drive_file.md5_checksum
                );
                return Ok(drive_file);
            } else if status == StatusCode::from_u16(308).unwrap() {
                // Resume Incomplete - proceed with next chunk
                uploaded_bytes = chunk_end + 1;
                debug!("Uploaded chunk: {} (status 308)", content_range);
            } else {
                let err = chunk_resp.text().await.unwrap_or_default();
                bail!("Resumable upload failed on chunk {}: {}", content_range, err);
            }
        }

        bail!("Upload finished loop without receiving completion response");
    }

    /// Downloads a remote file from Google Drive and writes it to `destination_path`.
    pub async fn download_file(&self, file_id: &str, destination_path: &Path) -> Result<()> {
        let auth = self.auth_header().await?;
        let url = format!("{}/{}?alt=media", DRIVE_FILES_API, file_id);

        if let Some(parent) = destination_path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        let resp = self
            .client
            .get(&url)
            .header(header::AUTHORIZATION, &auth)
            .send()
            .await
            .context("Failed to download file from Drive")?;

        if !resp.status().is_success() {
            let err = resp.text().await.unwrap_or_default();
            bail!("Drive download_file failed for {}: {}", file_id, err);
        }

        let bytes = resp.bytes().await.context("Failed to read response body")?;
        std::fs::write(destination_path, &bytes)
            .with_context(|| format!("Failed to write downloaded file to {:?}", destination_path))?;

        debug!("Downloaded Drive file {} to {:?}", file_id, destination_path);
        Ok(())
    }

    /// Permanently deletes or trashes a file/folder in Google Drive.
    pub async fn delete_file(&self, file_id: &str) -> Result<()> {
        let auth = self.auth_header().await?;
        let url = format!("{}/{}", DRIVE_FILES_API, file_id);

        let resp = self
            .client
            .delete(&url)
            .header(header::AUTHORIZATION, &auth)
            .send()
            .await
            .context("Failed to send delete file request")?;

        if !resp.status().is_success() && resp.status() != StatusCode::NOT_FOUND {
            let err = resp.text().await.unwrap_or_default();
            bail!("Drive delete_file failed for {}: {}", file_id, err);
        }

        debug!("Deleted Drive file/folder: {}", file_id);
        Ok(())
    }
}
