use anyhow::{Context, Result};
use ignore::gitignore::{Gitignore, GitignoreBuilder};
use ignore::WalkBuilder;
use md5::{Digest, Md5};
use std::fs::File;
use std::io::{BufReader, Read};
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;
use tracing::{debug, trace};

/// Essential safety patterns that must never be synced to Google Drive.
const MANDATORY_IGNORES: &[&str] = &[
    ".git",
    ".git/**",
    "node_modules",
    "node_modules/**",
    ".venv",
    ".venv/**",
    "venv",
    "venv/**",
    "target",
    "target/**",
    "__pycache__",
    "__pycache__/**",
    ".cache",
    ".cache/**",
    ".gdsync",
    ".gdsync/**",
    ".gdsyncignore",
    "*.tmp",
    "*.swp",
    "*~",
];

#[derive(Debug, Clone)]
pub struct UnignoredFile {
    pub absolute_path: PathBuf,
    pub relative_path: PathBuf,
    pub is_directory: bool,
    pub size_bytes: u64,
    pub modified_secs: i64,
}

#[derive(Debug, Clone)]
pub struct GitignoreFilter {
    root: PathBuf,
    gitignore: Gitignore,
}

impl GitignoreFilter {
    /// Creates a new GitignoreFilter by inspecting root and nested `.gitignore` and `.gdsyncignore` files.
    pub fn new(root: &Path) -> Result<Self> {
        let canonical_root = std::fs::canonicalize(root)
            .with_context(|| format!("Failed to canonicalize filter root {:?}", root))?;

        let mut builder = GitignoreBuilder::new(&canonical_root);

        // Add mandatory safety rules
        for pattern in MANDATORY_IGNORES {
            builder
                .add_line(None, pattern)
                .with_context(|| format!("Failed to add mandatory pattern {}", pattern))?;
        }

        // Recursively find and add all .gitignore and .gdsyncignore files
        for entry in WalkBuilder::new(&canonical_root)
            .hidden(false)
            .git_ignore(false)
            .build()
            .flatten()
        {
            let name = entry.file_name();
            if name == ".gitignore" || name == ".gdsyncignore" {
                let path = entry.path();
                debug!("Adding ignore rules from {:?}", path);
                builder.add(path);
            }
        }

        let gitignore = builder.build().context("Failed to build Gitignore matcher")?;
        Ok(Self {
            root: canonical_root,
            gitignore,
        })
    }

    /// Checks if a file or directory path is ignored.
    pub fn is_ignored(&self, path: &Path, is_dir: bool) -> bool {
        // Fast checks for mandatory names anywhere in the path components
        for comp in path.components() {
            if let std::path::Component::Normal(c) = comp {
                let name = c.to_string_lossy();
                if name == ".git"
                    || name == "node_modules"
                    || name == "target"
                    || name == ".venv"
                    || name == "venv"
                    || name == "__pycache__"
                    || name == ".gdsync"
                {
                    return true;
                }
            }
        }

        let match_result = self.gitignore.matched_path_or_any_parents(path, is_dir);
        match_result.is_ignore()
    }

    /// Recursively walks the directory and returns all unignored files and folders.
    pub fn walk_unignored(&self) -> Vec<UnignoredFile> {
        let mut results = Vec::new();

        let walker = WalkBuilder::new(&self.root)
            .hidden(false)
            .git_ignore(true)
            .git_global(true)
            .git_exclude(true)
            .ignore(true)
            .build();

        for result in walker {
            match result {
                Ok(entry) => {
                    let path = entry.path();
                    if path == self.root {
                        continue;
                    }

                    let is_dir = entry.file_type().map(|ft| ft.is_dir()).unwrap_or(false);

                    // Check our filter including mandatory ignores
                    if self.is_ignored(path, is_dir) {
                        trace!("Skipping ignored path: {:?}", path);
                        continue;
                    }

                    if let Ok(rel_path) = path.strip_prefix(&self.root) {
                        let metadata = match entry.metadata() {
                            Ok(m) => m,
                            Err(err) => {
                                debug!("Failed to read metadata for {:?}: {}", path, err);
                                continue;
                            }
                        };

                        let modified_secs = metadata
                            .modified()
                            .ok()
                            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                            .map(|d| d.as_secs() as i64)
                            .unwrap_or(0);

                        results.push(UnignoredFile {
                            absolute_path: path.to_path_buf(),
                            relative_path: rel_path.to_path_buf(),
                            is_directory: is_dir,
                            size_bytes: if is_dir { 0 } else { metadata.len() },
                            modified_secs,
                        });
                    }
                }
                Err(err) => {
                    debug!("Error encountered during walk: {}", err);
                }
            }
        }

        results
    }
}

/// Computes the MD5 checksum of a local file in lowercase hex format,
/// matching Google Drive's `md5Checksum` format.
pub fn compute_file_md5(path: &Path) -> Result<String> {
    let file = File::open(path).with_context(|| format!("Failed to open file {:?}", path))?;
    let mut reader = BufReader::new(file);
    let mut hasher = Md5::new();
    let mut buffer = [0u8; 64 * 1024];

    loop {
        let count = reader
            .read(&mut buffer)
            .with_context(|| format!("Failed to read file chunks for {:?}", path))?;
        if count == 0 {
            break;
        }
        hasher.update(&buffer[..count]);
    }

    let digest = hasher.finalize();
    Ok(hex::encode(digest))
}

/// Asynchronously computes the MD5 checksum of a local file by offloading synchronous I/O
/// to Tokio's blocking threadpool, preventing worker thread starvation on large files.
pub async fn compute_file_md5_async(path: PathBuf) -> Result<String> {
    tokio::task::spawn_blocking(move || compute_file_md5(&path))
        .await
        .context("Background MD5 computation task panicked")?
}

/// Sanitizes a single filename component (file or directory name) from Google Drive
/// or external sources to be fully valid, safe, and compliant with Linux filesystems (NAME_MAX <= 255 bytes).
///
/// Features:
/// 1. Strips zero-width and invisible unicode characters (e.g. \u{200b} zero-width space, BOM, directional markers).
/// 2. Strips null bytes and non-printable control characters.
/// 3. Replaces directory separators ('/' or '\\') with '_'.
/// 4. Trims leading/trailing whitespace.
/// 5. Clamps byte length to <= 255 bytes on valid UTF-8 character boundaries, preserving file extension.
pub fn sanitize_filename_component(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .filter(|&c| {
            !matches!(
                c,
                '\0' | '\u{200B}'..='\u{200F}' | '\u{202A}'..='\u{202E}' | '\u{2060}' | '\u{FEFF}'
            ) && !c.is_control()
        })
        .map(|c| if c == '/' || c == '\\' { '_' } else { c })
        .collect();

    let cleaned = cleaned.trim();
    let candidate = if cleaned.is_empty() {
        "unnamed_file".to_string()
    } else {
        cleaned.to_string()
    };

    if candidate.as_bytes().len() <= 255 {
        return candidate;
    }

    // If over 255 bytes, preserve extension if reasonably sized (<= 32 bytes)
    if let Some(dot_idx) = candidate.rfind('.') {
        let ext = &candidate[dot_idx..];
        let stem = &candidate[..dot_idx];
        if ext.as_bytes().len() <= 32 && ext.len() < candidate.len() {
            let max_stem_bytes = 255 - ext.as_bytes().len();
            let mut byte_count = 0;
            let mut valid_stem_end = 0;
            for (idx, ch) in stem.char_indices() {
                if byte_count + ch.len_utf8() > max_stem_bytes {
                    break;
                }
                byte_count += ch.len_utf8();
                valid_stem_end = idx + ch.len_utf8();
            }
            return format!("{}{}", &stem[..valid_stem_end], ext);
        }
    }

    // Truncate candidate to 255 bytes at a valid UTF-8 character boundary
    let mut byte_count = 0;
    let mut valid_end = 0;
    for (idx, ch) in candidate.char_indices() {
        if byte_count + ch.len_utf8() > 255 {
            break;
        }
        byte_count += ch.len_utf8();
        valid_end = idx + ch.len_utf8();
    }
    candidate[..valid_end].to_string()
}

/// Formats a byte count into a human-readable string (B, KB, MB, GB).
pub fn format_size(bytes: u64) -> String {
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    #[test]
    fn test_gitignore_and_mandatory_filtering() -> Result<()> {
        let dir = tempdir()?;
        let root = dir.path();

        // Create structure
        fs::create_dir_all(root.join("node_modules/some-pkg"))?;
        fs::write(root.join("node_modules/some-pkg/index.js"), "console.log(1)")?;

        fs::create_dir_all(root.join("target/debug"))?;
        fs::write(root.join("target/debug/bin"), "elf")?;

        fs::create_dir_all(root.join("src"))?;
        fs::write(root.join("src/main.rs"), "fn main() {}")?;

        // Create root .gitignore
        fs::write(root.join(".gitignore"), "*.secret\nignored_dir/\n")?;
        fs::write(root.join("secret.secret"), "password")?;
        fs::create_dir_all(root.join("ignored_dir"))?;
        fs::write(root.join("ignored_dir/data.txt"), "data")?;

        let filter = GitignoreFilter::new(root)?;

        // Test filter
        assert!(filter.is_ignored(&root.join("node_modules/some-pkg/index.js"), false));
        assert!(filter.is_ignored(&root.join("target/debug/bin"), false));
        assert!(filter.is_ignored(&root.join("secret.secret"), false));
        assert!(filter.is_ignored(&root.join("ignored_dir/data.txt"), false));
        assert!(!filter.is_ignored(&root.join("src/main.rs"), false));

        let unignored = filter.walk_unignored();
        let rel_paths: Vec<String> = unignored
            .iter()
            .map(|u| u.relative_path.to_string_lossy().to_string())
            .collect();

        assert!(rel_paths.contains(&"src".to_string()));
        assert!(rel_paths.contains(&"src/main.rs".to_string()));
        assert!(!rel_paths.iter().any(|p| p.starts_with("node_modules")));
        assert!(!rel_paths.iter().any(|p| p.starts_with("target")));
        assert!(!rel_paths.iter().any(|p| p.ends_with(".secret")));
        assert!(!rel_paths.iter().any(|p| p.starts_with("ignored_dir")));

        Ok(())
    }

    #[test]
    fn test_md5_calculation() -> Result<()> {
        let dir = tempdir()?;
        let file_path = dir.path().join("hello.txt");
        std::fs::write(&file_path, "hello world\n")?;

        let md5 = compute_file_md5(&file_path)?;
        // MD5 of "hello world\n" is 6f5902ac237024bdd0c176cb93063dc4
        assert_eq!(md5, "6f5902ac237024bdd0c176cb93063dc4");
        Ok(())
    }

    #[test]
    fn test_sanitize_filename_component() {
        // Zero-width space test from user issue
        let cod_with_zwsp = "C\u{200b}a\u{200b}l\u{200b}l\u{200b} \u{200b}o\u{200b}f\u{200b} \u{200b}D\u{200b}u\u{200b}t\u{200b}y\u{200b}®\u{200b}_\u{200b} \u{200b}M\u{200b}o\u{200b}d\u{200b}e\u{200b}r\u{200b}n\u{200b} \u{200b}W\u{200b}a\u{200b}r\u{200b}f\u{200b}a\u{200b}r\u{200b}e\u{200b}®\u{200b}\u{200b}\u{200b}\u{200b}\u{200b} 6_23_2025 10_47_01 AM.png";
        let cleaned = sanitize_filename_component(cod_with_zwsp);
        assert_eq!(cleaned, "Call of Duty®_ Modern Warfare® 6_23_2025 10_47_01 AM.png");
        assert!(cleaned.as_bytes().len() <= 255);

        // Slash replacement
        assert_eq!(sanitize_filename_component("folder/subname.txt"), "folder_subname.txt");

        // Enforce 255 bytes limit
        let long_stem = "a".repeat(300);
        let long_filename = format!("{}.png", long_stem);
        let clamped = sanitize_filename_component(&long_filename);
        assert!(clamped.as_bytes().len() <= 255);
        assert!(clamped.ends_with(".png"));
    }

    #[test]
    fn test_gdsyncignore_support() -> Result<()> {
        let dir = tempdir()?;
        let root = dir.path();

        // Create .gdsyncignore file
        fs::write(root.join(".gdsyncignore"), "*.raw\ncustom_ignore/\n")?;
        fs::write(root.join("photo.raw"), "raw-data")?;
        fs::write(root.join("photo.jpg"), "jpg-data")?;
        fs::create_dir_all(root.join("custom_ignore"))?;
        fs::write(root.join("custom_ignore/test.txt"), "data")?;

        let filter = GitignoreFilter::new(root)?;

        assert!(filter.is_ignored(&root.join("photo.raw"), false));
        assert!(filter.is_ignored(&root.join("custom_ignore/test.txt"), false));
        assert!(!filter.is_ignored(&root.join("photo.jpg"), false));
        assert!(filter.is_ignored(&root.join(".gdsyncignore"), false));

        Ok(())
    }
}
