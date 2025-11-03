mod manifest;
mod plugin;
mod server;

use anyhow::Result;
use clap::Parser;
use server::{HostOptions, PluginHost};
use tower_lsp::{LspService, Server};

/// Command line arguments for the plugin host.
#[derive(Debug, Parser, Clone)]
#[command(
    author,
    version,
    about = "Helix plugin runtime host",
    propagate_version = true
)]
struct Cli {
    /// Path to the plugin manifest. Defaults to the Helix config directory.
    #[arg(long)]
    manifest: Option<std::path::PathBuf>,

    /// Enable verbose logging for the plugin host.
    #[arg(long)]
    verbose: bool,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    if cli.verbose {
        std::env::set_var("RUST_LOG", "info,helix_plugin_host=debug");
    }

    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("warn"))
        .format_timestamp_millis()
        .init();

    let options = HostOptions::from_cli(cli.manifest.as_deref())?;

    let stdin = tokio::io::stdin();
    let stdout = tokio::io::stdout();

    let (service, socket) = LspService::new(|client| PluginHost::new(client, options.clone()));
    Server::new(stdin, stdout, socket).serve(service).await;

    Ok(())
}
