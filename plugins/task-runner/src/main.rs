use anyhow::{anyhow, Context, Result};
use helix_plugin_sdk::{
    run, CommandContext, InitializeContext, MessageLevel, Plugin, PluginCommand, Registrar,
};
use serde::Deserialize;
use serde_json::{json, Value};
use std::{env, fs, path::PathBuf, process::Command};

#[derive(Default)]
struct TaskRunnerPlugin {
    workspace_root: PathBuf,
}

#[derive(Debug, Deserialize)]
struct PackageJson {
    #[serde(default)]
    scripts: serde_json::Map<String, Value>,
}

#[derive(Debug, Clone, serde::Serialize)]
struct Task {
    name: String,
    provider: String,
    command: String,
}

impl TaskRunnerPlugin {
    fn new() -> Result<Self> {
        let root = env::var("HELIX_WORKSPACE_ROOT")
            .map(PathBuf::from)
            .or_else(|_| env::current_dir())?;
        Ok(Self {
            workspace_root: root,
        })
    }

    fn discover_tasks(&self) -> Result<Vec<Task>> {
        let mut tasks = Vec::new();

        tasks.extend(self.extract_package_scripts()?);
        tasks.extend(self.extract_justfile()?);
        tasks.extend(self.extract_makefile()?);

        Ok(tasks)
    }

    fn extract_package_scripts(&self) -> Result<Vec<Task>> {
        let package_json = self.workspace_root.join("package.json");
        if !package_json.exists() {
            return Ok(Vec::new());
        }

        let content = fs::read_to_string(&package_json)
            .with_context(|| format!("failed to read {}", package_json.display()))?;
        let parsed: PackageJson = serde_json::from_str(&content)
            .with_context(|| format!("failed to parse {}", package_json.display()))?;

        Ok(parsed
            .scripts
            .iter()
            .map(|(name, value)| Task {
                name: name.clone(),
                provider: "npm".to_string(),
                command: value.as_str().unwrap_or_default().to_string(),
            })
            .collect())
    }

    fn extract_justfile(&self) -> Result<Vec<Task>> {
        let justfile = self.workspace_root.join("justfile");
        if !justfile.exists() {
            return Ok(Vec::new());
        }

        let content = fs::read_to_string(&justfile)
            .with_context(|| format!("failed to read {}", justfile.display()))?;

        let tasks = content
            .lines()
            .filter_map(|line| {
                let line = line.trim();
                if line.is_empty() || line.starts_with('#') {
                    return None;
                }
                line.split_once(':').map(|(name, _)| Task {
                    name: name.trim().to_string(),
                    provider: "just".to_string(),
                    command: String::new(),
                })
            })
            .collect();

        Ok(tasks)
    }

    fn extract_makefile(&self) -> Result<Vec<Task>> {
        let makefile = self.workspace_root.join("Makefile");
        if !makefile.exists() {
            return Ok(Vec::new());
        }

        let content = fs::read_to_string(&makefile)
            .with_context(|| format!("failed to read {}", makefile.display()))?;

        let tasks = content
            .lines()
            .filter_map(|line| {
                let trimmed = line.trim();
                if trimmed.is_empty() || trimmed.starts_with('#') || trimmed.starts_with('.') {
                    return None;
                }
                trimmed.split_once(':').map(|(name, _)| Task {
                    name: name.trim().to_string(),
                    provider: "make".to_string(),
                    command: String::new(),
                })
            })
            .collect();

        Ok(tasks)
    }

    fn run_task(&self, provider: &str, name: &str) -> Result<String> {
        match provider {
            "npm" | "yarn" | "pnpm" => self.run_package_script(provider, name),
            "just" => self.exec_process("just", &[name]),
            "make" => self.exec_process("make", &[name]),
            other => Err(anyhow!("task provider `{other}` is not supported")),
        }
    }

    fn run_package_script(&self, provider: &str, script: &str) -> Result<String> {
        let (cmd, args) = match provider {
            "npm" => ("npm", vec!["run", script]),
            "yarn" => ("yarn", vec![script]),
            "pnpm" => ("pnpm", vec!["run", script]),
            other => (other, vec![script]),
        };
        self.exec_process(cmd, &args)
    }

    fn exec_process(&self, binary: &str, args: &[&str]) -> Result<String> {
        let output = Command::new(binary)
            .args(args)
            .current_dir(&self.workspace_root)
            .output()
            .with_context(|| format!("failed to spawn `{binary}`"))?;

        if output.status.success() {
            let stdout = String::from_utf8_lossy(&output.stdout).to_string();
            Ok(stdout)
        } else {
            let stderr = String::from_utf8_lossy(&output.stderr);
            Err(anyhow!("task failed: {stderr}"))
        }
    }
}

impl Plugin for TaskRunnerPlugin {
    fn name(&self) -> &'static str {
        "task-runner"
    }

    fn initialize(
        &mut self,
        ctx: &mut InitializeContext,
        registrar: &mut dyn Registrar,
    ) -> Result<()> {
        registrar.register_command(
            PluginCommand::new("helix.task.list", "List project tasks")
                .with_description("Enumerate runnable tasks discovered in the current workspace"),
        )?;
        registrar.register_command(
            PluginCommand::new("helix.task.run", "Run project task")
                .with_description("Execute a task by provider and name"),
        )?;

        if !self.workspace_root.exists() {
            ctx.log(
                MessageLevel::Warning,
                "Task runner could not determine the workspace root.",
            )?;
        }

        Ok(())
    }

    fn execute(
        &mut self,
        command: &str,
        arguments: Vec<Value>,
        ctx: &mut CommandContext<'_>,
    ) -> Result<Option<Value>> {
        match command {
            "helix.task.list" => {
                let tasks = self.discover_tasks()?;
                let response = serde_json::to_value(tasks)?;
                Ok(Some(response))
            }
            "helix.task.run" => {
                if arguments.is_empty() {
                    return Err(anyhow!(
                        "expected arguments {{ provider: string, name: string }}"
                    ));
                }

                let payload = &arguments[0];
                let provider = payload
                    .get("provider")
                    .and_then(Value::as_str)
                    .ok_or_else(|| anyhow!("missing `provider` field"))?;
                let name = payload
                    .get("name")
                    .and_then(Value::as_str)
                    .ok_or_else(|| anyhow!("missing `name` field"))?;

                match self.run_task(provider, name) {
                    Ok(stdout) => {
                        ctx.show_message(
                            MessageLevel::Info,
                            format!("task `{provider}:{name}` completed"),
                        )?;
                        Ok(Some(json!({ "stdout": stdout })))
                    }
                    Err(err) => {
                        ctx.show_message(
                            MessageLevel::Error,
                            format!("task `{provider}:{name}` failed: {err}"),
                        )?;
                        Err(err)
                    }
                }
            }
            _ => Err(anyhow!("unknown command `{command}`")),
        }
    }
}

fn main() -> Result<()> {
    run(TaskRunnerPlugin::new()?)
}
