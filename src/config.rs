use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct AppConfig {
    #[serde(default)]
    pub profiles: Vec<MachineProfile>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MachineProfile {
    pub name: String,
    pub http_base_url: String,
    pub ftp_base_url: String,
    pub output_dir: Option<PathBuf>,
    #[serde(default)]
    pub last_remote_dir: Option<String>,
}

impl AppConfig {
    pub fn load() -> Result<Self> {
        let path = default_config_path();
        Self::load_from_path(&path)
    }

    pub fn save(&self) -> Result<()> {
        let path = default_config_path();
        self.save_to_path(&path)
    }

    pub fn load_from_path(path: &Path) -> Result<Self> {
        if !path.exists() {
            return Ok(Self::default());
        }

        let raw = fs::read_to_string(path)
            .with_context(|| format!("failed to read config file {}", path.display()))?;

        let parsed: Self = toml::from_str(&raw)
            .with_context(|| format!("failed to parse config file {}", path.display()))?;

        Ok(parsed)
    }

    pub fn save_to_path(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).with_context(|| {
                format!("failed to create config directory {}", parent.display())
            })?;
        }

        let encoded = toml::to_string_pretty(self).context("failed to encode config")?;
        fs::write(path, encoded)
            .with_context(|| format!("failed to write config file {}", path.display()))?;

        Ok(())
    }

    pub fn upsert_profile(&mut self, profile: MachineProfile) {
        if let Some(index) = self
            .profiles
            .iter()
            .position(|item| item.name == profile.name)
        {
            self.profiles[index] = profile;
        } else {
            self.profiles.push(profile);
        }

        self.profiles
            .sort_by_cached_key(|item| item.name.to_lowercase());
    }

    pub fn delete_profile_by_index(&mut self, index: usize) {
        if index < self.profiles.len() {
            self.profiles.remove(index);
        }
    }
}

pub fn default_config_path() -> PathBuf {
    let root = dirs::config_dir()
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))
        .unwrap_or_else(|| PathBuf::from("."));

    root.join("fast-movie-dl").join("profiles.toml")
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn saves_and_loads_profiles() {
        let temp = tempdir().expect("tempdir");
        let config_path = temp.path().join("profiles.toml");

        let mut config = AppConfig::default();
        config.upsert_profile(MachineProfile {
            name: "lab-server".to_string(),
            http_base_url: "https://files.example.com".to_string(),
            ftp_base_url: "ftp://files.example.com".to_string(),
            output_dir: Some(PathBuf::from("/tmp/movies")),
            last_remote_dir: Some("movies/2026".to_string()),
        });

        config
            .save_to_path(&config_path)
            .expect("save config should succeed");

        let loaded = AppConfig::load_from_path(&config_path).expect("load config should succeed");
        assert_eq!(loaded.profiles.len(), 1);
        assert_eq!(loaded.profiles[0].name, "lab-server");
        assert_eq!(
            loaded.profiles[0].last_remote_dir,
            Some("movies/2026".to_string())
        );
    }

    #[test]
    fn upsert_replaces_existing_profile() {
        let mut config = AppConfig::default();
        config.upsert_profile(MachineProfile {
            name: "server".to_string(),
            http_base_url: "https://a.example.com".to_string(),
            ftp_base_url: "ftp://a.example.com".to_string(),
            output_dir: None,
            last_remote_dir: None,
        });

        config.upsert_profile(MachineProfile {
            name: "server".to_string(),
            http_base_url: "https://b.example.com".to_string(),
            ftp_base_url: "ftp://b.example.com".to_string(),
            output_dir: Some(PathBuf::from("/movies")),
            last_remote_dir: Some("movies".to_string()),
        });

        assert_eq!(config.profiles.len(), 1);
        assert_eq!(config.profiles[0].http_base_url, "https://b.example.com");
        assert_eq!(
            config.profiles[0].output_dir,
            Some(PathBuf::from("/movies"))
        );
        assert_eq!(
            config.profiles[0].last_remote_dir,
            Some("movies".to_string())
        );
    }
}
