use crate::{
    manifest::{PluginEntry, PluginManifest},
    plugin::PluginProcess,
};
use anyhow::{Context, Result};
use helix_plugin_sdk::protocol::{HostRequestPayload, PluginResponse};
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::Arc,
};
use tokio::sync::Mutex;
use tower_lsp::{
    jsonrpc::{Error as RpcError, ErrorCode},
    lsp_types::{self as lsp, InitializeParams, InitializeResult},
    Client, LanguageServer,
};

/// Runtime configuration for the plugin host.
#[derive(Debug, Clone)]
pub struct HostOptions(Arc<HostOptionsInner>);

#[derive(Debug)]
struct HostOptionsInner {
    manifest_path: PathBuf,
}

impl HostOptions {
    /// Construct options from CLI arguments.
    pub fn from_cli(manifest: Option<&Path>) -> Result<Self> {
        let manifest_path = match manifest {
            Some(path) => path.to_path_buf(),
            None => helix_loader::config_dir().join("plugins.toml"),
        };

        Ok(Self(Arc::new(HostOptionsInner { manifest_path })))
    }

    /// Absolute path to the manifest file.
    pub fn manifest_path(&self) -> &Path {
        &self.0.manifest_path
    }

    /// Directory containing the manifest file.
    pub fn manifest_dir(&self) -> PathBuf {
        self.0
            .manifest_path
            .parent()
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|| std::env::current_dir().unwrap_or_default())
    }
}

#[derive(Clone)]
struct CommandBinding {
    plugin: PluginProcess,
    #[allow(dead_code)]
    title: String,
    #[allow(dead_code)]
    description: Option<String>,
}

struct PluginManager {
    options: HostOptions,
    plugins: Vec<(String, PluginProcess)>,
    commands: HashMap<String, CommandBinding>,
    initialized: bool,
}

impl PluginManager {
    fn new(options: HostOptions) -> Self {
        Self {
            options,
            plugins: Vec::new(),
            commands: HashMap::new(),
            initialized: false,
        }
    }

    fn manifest_dir(&self) -> PathBuf {
        self.options.manifest_dir()
    }

    async fn ensure_initialized(
        &mut self,
        client: &Client,
        workspace_root: Option<&Path>,
    ) -> Result<()> {
        if self.initialized {
            return Ok(());
        }

        let manifest_path = self.options.manifest_path().to_path_buf();
        let manifest = PluginManifest::load(&manifest_path)?;
        let manifest_dir = self.manifest_dir();

        self.plugins.clear();
        self.commands.clear();

        let workspace_string = workspace_root
            .map(|path| path.to_path_buf())
            .map(|path| path.to_string_lossy().to_string());

        for entry in manifest.plugins {
            match PluginProcess::spawn(&manifest_dir, &entry, client.clone(), workspace_root).await
            {
                Ok(process) => {
                    self.register_plugin(entry, process, workspace_string.clone())
                        .await?;
                }
                Err(err) => {
                    log::error!("failed to start plugin `{}`: {err:?}", entry.name);
                }
            }
        }

        self.initialized = true;
        Ok(())
    }

    async fn register_plugin(
        &mut self,
        entry: PluginEntry,
        process: PluginProcess,
        workspace: Option<String>,
    ) -> Result<()> {
        let response = process
            .send_request(HostRequestPayload::Initialize {
                workspace_root: workspace,
            })
            .await
            .with_context(|| format!("plugin `{}` failed initialization handshake", entry.name))?;

        let PluginResponse::Initialized { commands } = response else {
            log::warn!(
                "plugin `{}` responded with unexpected payload during initialization",
                entry.name
            );
            return Ok(());
        };

        for command in commands {
            let binding = CommandBinding {
                plugin: process.clone(),
                title: command.title.clone(),
                description: command.description.clone(),
            };

            if self.commands.insert(command.id.clone(), binding).is_some() {
                log::warn!(
                    "command `{}` already registered ? overriding with plugin `{}`",
                    command.id,
                    entry.name
                );
            }
        }

        self.plugins.push((entry.name, process));
        Ok(())
    }

    fn command_names(&self) -> Vec<String> {
        self.commands.keys().cloned().collect()
    }

    fn lookup_command(&self, name: &str) -> Option<CommandBinding> {
        self.commands.get(name).cloned()
    }

    async fn shutdown_all(&mut self) {
        for (_, plugin) in &self.plugins {
            if let Err(err) = plugin.shutdown().await {
                log::warn!("failed to gracefully shutdown plugin: {err:?}");
            }
        }
        self.plugins.clear();
        self.commands.clear();
        self.initialized = false;
    }
}

#[derive(Clone)]
pub struct PluginHost {
    client: Client,
    options: HostOptions,
    manager: Arc<Mutex<PluginManager>>,
}

impl PluginHost {
    pub fn new(client: Client, options: HostOptions) -> Self {
        let manager = PluginManager::new(options.clone());
        Self {
            client,
            options,
            manager: Arc::new(Mutex::new(manager)),
        }
    }
}

#[tower_lsp::async_trait]
impl LanguageServer for PluginHost {
    async fn initialize(&self, params: InitializeParams) -> Result<InitializeResult, RpcError> {
        let workspace_root = params
            .root_uri
            .as_ref()
            .and_then(|uri| uri.to_file_path().ok());

        {
            let mut manager = self.manager.lock().await;
            manager
                .ensure_initialized(&self.client, workspace_root.as_deref())
                .await
                .map_err(internal_error)?;
        }

        let command_names = {
            let manager = self.manager.lock().await;
            manager.command_names()
        };

        let capabilities = lsp::ServerCapabilities {
            execute_command_provider: Some(lsp::ExecuteCommandOptions {
                commands: command_names,
                ..Default::default()
            }),
            ..Default::default()
        };

        Ok(InitializeResult {
            capabilities,
            server_info: Some(lsp::ServerInfo {
                name: "helix-plugin-host".into(),
                version: None,
            }),
        })
    }

    async fn initialized(&self, _: lsp::InitializedParams) {
        log::info!(
            "Helix plugin host initialized (manifest: {})",
            self.options.manifest_path().display()
        );
    }

    async fn shutdown(&self) -> Result<(), RpcError> {
        let mut manager = self.manager.lock().await;
        manager.shutdown_all().await;
        Ok(())
    }

    async fn execute_command(
        &self,
        params: lsp::ExecuteCommandParams,
    ) -> Result<Option<serde_json::Value>, RpcError> {
        let lsp::ExecuteCommandParams {
            command, arguments, ..
        } = params;

        let binding = {
            let manager = self.manager.lock().await;
            manager.lookup_command(&command)
        }
        .ok_or_else(|| method_not_found(&command))?;

        let response = binding
            .plugin
            .send_request(HostRequestPayload::Execute {
                command: command.clone(),
                arguments,
            })
            .await
            .map_err(internal_error)?;

        match response {
            PluginResponse::CommandResult { result } => Ok(result),
            PluginResponse::CommandError { message } => Err(internal_error(message)),
            other => Err(internal_error(format!(
                "plugin returned unexpected response for executeCommand: {other:?}"
            ))),
        }
    }
}

fn internal_error(err: impl ToString) -> RpcError {
    RpcError {
        code: ErrorCode::InternalError,
        message: err.to_string().into(),
        data: None,
    }
}

fn method_not_found(command: &str) -> RpcError {
    RpcError {
        code: ErrorCode::MethodNotFound,
        message: format!("command `{command}` not registered").into(),
        data: None,
    }
}
