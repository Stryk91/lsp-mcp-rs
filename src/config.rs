use anyhow::{Context, Result};
use serde::Deserialize;
use std::collections::HashMap;
use std::path::Path;

#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    pub servers: HashMap<String, ServerConfig>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ServerConfig {
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    pub extensions: Vec<String>,
    #[serde(default)]
    pub root_patterns: Vec<String>,
    #[serde(default = "default_timeout")]
    pub timeout_ms: u64,
}

fn default_timeout() -> u64 {
    30000
}

impl Config {
    pub fn load<P: AsRef<Path>>(path: P) -> Result<Self> {
        let content = std::fs::read_to_string(path.as_ref())
            .context("Failed to read config file")?;
        toml::from_str(&content).context("Failed to parse config")
    }

    pub fn load_default() -> Result<Self> {
        let exe_dir = std::env::current_exe()
            .ok()
            .and_then(|p| p.parent().map(|p| p.to_path_buf()));

        let paths = [
            exe_dir.as_ref().map(|d| d.join("config.toml")),
            Some(std::path::PathBuf::from("config.toml")),
            dirs::config_dir().map(|d| d.join("lsp-mcp-rs").join("config.toml")),
        ];

        for path in paths.into_iter().flatten() {
            if path.exists() {
                eprintln!("Loading config from: {}", path.display());
                return Self::load(&path);
            }
        }

        anyhow::bail!("No config.toml found")
    }

    pub fn server_for_extension(&self, ext: &str) -> Option<(&str, &ServerConfig)> {
        let ext_lower = ext.to_lowercase();
        for (name, config) in &self.servers {
            if config.extensions.iter().any(|e| e.to_lowercase() == ext_lower) {
                return Some((name, config));
            }
        }
        None
    }
}
