use anyhow::{Context as _, Result, bail};
use askpass::AskPassDelegate;
use browser_tab::BrowserTab;
use editor::Editor;
use git::repository::{
    CommitOptions, DiffType, FetchOptions, PushOptions, Remote, RepoPath, UpstreamTracking,
};
use gpui::{App, AsyncApp, Entity, Task, WindowHandle};
use project::git_store::Repository;
use serde::Deserialize;
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use task::{RevealStrategy, SpawnInTerminal, TaskId};
use text::Point;
use workspace::{MultiWorkspace, OpenOptions, OpenVisible, Workspace};

const REPOSITORY_DESCRIPTION: &str = "Absolute path of the repository to act on, or of anything inside it. Pass the project you are working in. Defaults to the repository shown in OTerminal's git panel.";

pub(crate) fn definitions() -> Value {
    let repository = json!({ "type": "string", "description": REPOSITORY_DESCRIPTION });
    json!([
        {
            "name": "git.status",
            "description": "Summarize the repository state: branch, ahead/behind counts, and staged / unstaged / untracked / conflicted files.",
            "inputSchema": {
                "type": "object",
                "properties": { "repository": repository },
                "additionalProperties": false,
            },
        },
        {
            "name": "git.stage",
            "description": "Stage files (git add), as OTerminal's git panel does. Paths may be repo-relative or absolute. Pass {\"all\": true} to stage every change.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "repository": repository,
                    "paths": { "type": "array", "items": { "type": "string" }, "description": "Paths to stage. Required unless \"all\" is true." },
                    "all": { "type": "boolean", "description": "Stage every change, untracked files included." },
                },
                "additionalProperties": false,
            },
        },
        {
            "name": "git.unstage",
            "description": "Unstage files (git reset HEAD -- <paths>). Paths may be repo-relative or absolute. Pass {\"all\": true} to unstage everything.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "repository": repository,
                    "paths": { "type": "array", "items": { "type": "string" } },
                    "all": { "type": "boolean" },
                },
                "additionalProperties": false,
            },
        },
        {
            "name": "git.commit",
            "description": "Commit the staged changes as the configured local git user. AI attribution lines (Co-Authored-By: Claude, GitHub Copilot, \"Generated with Claude Code\", etc.) are stripped from the message before committing.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "repository": repository,
                    "message": { "type": "string", "description": "Commit message." },
                    "amend": { "type": "boolean", "description": "Amend the previous commit. Default false." },
                    "signoff": { "type": "boolean" },
                },
                "required": ["message"],
                "additionalProperties": false,
            },
        },
        {
            "name": "git.diff",
            "description": "Return the unified diff of every uncommitted change against HEAD (default), or of the staged changes only (cached: true).",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "repository": repository,
                    "cached": { "type": "boolean", "description": "Diff the staged index against HEAD. Default false." },
                },
                "additionalProperties": false,
            },
        },
        {
            "name": "git.branch",
            "description": "List branches (local by default) and return the currently checked-out branch.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "repository": repository,
                    "remote": { "type": "boolean", "description": "Include remote-tracking branches. Default false." },
                },
                "additionalProperties": false,
            },
        },
        {
            "name": "git.checkout",
            "description": "Switch to an existing branch. Pass {\"createBranch\": \"feature/x\"} to create and switch to a new branch from the current HEAD (or from \"ref\").",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "repository": repository,
                    "branch": { "type": "string", "description": "Branch to switch to. Required unless createBranch is set." },
                    "createBranch": { "type": "string", "description": "Create a new branch with this name and switch to it." },
                    "ref": { "type": "string", "description": "Starting point for createBranch. Defaults to the current HEAD." },
                },
                "additionalProperties": false,
            },
        },
        {
            "name": "git.fetch",
            "description": "Fetch from a remote (default: all remotes) so ahead/behind counts are up to date.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "repository": repository,
                    "remote": { "type": "string", "description": "Remote to fetch from. Defaults to all remotes." },
                },
                "additionalProperties": false,
            },
        },
        {
            "name": "git.pull",
            "description": "Pull the current branch from its upstream, using the git credentials OTerminal's git panel uses.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "repository": repository,
                    "rebase": { "type": "boolean", "description": "Rebase instead of merging. Default false." },
                },
                "additionalProperties": false,
            },
        },
        {
            "name": "git.push",
            "description": "Push the current branch, using the GitHub account signed in to OTerminal. A branch without an upstream is published and its upstream is set. Plain force pushes are not offered; use forceWithLease to overwrite a remote branch you rewrote.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "repository": repository,
                    "remote": { "type": "string", "description": "Remote to push to. Defaults to the branch's remote, else \"origin\", else the only remote." },
                    "forceWithLease": { "type": "boolean", "description": "Push with --force-with-lease. Default false." },
                },
                "additionalProperties": false,
            },
        },
        {
            "name": "editor.openFile",
            "description": "Open a file in an OTerminal editor tab and optionally select a line range, to show the user something.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "Absolute path, or a path relative to the window's first folder." },
                    "startLine": { "type": "number", "description": "1-based line to go to." },
                    "endLine": { "type": "number", "description": "1-based end line of the selection." },
                },
                "required": ["path"],
                "additionalProperties": false,
            },
        },
        {
            "name": "browser.open",
            "description": "Open a URL in an OTerminal browser tab (default) or in the OS default browser. Use this to show the user a live preview, docs, or a localhost dev server.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "url": { "type": "string", "description": "Absolute http or https URL." },
                    "target": { "type": "string", "enum": ["tab", "external"], "description": "tab = OTerminal browser tab, external = OS default browser. Default tab." },
                },
                "required": ["url"],
                "additionalProperties": false,
            },
        },
        {
            "name": "terminal.run",
            "description": "Run a command in an OTerminal terminal so the user can watch it. This is for user-visible commands: it does NOT return the command's output.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "command": { "type": "string", "description": "The command line to run." },
                    "name": { "type": "string", "description": "Terminal tab name. Default \"MCP\"." },
                    "cwd": { "type": "string", "description": "Absolute working directory. Defaults to the window's first folder." },
                },
                "required": ["command"],
                "additionalProperties": false,
            },
        },
        {
            "name": "workspace.list",
            "description": "List the folders and git repositories open in each OTerminal window.",
            "inputSchema": { "type": "object", "properties": {}, "additionalProperties": false },
        },
    ])
}

pub(crate) async fn call(name: &str, arguments: Value, cx: &mut AsyncApp) -> Result<Value> {
    fn parse<T: serde::de::DeserializeOwned>(arguments: Value) -> Result<T> {
        serde_json::from_value(arguments).context("invalid tool arguments")
    }

    match name {
        "git.status" => git_status(parse(arguments)?, cx),
        "git.stage" => git_stage(parse(arguments)?, true, cx).await,
        "git.unstage" => git_stage(parse(arguments)?, false, cx).await,
        "git.commit" => git_commit(parse(arguments)?, cx).await,
        "git.diff" => git_diff(parse(arguments)?, cx).await,
        "git.branch" => git_branch(parse(arguments)?, cx),
        "git.checkout" => git_checkout(parse(arguments)?, cx).await,
        "git.fetch" => git_fetch(parse(arguments)?, cx).await,
        "git.pull" => git_pull(parse(arguments)?, cx).await,
        "git.push" => git_push(parse(arguments)?, cx).await,
        "editor.openFile" => editor_open_file(parse(arguments)?, cx).await,
        "browser.open" => browser_open(parse(arguments)?, cx),
        "terminal.run" => terminal_run(parse(arguments)?, cx),
        "workspace.list" => Ok(cx.update(|cx| workspace_list(cx))),
        _ => bail!("Unknown tool: {name}"),
    }
}

#[derive(Clone)]
struct WorkspaceTarget {
    window: WindowHandle<MultiWorkspace>,
    workspace: Entity<Workspace>,
}

impl WorkspaceTarget {
    fn folders(&self, cx: &App) -> Vec<PathBuf> {
        self.workspace
            .read(cx)
            .visible_worktrees(cx)
            .map(|worktree| worktree.read(cx).abs_path().to_path_buf())
            .collect()
    }

    fn repositories(&self, cx: &App) -> Vec<Entity<Repository>> {
        self.workspace
            .read(cx)
            .project()
            .read(cx)
            .repositories(cx)
            .values()
            .cloned()
            .collect()
    }
}

/// Every open workspace, starting with the one shown in the active window.
fn workspace_targets(cx: &App) -> Vec<WorkspaceTarget> {
    let active_window_id = cx.active_window().map(|window| window.window_id());
    let mut windows = cx
        .windows()
        .into_iter()
        .filter_map(|window| window.downcast::<MultiWorkspace>())
        .collect::<Vec<_>>();
    windows.sort_by_key(|window| Some(window.window_id()) != active_window_id);

    let mut targets = Vec::new();
    for window in windows {
        let Ok(multi_workspace) = window.read(cx) else {
            continue;
        };
        let displayed = multi_workspace.workspace().clone();
        targets.push(WorkspaceTarget {
            window,
            workspace: displayed.clone(),
        });
        for workspace in multi_workspace.workspaces() {
            if *workspace != displayed {
                targets.push(WorkspaceTarget {
                    window,
                    workspace: workspace.clone(),
                });
            }
        }
    }
    targets
}

/// The workspace with `path` in one of its folders, else the first one.
fn workspace_target_for_path(path: Option<&Path>, cx: &App) -> Result<WorkspaceTarget> {
    let targets = workspace_targets(cx);
    let containing = path.and_then(|path| {
        let path = path_key(path);
        targets.iter().find(|target| {
            target
                .folders(cx)
                .iter()
                .any(|folder| is_within(&path, &path_key(folder)))
        })
    });
    containing
        .or(targets.first())
        .cloned()
        .context("No OTerminal window is open")
}

/// A form of `path` that compares equal for the spellings of one path that clients
/// send: either separator, and any letter case on Windows.
fn path_key(path: &Path) -> String {
    let key = path.to_string_lossy().replace('\\', "/");
    let key = key.trim_end_matches('/');
    if cfg!(windows) {
        key.to_lowercase()
    } else {
        key.to_string()
    }
}

fn is_within(path_key: &str, directory_key: &str) -> bool {
    path_key
        .strip_prefix(directory_key)
        .is_some_and(|rest| rest.is_empty() || rest.starts_with('/'))
}

fn resolve_repository(hint: Option<&str>, cx: &App) -> Result<Entity<Repository>> {
    let targets = workspace_targets(cx);

    if let Some(hint) = hint {
        let hint_key = path_key(Path::new(hint));
        let mut best: Option<(usize, Entity<Repository>)> = None;
        for target in &targets {
            for repository in target.repositories(cx) {
                let directory_key = path_key(&repository.read(cx).work_directory_abs_path);
                let is_better = best
                    .as_ref()
                    .is_none_or(|(length, _)| directory_key.len() > *length);
                if is_within(&hint_key, &directory_key) && is_better {
                    best = Some((directory_key.len(), repository));
                }
            }
        }
        return best
            .map(|(_, repository)| repository)
            .with_context(|| format!("No repository open in OTerminal contains {hint}"));
    }

    targets
        .iter()
        .find_map(|target| {
            target
                .workspace
                .read(cx)
                .project()
                .read(cx)
                .active_repository(cx)
        })
        .or_else(|| {
            targets
                .iter()
                .find_map(|target| target.repositories(cx).into_iter().next())
        })
        .context("No git repository is open in OTerminal")
}

fn repo_paths(repository: &Repository, paths: &[String]) -> Result<Vec<RepoPath>> {
    paths
        .iter()
        .map(|path| {
            let path = Path::new(path);
            let abs_path = if path.is_absolute() {
                path.to_path_buf()
            } else {
                repository.work_directory_abs_path.join(path)
            };
            repository
                .abs_path_to_repo_path(&abs_path)
                .with_context(|| format!("{} is not inside the repository", abs_path.display()))
        })
        .collect()
}

/// git prompts (a passphrase, a password) cannot be answered by an MCP client, so they
/// are declined and the command fails with git's own message.
fn declining_askpass(cx: &mut AsyncApp) -> AskPassDelegate {
    AskPassDelegate::new(cx, |_prompt, _response, _cx| {})
}

fn is_ai_attribution(line: &str) -> bool {
    let line = line.trim().to_lowercase();
    if let Some(author) = line.strip_prefix("co-authored-by:") {
        let author = author.trim_start();
        return author.starts_with("claude")
            || author.starts_with("github copilot")
            || author.contains("openai")
            || author.contains("<noreply@anthropic.com>");
    }
    let (has_robot, line) = match line.strip_prefix('🤖') {
        Some(rest) => (true, rest.trim_start()),
        None => (false, line.as_str()),
    };
    let generated = line
        .strip_prefix("generated with")
        .or_else(|| line.strip_prefix("generated-with"));
    generated.is_some_and(|rest| has_robot || rest.contains("claude"))
}

/// Commits made through OTerminal are authored by the local git user alone, whoever
/// wrote the message.
fn strip_ai_attribution(message: &str) -> String {
    let mut lines = Vec::new();
    for line in message.lines().filter(|line| !is_ai_attribution(line)) {
        let follows_blank_line = lines
            .last()
            .is_some_and(|last: &&str| last.trim().is_empty());
        if line.trim().is_empty() && (lines.is_empty() || follows_blank_line) {
            continue;
        }
        lines.push(line);
    }
    lines.join("\n").trim_end().to_string()
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct RepositoryInput {
    repository: Option<String>,
}

fn git_status(input: RepositoryInput, cx: &mut AsyncApp) -> Result<Value> {
    cx.update(|cx| {
        let repository = resolve_repository(input.repository.as_deref(), cx)?;
        let repository = repository.read(cx);

        let mut staged = Vec::new();
        let mut unstaged = Vec::new();
        let mut untracked = Vec::new();
        let mut conflicts = Vec::new();
        for entry in repository.status() {
            let path = entry
                .repo_path
                .as_std_path()
                .to_string_lossy()
                .replace('\\', "/");
            if entry.status.is_conflicted() {
                conflicts.push(path);
            } else if entry.status.is_untracked() {
                untracked.push(path);
            } else {
                let staging = entry.status.staging();
                if staging.has_staged() {
                    staged.push(path.clone());
                }
                if staging.has_unstaged() {
                    unstaged.push(path);
                }
            }
        }

        let upstream = repository
            .branch
            .as_ref()
            .and_then(|branch| branch.upstream.as_ref());
        let tracking = upstream.and_then(|upstream| match upstream.tracking {
            UpstreamTracking::Tracked(status) => Some(status),
            UpstreamTracking::Gone => None,
        });
        Ok(json!({
            "repository": repository.work_directory_abs_path,
            "branch": repository.branch.as_ref().map(|branch| branch.name()),
            "upstream": upstream.map(|upstream| upstream.ref_name.as_ref()),
            "ahead": tracking.map(|tracking| tracking.ahead),
            "behind": tracking.map(|tracking| tracking.behind),
            "staged": staged,
            "unstaged": unstaged,
            "untracked": untracked,
            "conflicts": conflicts,
        }))
    })
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct StageInput {
    repository: Option<String>,
    paths: Vec<String>,
    all: bool,
}

async fn git_stage(input: StageInput, stage: bool, cx: &mut AsyncApp) -> Result<Value> {
    if !input.all && input.paths.is_empty() {
        bail!("Pass \"paths\" or {{\"all\": true}}");
    }
    let task: Task<Result<()>> = cx.update(|cx| {
        let repository = resolve_repository(input.repository.as_deref(), cx)?;
        repository.update(cx, |repository, cx| {
            if input.all {
                return Ok(if stage {
                    repository.stage_all(cx)
                } else {
                    repository.unstage_all(cx)
                });
            }
            let paths = repo_paths(repository, &input.paths)?;
            anyhow::Ok(if stage {
                repository.stage_entries(paths, cx)
            } else {
                repository.unstage_entries(paths, cx)
            })
        })
    })?;
    task.await?;
    Ok(json!({
        (if stage { "staged" } else { "unstaged" }): if input.all { json!("all") } else { json!(input.paths) },
    }))
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct CommitInput {
    repository: Option<String>,
    message: String,
    amend: bool,
    signoff: bool,
}

async fn git_commit(input: CommitInput, cx: &mut AsyncApp) -> Result<Value> {
    let message = strip_ai_attribution(&input.message);
    if message.is_empty() {
        bail!(
            "The commit message is empty once AI attribution is stripped. Provide a real message."
        );
    }
    let options = CommitOptions {
        amend: input.amend,
        signoff: input.signoff,
        ..Default::default()
    };
    let askpass = declining_askpass(cx);
    let (repository, commit) = cx.update(|cx| {
        let repository = resolve_repository(input.repository.as_deref(), cx)?;
        let commit = repository.update(cx, |repository, cx| {
            repository.commit(message.clone().into(), None, options, askpass, cx)
        });
        anyhow::Ok((repository, commit))
    })?;
    commit.await??;

    let branch = repository.read_with(cx, |repository, _| {
        repository
            .branch
            .as_ref()
            .map(|branch| branch.name().to_string())
    });
    Ok(json!({
        "committed": true,
        "message": message,
        "branch": branch,
        "note": "Committed as the local git user (user.name / user.email). No AI attribution was added.",
    }))
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct DiffInput {
    repository: Option<String>,
    cached: bool,
}

async fn git_diff(input: DiffInput, cx: &mut AsyncApp) -> Result<Value> {
    let diff_type = if input.cached {
        DiffType::HeadToIndex
    } else {
        DiffType::HeadToWorktree
    };
    let diff = cx.update(|cx| {
        let repository = resolve_repository(input.repository.as_deref(), cx)?;
        anyhow::Ok(repository.update(cx, |repository, cx| repository.diff(diff_type, cx)))
    })?;
    let diff = diff.await??;
    Ok(Value::String(if diff.trim().is_empty() {
        "No changes.".to_string()
    } else {
        diff
    }))
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct BranchInput {
    repository: Option<String>,
    remote: bool,
}

fn git_branch(input: BranchInput, cx: &mut AsyncApp) -> Result<Value> {
    cx.update(|cx| {
        let repository = resolve_repository(input.repository.as_deref(), cx)?;
        let repository = repository.read(cx);
        let branches = repository
            .branch_list
            .iter()
            .filter(|branch| input.remote || !branch.is_remote())
            .map(|branch| {
                json!({
                    "name": branch.name(),
                    "remote": branch.is_remote(),
                    "upstream": branch.upstream.as_ref().map(|upstream| upstream.ref_name.as_ref()),
                })
            })
            .collect::<Vec<_>>();
        Ok(json!({
            "current": repository.branch.as_ref().map(|branch| branch.name()),
            "branches": branches,
        }))
    })
}

#[derive(Deserialize, Default)]
#[serde(default, rename_all = "camelCase")]
struct CheckoutInput {
    repository: Option<String>,
    branch: Option<String>,
    create_branch: Option<String>,
    r#ref: Option<String>,
}

async fn git_checkout(input: CheckoutInput, cx: &mut AsyncApp) -> Result<Value> {
    let checkout = cx.update(|cx| {
        let repository = resolve_repository(input.repository.as_deref(), cx)?;
        repository.update(cx, |repository, _| {
            if let Some(new_branch) = input.create_branch.clone() {
                Ok(repository.create_branch(new_branch, input.r#ref.clone()))
            } else {
                let branch = input
                    .branch
                    .clone()
                    .context("Pass \"branch\" or \"createBranch\"")?;
                anyhow::Ok(repository.change_branch(branch))
            }
        })
    })?;
    checkout.await??;
    Ok(json!({ "branch": input.create_branch.or(input.branch) }))
}

fn remote_output(output: git::repository::RemoteCommandOutput) -> Value {
    let text = [output.stdout.trim(), output.stderr.trim()]
        .into_iter()
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join("\n");
    Value::String(if text.is_empty() {
        "Done.".to_string()
    } else {
        text
    })
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct FetchInput {
    repository: Option<String>,
    remote: Option<String>,
}

async fn git_fetch(input: FetchInput, cx: &mut AsyncApp) -> Result<Value> {
    let options = match input.remote {
        Some(remote) => FetchOptions::Remote(Remote {
            name: remote.into(),
        }),
        None => FetchOptions::All,
    };
    let askpass = declining_askpass(cx);
    let fetch = cx.update(|cx| {
        let repository = resolve_repository(input.repository.as_deref(), cx)?;
        anyhow::Ok(repository.update(cx, |repository, cx| repository.fetch(options, askpass, cx)))
    })?;
    Ok(remote_output(fetch.await??))
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct PullInput {
    repository: Option<String>,
    rebase: bool,
}

async fn git_pull(input: PullInput, cx: &mut AsyncApp) -> Result<Value> {
    let askpass = declining_askpass(cx);
    let pull = cx.update(|cx| {
        let repository = resolve_repository(input.repository.as_deref(), cx)?;
        repository.update(cx, |repository, cx| {
            let upstream = repository
                .branch
                .as_ref()
                .context("No branch is checked out")?
                .upstream
                .as_ref()
                .context("The current branch has no upstream to pull from")?;
            let remote = upstream
                .remote_name()
                .context("The upstream of the current branch is not on a remote")?
                .to_string();
            anyhow::Ok(repository.pull(None, remote.into(), input.rebase, askpass, cx))
        })
    })?;
    Ok(remote_output(pull.await??))
}

#[derive(Deserialize, Default)]
#[serde(default, rename_all = "camelCase")]
struct PushInput {
    repository: Option<String>,
    remote: Option<String>,
    force_with_lease: bool,
}

async fn git_push(input: PushInput, cx: &mut AsyncApp) -> Result<Value> {
    let (repository, branch, remotes) = cx.update(|cx| {
        let repository = resolve_repository(input.repository.as_deref(), cx)?;
        let (branch, remotes) = repository.update(cx, |repository, _| {
            let branch = repository
                .branch
                .clone()
                .context("No branch is checked out")?;
            let remotes = repository.get_remotes(Some(branch.name().to_string()), true);
            anyhow::Ok((branch, remotes))
        })?;
        anyhow::Ok((repository, branch, remotes))
    })?;
    let remotes = remotes.await??;

    let remote = match input.remote {
        Some(remote) => remote,
        None => match remotes.as_slice() {
            [remote] => remote.name.to_string(),
            remotes
                if remotes
                    .iter()
                    .any(|remote| remote.name.as_ref() == "origin") =>
            {
                "origin".to_string()
            }
            [] => bail!("The repository has no remote to push to"),
            _ => bail!("The repository has several remotes; pass \"remote\""),
        },
    };
    let tracked_upstream = branch
        .upstream
        .as_ref()
        .filter(|upstream| matches!(upstream.tracking, UpstreamTracking::Tracked(_)));
    let options = if input.force_with_lease {
        Some(PushOptions::Force)
    } else if tracked_upstream.is_none() {
        Some(PushOptions::SetUpstream)
    } else {
        None
    };
    let remote_branch = tracked_upstream
        .and_then(|upstream| upstream.branch_name())
        .unwrap_or_else(|| branch.name())
        .to_string();

    let askpass = declining_askpass(cx);
    let push = repository.update(cx, |repository, cx| {
        repository.push(
            branch.name().to_string().into(),
            remote_branch.into(),
            remote.into(),
            options,
            askpass,
            cx,
        )
    });
    Ok(remote_output(push.await??))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct OpenFileInput {
    path: String,
    start_line: Option<u32>,
    end_line: Option<u32>,
}

async fn editor_open_file(input: OpenFileInput, cx: &mut AsyncApp) -> Result<Value> {
    let (target, abs_path) = cx.update(|cx| {
        let path = Path::new(&input.path);
        if path.is_absolute() {
            let target = workspace_target_for_path(Some(path), cx)?;
            return Ok((target, path.to_path_buf()));
        }
        let target = workspace_target_for_path(None, cx)?;
        let folder = target
            .folders(cx)
            .into_iter()
            .next()
            .context("The window has no folder to resolve a relative path against")?;
        anyhow::Ok((target, folder.join(path)))
    })?;

    let open = target.window.update(cx, |_, window, cx| {
        target.workspace.update(cx, |workspace, cx| {
            workspace.open_abs_path(
                abs_path.clone(),
                OpenOptions {
                    visible: Some(OpenVisible::None),
                    focus: Some(true),
                    ..Default::default()
                },
                window,
                cx,
            )
        })
    })?;
    let item = open.await?;

    if let Some(start_line) = input.start_line
        && let Some(editor) = item.downcast::<Editor>()
    {
        let start = Point::new(start_line.saturating_sub(1), 0);
        let end = input.end_line.map_or(start, |end_line| {
            Point::new(end_line.saturating_sub(1), 0).max(start)
        });
        target.window.update(cx, |_, window, cx| {
            editor.update(cx, |editor, cx| {
                let max_point = editor.buffer().read(cx).snapshot(cx).max_point();
                editor.go_to_singleton_buffer_range(
                    start.min(max_point)..end.min(max_point),
                    window,
                    cx,
                );
            })
        })?;
    }
    Ok(json!({ "opened": abs_path }))
}

#[derive(Deserialize)]
struct BrowserOpenInput {
    url: String,
    target: Option<String>,
}

fn browser_open(input: BrowserOpenInput, cx: &mut AsyncApp) -> Result<Value> {
    let url = url::Url::parse(&input.url).context("\"url\" must be an absolute URL")?;
    if !matches!(url.scheme(), "http" | "https") {
        bail!("Only http and https URLs can be opened");
    }
    let url = url.to_string();

    match input.target.as_deref() {
        Some("external") => cx.update(|cx| cx.open_url(&url)),
        Some("tab") | None => {
            let target = cx.update(|cx| workspace_target_for_path(None, cx))?;
            target.window.update(cx, |_, window, cx| {
                target.workspace.update(cx, |workspace, cx| {
                    let pane = workspace.active_pane().clone();
                    BrowserTab::open_url(url.clone(), pane, workspace, window, cx);
                })
            })?;
        }
        Some(other) => bail!("Unknown target: {other}"),
    }
    Ok(json!({ "opened": url }))
}

#[derive(Deserialize)]
struct TerminalRunInput {
    command: String,
    name: Option<String>,
    cwd: Option<String>,
}

fn terminal_run(input: TerminalRunInput, cx: &mut AsyncApp) -> Result<Value> {
    let name = input.name.unwrap_or_else(|| "MCP".to_string());
    let requested_cwd = input.cwd.map(PathBuf::from);
    let (target, cwd) = cx.update(|cx| {
        let target = workspace_target_for_path(requested_cwd.as_deref(), cx)?;
        let cwd = requested_cwd.or_else(|| target.folders(cx).into_iter().next());
        anyhow::Ok((target, cwd))
    })?;

    let spawn = SpawnInTerminal {
        id: TaskId(format!("oterminal-mcp:{name}")),
        full_label: name.clone(),
        label: name.clone(),
        command: Some(input.command.clone()),
        command_label: input.command.clone(),
        cwd,
        // A command that is still running (a dev server, say) keeps its terminal.
        allow_concurrent_runs: true,
        reveal: RevealStrategy::Always,
        show_command: true,
        ..Default::default()
    };
    target.window.update(cx, |_, window, cx| {
        target.workspace.update(cx, |workspace, cx| {
            workspace.spawn_in_terminal(spawn, window, cx).detach();
        })
    })?;
    Ok(json!({
        "started": input.command,
        "terminal": name,
        "note": "The command runs in a terminal the user can see. Its output is not returned.",
    }))
}

fn workspace_list(cx: &App) -> Value {
    let active_window_id = cx.active_window().map(|window| window.window_id());
    let workspaces = workspace_targets(cx)
        .into_iter()
        .map(|target| {
            let repositories = target
                .repositories(cx)
                .into_iter()
                .map(|repository| repository.read(cx).work_directory_abs_path.clone())
                .collect::<Vec<_>>();
            json!({
                "inActiveWindow": Some(target.window.window_id()) == active_window_id,
                "folders": target.folders(cx),
                "repositories": repositories,
            })
        })
        .collect::<Vec<_>>();
    json!({ "workspaces": workspaces })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_strip_ai_attribution() {
        let message = "Fix the parser\n\nHandles empty input.\n\n🤖 Generated with [Claude Code](https://claude.com/claude-code)\n\nCo-Authored-By: Claude <noreply@anthropic.com>\nCo-authored-by: Ada Lovelace <ada@example.com>\n";
        assert_eq!(
            strip_ai_attribution(message),
            "Fix the parser\n\nHandles empty input.\n\nCo-authored-by: Ada Lovelace <ada@example.com>"
        );
        assert_eq!(
            strip_ai_attribution("Co-Authored-By: GitHub Copilot <copilot@github.com>"),
            ""
        );
        assert_eq!(
            strip_ai_attribution("Generated with care by hand"),
            "Generated with care by hand"
        );
    }

    #[test]
    fn test_is_within() {
        let directory = path_key(Path::new("/home/user/project/"));
        assert!(is_within(
            &path_key(Path::new("/home/user/project")),
            &directory
        ));
        assert!(is_within(
            &path_key(Path::new("/home/user/project/src/main.rs")),
            &directory
        ));
        assert!(!is_within(
            &path_key(Path::new("/home/user/project-two")),
            &directory
        ));
    }
}
