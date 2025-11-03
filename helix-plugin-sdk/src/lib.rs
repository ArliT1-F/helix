#![deny(missing_docs)]
#![deny(rust_2018_idioms)]

//! Helix plugin SDK shared between the plugin host and plugin implementations.
//!
//! This crate provides the JSON protocol definitions exchanged between the
//! plugin host and plugin processes as well as a small runtime that plugin
//! authors can embed in their binaries.

pub mod protocol {
    //! Shared protocol definitions between the host and plugin processes.

    use serde::{Deserialize, Serialize};
    use serde_json::Value;

    /// A command exported by a plugin.
    #[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
    pub struct PluginCommand {
        /// Fully qualified command identifier.
        pub id: String,
        /// Human readable title surfaced in Helix.
        pub title: String,
        /// Optional command description shown in UIs.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub description: Option<String>,
    }

    impl PluginCommand {
        /// Create a new command declaration.
        pub fn new(id: impl Into<String>, title: impl Into<String>) -> Self {
            Self {
                id: id.into(),
                title: title.into(),
                description: None,
            }
        }

        /// Attach a description to the command declaration.
        pub fn with_description(mut self, description: impl Into<String>) -> Self {
            self.description = Some(description.into());
            self
        }
    }

    /// Severity levels understood by the host for logging and UI messages.
    #[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
    #[serde(rename_all = "snake_case")]
    pub enum MessageLevel {
        /// Error severity.
        Error,
        /// Warning severity.
        Warning,
        /// Informational severity.
        Info,
        /// Verbose log message.
        Log,
    }

    /// Request message sent from the host to a plugin.
    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub struct HostRequest {
        /// Unique request identifier.
        pub id: u64,
        /// Payload of the request.
        pub payload: HostRequestPayload,
    }

    /// Host -> plugin request payload variants.
    #[derive(Debug, Clone, Serialize, Deserialize)]
    #[serde(tag = "type", rename_all = "snake_case")]
    pub enum HostRequestPayload {
        /// Initial handshake providing workspace information.
        Initialize {
            /// Optional workspace root resolved by the host.
            workspace_root: Option<String>,
        },
        /// Execute a previously registered command.
        Execute {
            /// Command identifier.
            command: String,
            /// Command arguments forwarded from Helix.
            #[serde(default)]
            arguments: Vec<Value>,
        },
        /// Terminate the plugin process gracefully.
        Shutdown,
    }

    /// Message emitted by the plugin process towards the host.
    #[derive(Debug, Clone, Serialize, Deserialize)]
    #[serde(tag = "type", rename_all = "snake_case")]
    pub enum PluginMessage {
        /// Response to a host request.
        Response {
            /// Correlates with the originating host request.
            id: u64,
            /// Outcome of the request.
            result: PluginResponse,
        },
        /// Asynchronous event notification.
        Event {
            /// Event payload.
            event: PluginEvent,
        },
    }

    /// Response kinds emitted by a plugin.
    #[derive(Debug, Clone, Serialize, Deserialize)]
    #[serde(tag = "type", rename_all = "snake_case")]
    pub enum PluginResponse {
        /// Successful initialization containing command metadata.
        Initialized {
            /// Commands exposed by the plugin.
            commands: Vec<PluginCommand>,
        },
        /// Command executed successfully.
        CommandResult {
            /// Optional command return value.
            #[serde(default)]
            result: Option<Value>,
        },
        /// Command execution failed with an error message.
        CommandError {
            /// Human readable error string.
            message: String,
        },
        /// Acknowledge completion (used for shutdown, etc.).
        Acknowledge,
    }

    /// Out-of-band events emitted by a plugin.
    #[derive(Debug, Clone, Serialize, Deserialize)]
    #[serde(tag = "type", rename_all = "snake_case")]
    pub enum PluginEvent {
        /// Request the host to surface a status / error message to the user.
        ShowMessage {
            /// Message severity.
            level: MessageLevel,
            /// Message contents.
            message: String,
        },
        /// Emit a log message that should end up in Helix logs.
        Log {
            /// Log severity.
            level: MessageLevel,
            /// Log message.
            message: String,
        },
    }
}

pub mod runtime {
    //! Minimal runtime for authoring Helix plugins.

    use anyhow::{anyhow, Context, Result};
    use log::{debug, error, trace};
    use serde_json::Value;
    use std::{
        collections::HashSet,
        io::{self, BufRead, Write},
        path::{Path, PathBuf},
        sync::{Arc, Mutex},
    };

    use crate::protocol::{
        HostRequest, HostRequestPayload, MessageLevel, PluginCommand, PluginEvent, PluginMessage,
        PluginResponse,
    };

    /// Plugins implement this trait to participate in the runtime.
    pub trait Plugin: Send {
        /// Name of the plugin used for diagnostics.
        fn name(&self) -> &'static str;

        /// Called once when the host sends the initialization message.
        fn initialize(
            &mut self,
            ctx: &mut InitializeContext,
            registrar: &mut dyn Registrar,
        ) -> Result<()>;

        /// Execute a registered command.
        fn execute(
            &mut self,
            command: &str,
            arguments: Vec<Value>,
            ctx: &mut CommandContext<'_>,
        ) -> Result<Option<Value>>;
    }

    /// Registrar passed to [`Plugin::initialize`] allowing command registration.
    pub trait Registrar {
        /// Register a command with the runtime.
        fn register_command(&mut self, command: PluginCommand) -> Result<()>;
    }

    #[derive(Default)]
    struct CommandRegistry {
        commands: Vec<PluginCommand>,
        seen: HashSet<String>,
    }

    impl Registrar for CommandRegistry {
        fn register_command(&mut self, command: PluginCommand) -> Result<()> {
            if !self.seen.insert(command.id.clone()) {
                return Err(anyhow!(
                    "command `{}` registered multiple times",
                    command.id
                ));
            }
            self.commands.push(command);
            Ok(())
        }
    }

    /// Connection handle for emitting events back to the host.
    #[derive(Clone)]
    struct HostConnection {
        writer: Arc<Mutex<io::Stdout>>,
    }

    impl HostConnection {
        fn send_message(&self, message: &PluginMessage) -> Result<()> {
            let mut writer = self
                .writer
                .lock()
                .map_err(|_| anyhow!("failed to lock stdout for writing"))?;
            serde_json::to_writer(&mut *writer, message)
                .context("failed to serialize plugin protocol message")?;
            writer
                .write_all(b"\n")
                .context("failed to write message delimiter")?;
            writer.flush().context("failed to flush plugin message")?;

            Ok(())
        }
    }

    /// Context available during plugin initialization.
    pub struct InitializeContext {
        connection: HostConnection,
        workspace_root: Option<PathBuf>,
    }

    impl InitializeContext {
        fn new(connection: HostConnection, workspace_root: Option<PathBuf>) -> Self {
            Self {
                connection,
                workspace_root,
            }
        }

        /// Returns the workspace root provided by the host, if available.
        pub fn workspace_root(&self) -> Option<&Path> {
            self.workspace_root.as_deref()
        }

        /// Emit a user facing message through the host.
        pub fn show_message(&self, level: MessageLevel, message: impl Into<String>) -> Result<()> {
            self.connection.send_message(&PluginMessage::Event {
                event: PluginEvent::ShowMessage {
                    level,
                    message: message.into(),
                },
            })
        }

        /// Emit a log message routed to the host logs.
        pub fn log(&self, level: MessageLevel, message: impl Into<String>) -> Result<()> {
            self.connection.send_message(&PluginMessage::Event {
                event: PluginEvent::Log {
                    level,
                    message: message.into(),
                },
            })
        }
    }

    /// Execution context made available to command handlers.
    pub struct CommandContext<'a> {
        connection: &'a HostConnection,
        plugin_name: &'a str,
    }

    impl<'a> CommandContext<'a> {
        fn new(connection: &'a HostConnection, plugin_name: &'a str) -> Self {
            Self {
                connection,
                plugin_name,
            }
        }

        /// Emit a user facing message via the host.
        pub fn show_message(&self, level: MessageLevel, message: impl Into<String>) -> Result<()> {
            trace!("{}: show_message({level:?})", self.plugin_name);
            self.connection.send_message(&PluginMessage::Event {
                event: PluginEvent::ShowMessage {
                    level,
                    message: message.into(),
                },
            })
        }

        /// Emit a log message.
        pub fn log(&self, level: MessageLevel, message: impl Into<String>) -> Result<()> {
            trace!("{}: log({level:?})", self.plugin_name);
            self.connection.send_message(&PluginMessage::Event {
                event: PluginEvent::Log {
                    level,
                    message: message.into(),
                },
            })
        }
    }

    /// Run the plugin event loop.
    pub fn run<P: Plugin>(mut plugin: P) -> Result<()> {
        let stdout = io::stdout();
        let connection = HostConnection {
            writer: Arc::new(Mutex::new(stdout)),
        };

        let stdin = io::stdin();
        let reader = io::BufReader::new(stdin.lock());

        let mut initialized = false;
        let mut registry = CommandRegistry::default();

        for line in reader.lines() {
            let line = line.context("failed to read plugin request")?;
            if line.trim().is_empty() {
                continue;
            }

            let request: HostRequest =
                serde_json::from_str(&line).context("failed to parse plugin request payload")?;

            trace!("plugin received request: {:?}", request.payload);

            match request.payload {
                HostRequestPayload::Initialize { workspace_root } => {
                    if initialized {
                        error!("plugin received duplicate initialize request");
                        connection.send_message(&PluginMessage::Response {
                            id: request.id,
                            result: PluginResponse::CommandError {
                                message: "plugin already initialized".to_string(),
                            },
                        })?;
                        continue;
                    }

                    let workspace_root = workspace_root.map(PathBuf::from);
                    let mut init_ctx = InitializeContext::new(connection.clone(), workspace_root);
                    plugin
                        .initialize(&mut init_ctx, &mut registry)
                        .with_context(|| format!("{} failed to initialize", plugin.name()))?;

                    connection.send_message(&PluginMessage::Response {
                        id: request.id,
                        result: PluginResponse::Initialized {
                            commands: registry.commands.clone(),
                        },
                    })?;
                    initialized = true;
                }
                HostRequestPayload::Execute { command, arguments } => {
                    if !initialized {
                        error!("plugin received execute before initialize");
                        connection.send_message(&PluginMessage::Response {
                            id: request.id,
                            result: PluginResponse::CommandError {
                                message: "plugin not initialized".to_string(),
                            },
                        })?;
                        continue;
                    }

                    let mut ctx = CommandContext::new(&connection, plugin.name());

                    match plugin.execute(&command, arguments, &mut ctx) {
                        Ok(result) => {
                            connection.send_message(&PluginMessage::Response {
                                id: request.id,
                                result: PluginResponse::CommandResult { result },
                            })?;
                        }
                        Err(err) => {
                            error!("{} command `{command}` failed: {err:?}", plugin.name());
                            connection.send_message(&PluginMessage::Response {
                                id: request.id,
                                result: PluginResponse::CommandError {
                                    message: err.to_string(),
                                },
                            })?;
                        }
                    }
                }
                HostRequestPayload::Shutdown => {
                    debug!("{} shutting down", plugin.name());
                    connection.send_message(&PluginMessage::Response {
                        id: request.id,
                        result: PluginResponse::Acknowledge,
                    })?;
                    break;
                }
            }
        }

        Ok(())
    }
}

pub use protocol::{MessageLevel, PluginCommand};
pub use runtime::{run, CommandContext, InitializeContext, Plugin, Registrar};
