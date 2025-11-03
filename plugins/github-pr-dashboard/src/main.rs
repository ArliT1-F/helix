use anyhow::{anyhow, Result};
use helix_plugin_sdk::{
    run, CommandContext, InitializeContext, MessageLevel, Plugin, PluginCommand, Registrar,
};
use reqwest::blocking::Client;
use serde::Deserialize;
use serde_json::Value;
use std::{env, process::Command, time::Duration};
use thiserror::Error;
use url::Url;

#[derive(Default)]
struct GithubPrPlugin {
    repo: Option<Repository>,
    token: Option<String>,
    client: Client,
}

#[derive(Debug, Clone)]
struct Repository {
    owner: String,
    name: String,
}

#[derive(Debug, Deserialize)]
struct PullRequest {
    number: u64,
    title: String,
    html_url: String,
    state: String,
    #[serde(default)]
    draft: bool,
    #[serde(default)]
    user: Option<User>,
    #[serde(default)]
    mergeable_state: Option<String>,
}

#[derive(Debug, Deserialize)]
struct User {
    login: String,
}

#[derive(Debug, Error)]
enum PluginError {
    #[error(
        "GitHub repository could not be detected. Run `git remote -v` to ensure `origin` is set."
    )]
    MissingRepository,
    #[error("GitHub API responded with status {0}")]
    ApiStatus(reqwest::StatusCode),
}

impl GithubPrPlugin {
    fn new() -> Result<Self> {
        let token = env::var("GITHUB_TOKEN").ok();
        let client = Client::builder()
            .user_agent("helix-plugin-github-pr-dashboard/0.1")
            .timeout(Duration::from_secs(10))
            .build()?;

        Ok(Self {
            repo: detect_repository()?,
            token,
            client,
        })
    }

    fn list_pull_requests(&self) -> Result<Vec<PullRequest>> {
        let repo = self.repo.clone().ok_or(PluginError::MissingRepository)?;

        let url = format!(
            "https://api.github.com/repos/{owner}/{repo}/pulls",
            owner = repo.owner,
            repo = repo.name
        );

        let mut request = self.client.get(url);
        if let Some(token) = &self.token {
            request = request.bearer_auth(token);
        }

        let response = request.send()?;
        if !response.status().is_success() {
            return Err(PluginError::ApiStatus(response.status()).into());
        }

        let prs: Vec<PullRequest> = response.json()?;
        Ok(prs)
    }
}

impl Plugin for GithubPrPlugin {
    fn name(&self) -> &'static str {
        "github-pr-dashboard"
    }

    fn initialize(
        &mut self,
        ctx: &mut InitializeContext,
        registrar: &mut dyn Registrar,
    ) -> Result<()> {
        registrar.register_command(
            PluginCommand::new("helix.github.list_prs", "List GitHub pull requests")
                .with_description("Fetch open pull requests for the current repository"),
        )?;

        if self.repo.is_none() {
            ctx.log(
                MessageLevel::Warning,
                "GitHub PR dashboard could not detect the repository. Commands will fail until a git remote is configured.",
            )?;
        }

        Ok(())
    }

    fn execute(
        &mut self,
        command: &str,
        _arguments: Vec<Value>,
        _ctx: &mut CommandContext<'_>,
    ) -> Result<Option<Value>> {
        match command {
            "helix.github.list_prs" => {
                let prs = self.list_pull_requests()?;
                let result = serde_json::to_value(
                    prs.iter()
                        .map(|pr| {
                            serde_json::json!({
                                "number": pr.number,
                                "title": pr.title,
                                "url": pr.html_url,
                                "state": pr.state,
                                "draft": pr.draft,
                                "author": pr.user.as_ref().map(|user| user.login.clone()),
                                "mergeable_state": pr.mergeable_state,
                            })
                        })
                        .collect::<Vec<_>>(),
                )?;
                Ok(Some(result))
            }
            _ => Err(anyhow!("unknown command `{command}`")),
        }
    }
}

fn detect_repository() -> Result<Option<Repository>> {
    let workspace = env::var("HELIX_WORKSPACE_ROOT").ok();
    let repo_root = workspace
        .map(|path| path.into())
        .unwrap_or_else(|| env::current_dir().unwrap_or_default());

    let output = Command::new("git")
        .arg("-C")
        .arg(&repo_root)
        .arg("config")
        .arg("--get")
        .arg("remote.origin.url")
        .output();

    let url = match output {
        Ok(output) if output.status.success() => {
            let remote = String::from_utf8_lossy(&output.stdout).trim().to_string();
            if remote.is_empty() {
                return Ok(None);
            }
            remote
        }
        _ => return Ok(None),
    };

    parse_remote(&url).map(Some)
}

fn parse_remote(remote: &str) -> Result<Repository> {
    if remote.starts_with("git@") {
        // git@github.com:owner/repo.git
        let parts: Vec<&str> = remote.split(':').collect();
        if parts.len() != 2 {
            return Err(anyhow!("unable to parse git remote `{remote}`"));
        }
        let path = parts[1].trim_end_matches(".git");
        let (owner, name) = path
            .split_once('/')
            .ok_or_else(|| anyhow!("unable to parse git remote `{remote}`"))?;
        return Ok(Repository {
            owner: owner.to_string(),
            name: name.to_string(),
        });
    }

    let url = Url::parse(remote).map_err(|_| anyhow!("unable to parse git remote `{remote}`"))?;
    let mut segments = url
        .path_segments()
        .ok_or_else(|| anyhow!("git remote `{remote}` does not contain path segments"))?;

    let owner = segments
        .next()
        .ok_or_else(|| anyhow!("git remote `{remote}` missing owner"))?
        .to_string();
    let repo = segments
        .next()
        .ok_or_else(|| anyhow!("git remote `{remote}` missing repository"))?
        .trim_end_matches(".git")
        .to_string();

    Ok(Repository { owner, name: repo })
}

fn main() -> Result<()> {
    run(GithubPrPlugin::new()?)
}
