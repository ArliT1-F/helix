use anyhow::{Context, Result};
use serde::Deserialize;
use std::{
    collections::HashMap,
    fs,
    path::{Path, PathBuf},
};

/// Manifest describing available plugins.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PluginManifest {
    /// Declared plugin entries.
    #[serde(default)]
    pub plugins: Vec<PluginEntry>,
}

/// Individual plugin configuration entry.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PluginEntry {
    /// Logical plugin name.
    pub name: String,
    /// Command executed to spawn the plugin.
    pub command: String,
    /// Command line arguments passed to the plugin executable.
    #[serde(default)]
    pub args: Vec<String>,
    /// Environment variables injected when spawning the plugin.
    #[serde(default)]
    pub env: HashMap<String, String>,
    /// Optional working directory (relative to the manifest file if relative).
    #[serde(default)]
    pub cwd: Option<PathBuf>,
}

impl PluginManifest {
    /// Load a manifest from disk. Missing manifests resolve to an empty set.
    pub fn load(path: &Path) -> Result<Self> {
        if !path.exists() {
            log::info!(
                "plugin manifest `{}` not found ? plugin runtime will start without plugins",
                path.display()
            );
            return Ok(Self::default());
        }

        let contents = fs::read_to_string(path)
            .with_context(|| format!("failed to read plugin manifest `{}`", path.display()))?;
        toml::from_str(&contents)
            .with_context(|| format!("failed to parse plugin manifest `{}`", path.display()))
    }
}
