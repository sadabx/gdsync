use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};

const DEFAULT_DEBOUNCE_MS: u64 = 500;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct WatchedDirectory {
    pub local_path: PathBuf,
    pub drive_folder_id: String,
    #[serde(default)]
    pub drive_folder_name: Option<String>,
    #[serde(default = "default_sync_deletes")]
    pub sync_deletes: bool,
    #[serde(default = "default_debounce_ms")]
    pub debounce_ms: u64,
}

fn default_sync_deletes() -> bool {
    true
}

fn default_debounce_ms() -> u64 {
    DEFAULT_DEBOUNCE_MS
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Config {
    #[serde(default)]
    pub client_id: Option<String>,
    #[serde(default)]
    pub client_secret: Option<String>,
    #[serde(default)]
    pub directories: Vec<WatchedDirectory>,
}

pub fn config_dir() -> Result<PathBuf> {
    let dir = dirs::config_dir()
        .context("Unable to determine user config directory")?
        .join("gdsync");
    if !dir.exists() {
        fs::create_dir_all(&dir)
            .with_context(|| format!("Failed to create config directory at {:?}", dir))?;
    }
    Ok(dir)
}

pub fn config_path() -> Result<PathBuf> {
    Ok(config_dir()?.join("config.toml"))
}

pub fn database_path() -> Result<PathBuf> {
    Ok(config_dir()?.join("state.db"))
}

pub fn token_path() -> Result<PathBuf> {
    Ok(config_dir()?.join("token.json"))
}

pub fn load_config() -> Result<Config> {
    let path = config_path()?;
    if !path.exists() {
        let default_config = Config::default();
        save_config(&default_config)?;
        return Ok(default_config);
    }

    let contents = fs::read_to_string(&path)
        .with_context(|| format!("Failed to read config from {:?}", path))?;
    let config: Config = toml::from_str(&contents)
        .with_context(|| format!("Failed to parse TOML configuration from {:?}", path))?;
    Ok(config)
}

pub fn save_config(config: &Config) -> Result<()> {
    let path = config_path()?;
    let toml_str = toml::to_string_pretty(config).context("Failed to serialize configuration")?;
    fs::write(&path, toml_str)
        .with_context(|| format!("Failed to write configuration to {:?}", path))?;
    Ok(())
}

pub fn add_or_update_directory(
    local_path: &Path,
    drive_folder_id: &str,
    drive_folder_name: Option<String>,
) -> Result<()> {
    let canonical = fs::canonicalize(local_path)
        .with_context(|| format!("Failed to canonicalize path {:?}", local_path))?;
    let mut config = load_config()?;

    if let Some(existing) = config
        .directories
        .iter_mut()
        .find(|d| d.local_path == canonical)
    {
        existing.drive_folder_id = drive_folder_id.to_string();
        existing.drive_folder_name = drive_folder_name;
    } else {
        config.directories.push(WatchedDirectory {
            local_path: canonical,
            drive_folder_id: drive_folder_id.to_string(),
            drive_folder_name,
            sync_deletes: true,
            debounce_ms: DEFAULT_DEBOUNCE_MS,
        });
    }

    save_config(&config)?;
    Ok(())
}

pub fn find_directory_config(local_path: &Path) -> Result<Option<WatchedDirectory>> {
    let canonical = fs::canonicalize(local_path)
        .or_else(|_| Ok::<_, std::io::Error>(local_path.to_path_buf()))?;
    let config = load_config()?;
    Ok(config
        .directories
        .into_iter()
        .find(|d| d.local_path == canonical))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn test_config_serde_and_directory_management() -> Result<()> {
        let dir = tempdir()?;
        let sample_dir = dir.path().join("my_repo");
        fs::create_dir_all(&sample_dir)?;

        let mut cfg = Config::default();
        cfg.client_id = Some("test-client-id".to_string());
        cfg.directories.push(WatchedDirectory {
            local_path: sample_dir.clone(),
            drive_folder_id: "drive_folder_abc".to_string(),
            drive_folder_name: Some("my_repo".to_string()),
            sync_deletes: true,
            debounce_ms: 300,
        });

        let serialized = toml::to_string_pretty(&cfg)?;
        let deserialized: Config = toml::from_str(&serialized)?;

        assert_eq!(deserialized.client_id, Some("test-client-id".to_string()));
        assert_eq!(deserialized.directories.len(), 1);
        assert_eq!(deserialized.directories[0].drive_folder_id, "drive_folder_abc");
        assert_eq!(deserialized.directories[0].debounce_ms, 300);

        Ok(())
    }
}
