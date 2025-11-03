use anyhow::{anyhow, Result};
use helix_plugin_sdk::{
    run, CommandContext, InitializeContext, MessageLevel, Plugin, PluginCommand, Registrar,
};
use serde_json::Value;

struct HelloPlugin;

impl Plugin for HelloPlugin {
    fn name(&self) -> &'static str {
        "hello-plugin"
    }

    fn initialize(
        &mut self,
        ctx: &mut InitializeContext,
        registrar: &mut dyn Registrar,
    ) -> Result<()> {
        registrar.register_command(
            PluginCommand::new("helix.hello.say_hello", "Say Hello")
                .with_description("Display a friendly greeting."),
        )?;

        if let Some(root) = ctx.workspace_root() {
            let message = format!("Hello plugin loaded for workspace: {}", root.display());
            ctx.log(MessageLevel::Info, message)?;
        }

        Ok(())
    }

    fn execute(
        &mut self,
        command: &str,
        _: Vec<Value>,
        ctx: &mut CommandContext<'_>,
    ) -> Result<Option<Value>> {
        match command {
            "helix.hello.say_hello" => {
                ctx.show_message(MessageLevel::Info, "Hello from the Helix plugin runtime!")?;
                Ok(None)
            }
            other => Err(anyhow!("unknown command `{other}`")),
        }
    }
}

fn main() -> Result<()> {
    run(HelloPlugin)
}
