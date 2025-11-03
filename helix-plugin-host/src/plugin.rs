use crate::manifest::PluginEntry;
use anyhow::{anyhow, Context, Result};
use helix_plugin_sdk::protocol::{
    HostRequest, HostRequestPayload, MessageLevel, PluginEvent, PluginMessage, PluginResponse,
};
use std::{
    collections::HashMap,
    ffi::OsString,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    sync::Arc,
};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    process::{Child, ChildStderr, ChildStdout, Command},
    sync::{oneshot, Mutex},
};
use tower_lsp::Client;

/// Handle to a spawned plugin process.
#[derive(Clone)]
pub struct PluginProcess {
    inner: Arc<PluginProcessInner>,
}

struct PluginProcessInner {
    name: String,
    display_command: String,
    writer: Mutex<tokio::process::ChildStdin>,
    pending: Mutex<HashMap<u64, oneshot::Sender<PluginResponse>>>,
    next_request_id: AtomicU64,
    client: Client,
    child: Mutex<Option<Child>>,
}

impl PluginProcess {
    /// Spawn a new plugin process from the provided manifest entry.
    pub async fn spawn(
        manifest_dir: &Path,
        entry: &PluginEntry,
        client: Client,
        workspace_root: Option<&Path>,
    ) -> Result<Self> {
        let (cmd, display) = resolve_command(manifest_dir, &entry.command);

        let mut command = Command::new(&cmd);
        command.kill_on_drop(true);
        command.stdin(std::process::Stdio::piped());
        command.stdout(std::process::Stdio::piped());
        command.stderr(std::process::Stdio::piped());
        command.args(&entry.args);

        if let Some(cwd) = entry.cwd.as_ref() {
            command.current_dir(resolve_relative(manifest_dir, cwd));
        }

        for (key, value) in &entry.env {
            command.env(key, value);
        }

        command.env("HELIX_PLUGIN_NAME", &entry.name);
        if let Some(root) = workspace_root {
            command.env("HELIX_WORKSPACE_ROOT", root);
        }

        let mut child = command
            .spawn()
            .with_context(|| format!("failed to spawn plugin `{}`", entry.name))?;

        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| anyhow!("plugin `{}` stdin unavailable", entry.name))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| anyhow!("plugin `{}` stdout unavailable", entry.name))?;
        let stderr = child.stderr.take();

        let process = Self {
            inner: Arc::new(PluginProcessInner {
                name: entry.name.clone(),
                display_command: display,
                writer: Mutex::new(stdin),
                pending: Mutex::new(HashMap::new()),
                next_request_id: AtomicU64::new(1),
                client,
                child: Mutex::new(Some(child)),
            }),
        };

        process.spawn_stdout_task(stdout);
        if let Some(stderr) = stderr {
            process.spawn_stderr_task(stderr);
        }

        log::info!(
            "spawned plugin `{}` using command `{}`",
            entry.name,
            process.inner.display_command
        );

        Ok(process)
    }

    /// Send a request to the plugin and await the response.
    pub async fn send_request(&self, payload: HostRequestPayload) -> Result<PluginResponse> {
        let id = self.inner.next_request_id.fetch_add(1, Ordering::Relaxed);
        let request = HostRequest { id, payload };

        let (tx, rx) = oneshot::channel();
        {
            let mut pending = self.inner.pending.lock().await;
            pending.insert(id, tx);
        }

        let mut writer = self.inner.writer.lock().await;
        let serialized =
            serde_json::to_vec(&request).context("failed to serialize plugin request payload")?;
        writer
            .write_all(&serialized)
            .await
            .context("failed to write plugin request")?;
        writer
            .write_all(b"\n")
            .await
            .context("failed to delimit plugin request")?;
        writer
            .flush()
            .await
            .context("failed to flush plugin request")?;

        match rx.await {
            Ok(response) => Ok(response),
            Err(_) => Err(anyhow!(
                "plugin `{}` terminated before responding",
                self.inner.name
            )),
        }
    }

    /// Issue a shutdown request to the plugin and wait for process termination.
    pub async fn shutdown(&self) -> Result<()> {
        let _ = self.send_request(HostRequestPayload::Shutdown).await;
        let mut child = self.inner.child.lock().await;
        if let Some(mut child) = child.take() {
            let _ = child.wait().await;
        }
        Ok(())
    }

    fn spawn_stdout_task(&self, stdout: ChildStdout) {
        let inner = Arc::clone(&self.inner);
        let mut reader = BufReader::new(stdout).lines();

        tokio::spawn(async move {
            while let Ok(Some(line)) = reader.next_line().await {
                match serde_json::from_str::<PluginMessage>(&line) {
                    Ok(PluginMessage::Response { id, result }) => {
                        let sender = inner.pending.lock().await.remove(&id);
                        if let Some(sender) = sender {
                            let _ = sender.send(result);
                        } else {
                            log::warn!(
                                "plugin `{}` produced response for unknown request id {id}",
                                inner.name
                            );
                        }
                    }
                    Ok(PluginMessage::Event { event }) => {
                        handle_event(&inner, event).await;
                    }
                    Err(err) => {
                        log::warn!(
                            "failed to decode plugin `{}` message: {err}: {line}",
                            inner.name
                        );
                    }
                }
            }

            drain_pending_with_failure(&inner, "plugin stdout closed").await;
        });
    }

    fn spawn_stderr_task(&self, stderr: ChildStderr) {
        let name = self.inner.name.clone();
        let mut reader = BufReader::new(stderr).lines();
        tokio::spawn(async move {
            while let Ok(Some(line)) = reader.next_line().await {
                log::warn!("plugin `{name}` stderr: {line}");
            }
        });
    }
}

impl Drop for PluginProcessInner {
    fn drop(&mut self) {
        if let Ok(mut child) = self.child.try_lock() {
            if let Some(mut child) = child.take() {
                if let Err(err) = child.start_kill() {
                    log::warn!(
                        "failed to terminate plugin `{}` during drop: {err}",
                        self.name
                    );
                }
            }
        }
    }
}

async fn handle_event(inner: &PluginProcessInner, event: PluginEvent) {
    match event {
        PluginEvent::ShowMessage { level, message } => {
            let ty = map_message_level(level);
            inner.client.show_message(ty, message.clone()).await;
        }
        PluginEvent::Log { level, message } => {
            let ty = map_message_level(level);
            inner.client.log_message(ty, message.clone()).await;
        }
    }
}

async fn drain_pending_with_failure(inner: &PluginProcessInner, message: &str) {
    let mut pending = inner.pending.lock().await;
    if pending.is_empty() {
        return;
    }

    log::warn!("plugin `{}` disconnected: {message}", inner.name);

    for (_, sender) in pending.drain() {
        let _ = sender.send(PluginResponse::CommandError {
            message: format!("plugin `{}` disconnected", inner.name),
        });
    }
}

fn resolve_command(manifest_dir: &Path, command: &str) -> (OsString, String) {
    let path = Path::new(command);
    if path.is_absolute() {
        (path.as_os_str().to_owned(), command.to_string())
    } else if path.components().count() > 1 || command.starts_with('.') {
        let resolved = manifest_dir.join(path);
        let display = resolved.display().to_string();
        (resolved.into_os_string(), display)
    } else {
        (OsString::from(command), command.to_string())
    }
}

fn resolve_relative(base: &Path, path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        base.join(path)
    }
}

fn map_message_level(level: MessageLevel) -> tower_lsp::lsp_types::MessageType {
    use tower_lsp::lsp_types::MessageType;
    match level {
        MessageLevel::Error => MessageType::ERROR,
        MessageLevel::Warning => MessageType::WARNING,
        MessageLevel::Info => MessageType::INFO,
        MessageLevel::Log => MessageType::LOG,
    }
}
