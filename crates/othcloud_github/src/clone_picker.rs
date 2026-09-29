//! "Clone Repository": pick one of the active GitHub account's repositories
//! (fuzzy search) or paste any git URL, choose where to clone it, clone it
//! with progress, and open it as a new project tab.

use std::{
    path::{Path, PathBuf},
    sync::{Arc, atomic::AtomicBool},
    time::{Duration, Instant},
};

use anyhow::{Context as _, Result};
use db::kvp::KeyValueStore;
use fs::{Fs, JobEvent};
use futures::StreamExt as _;
use fuzzy::StringMatchCandidate;
use gpui::{
    AnyElement, App, AppContext as _, Context, DismissEvent, Entity, EventEmitter, FocusHandle,
    Focusable, PathPromptOptions, Render, SharedString, Subscription, Task, WeakEntity, Window,
};
use othcloud_client::GithubTokenKind;
use picker::{Picker, PickerDelegate};
use ui::{
    ContextMenu, HighlightedLabel, IconPosition, ListItem, ListItemSpacing, PopoverMenu, Tooltip,
    prelude::*,
};
use util::ResultExt as _;
use workspace::{ModalView, OpenMode, Workspace};

use crate::{
    AddGithubToken, GithubAccountStore, ResolvedAccount, SignInToGithub,
    account_picker::switch_to_othcloud_account,
    github_api::{self, GithubError, GithubRepo},
    oauth_client_id, othcloud_signed_in, show_progress, show_status,
};

/// Key-value store key of the folder repositories are cloned into.
const CLONE_PARENT_KEY: &str = "othcloud.github.cloneParentDirectory";
/// Minimum time between two progress toasts.
const PROGRESS_TOAST_INTERVAL: Duration = Duration::from_millis(1200);

/// `%USERPROFILE%\Documents\GitHub` (`~/Documents/GitHub`), where OTerminal
/// clones repositories unless the user picked another folder.
pub fn default_clone_directory() -> PathBuf {
    util::paths::home_dir().join("Documents").join("GitHub")
}

fn remembered_clone_directory(cx: &App) -> PathBuf {
    KeyValueStore::global(cx)
        .read_kvp(CLONE_PARENT_KEY)
        .log_err()
        .flatten()
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .unwrap_or_else(default_clone_directory)
}

/// A repository to clone that the user typed or pasted.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CloneTarget {
    pub url: String,
    /// What to show, e.g. `octo/hello` for GitHub repositories.
    pub display: String,
    /// The directory `git clone` creates.
    pub directory_name: String,
}

fn strip_git_suffix(name: &str) -> &str {
    name.strip_suffix(".git").unwrap_or(name)
}

fn is_valid_directory_name(name: &str) -> bool {
    !name.is_empty()
        && name != "."
        && name != ".."
        && !name.contains(['/', '\\', ':', '*', '?', '"', '<', '>', '|'])
}

fn is_github_name(part: &str) -> bool {
    !part.is_empty()
        && part
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
}

fn github_target(owner: &str, repo: &str) -> Option<CloneTarget> {
    let repo = strip_git_suffix(repo);
    if !is_github_name(owner) || !is_github_name(repo) || !is_valid_directory_name(repo) {
        return None;
    }
    Some(CloneTarget {
        url: format!("{}/{owner}/{repo}.git", github_api::GITHUB_WEB),
        display: format!("{owner}/{repo}"),
        directory_name: repo.to_string(),
    })
}

/// Understands what the user typed as a repository to clone: any git URL
/// (`https://…`, `git@host:owner/repo.git`, `ssh://…`), a GitHub web URL
/// (`https://github.com/owner/repo/tree/main` → the repository), or
/// `github.com/owner/repo` / `owner/repo` shorthands for GitHub.
pub fn parse_clone_input(input: &str) -> Option<CloneTarget> {
    let input = input.trim().trim_end_matches('/');
    if input.is_empty() || input.contains(char::is_whitespace) {
        return None;
    }

    let lower = input.to_ascii_lowercase();
    for prefix in [
        "https://github.com/",
        "http://github.com/",
        "https://www.github.com/",
        "github.com/",
        "www.github.com/",
    ] {
        if lower.starts_with(prefix) {
            let mut parts = input[prefix.len()..].split('/');
            let owner = parts.next()?;
            let repo = parts.next()?;
            return github_target(owner, repo);
        }
    }

    let is_url = ["https://", "http://", "ssh://", "git://", "file://"]
        .iter()
        .any(|scheme| lower.starts_with(scheme));
    let is_scp_like =
        !is_url && input.contains('@') && input.contains(':') && !input.contains("://");
    if is_url || is_scp_like {
        let last = input
            .rsplit(['/', ':'])
            .next()
            .map(strip_git_suffix)
            .unwrap_or_default();
        if !is_valid_directory_name(last) {
            return None;
        }
        return Some(CloneTarget {
            url: input.to_string(),
            display: input.to_string(),
            directory_name: last.to_string(),
        });
    }

    // `owner/repo`
    let mut parts = input.split('/');
    match (parts.next(), parts.next(), parts.next()) {
        (Some(owner), Some(repo), None) => github_target(owner, repo),
        _ => None,
    }
}

/// Turns git's last error line into something actionable.
pub fn friendly_clone_error(raw: &str, account: Option<&str>) -> String {
    let lower = raw.to_lowercase();
    let signed_in_as = |what: &str| match account {
        Some(login) => format!("{what} Signed in to GitHub as {login}."),
        None => what.to_string(),
    };
    if lower.contains("authentication failed")
        || lower.contains("invalid username or token")
        || lower.contains("invalid username or password")
        || lower.contains("could not read username")
        || lower.contains("could not read password")
    {
        return match account {
            Some(login) => format!(
                "GitHub rejected the token of {login}: it expired, was revoked or can't access \
                 this repository. Sign in again or switch accounts."
            ),
            None => "GitHub asked for credentials. Add a GitHub account (or sign in with your \
                     git credential manager) to clone private repositories."
                .to_string(),
        };
    }
    if lower.contains("repository not found")
        || (lower.contains("repository") && lower.contains("not found"))
    {
        return signed_in_as(
            "Repository not found — check the URL, or make sure your GitHub account can access it.",
        );
    }
    if lower.contains("permission to") && lower.contains("denied") || lower.contains("403") {
        return signed_in_as("GitHub denied access to this repository.");
    }
    if lower.contains("could not resolve host")
        || lower.contains("failed to connect")
        || lower.contains("unable to access") && lower.contains("timed out")
        || lower.contains("connection timed out")
        || lower.contains("network is unreachable")
    {
        return "Can't reach the git server. Check your internet connection.".to_string();
    }
    if lower.contains("already exists and is not an empty directory") {
        return "The destination folder already exists and isn't empty. Choose another folder."
            .to_string();
    }
    if lower.contains("permission denied (publickey)") {
        return "The SSH key was rejected. Use the HTTPS URL to clone with your GitHub account."
            .to_string();
    }
    let raw = raw.trim();
    let raw = raw.strip_prefix("git clone failed:").unwrap_or(raw).trim();
    let raw = raw.strip_prefix("fatal:").unwrap_or(raw).trim();
    format!("Clone failed: {raw}")
}

pub struct CloneRepoModal {
    picker: Entity<Picker<CloneRepoPickerDelegate>>,
    _subscription: Option<Subscription>,
}

impl CloneRepoModal {
    pub fn new(
        workspace: WeakEntity<Workspace>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let delegate = CloneRepoPickerDelegate {
            modal: cx.entity().downgrade(),
            workspace,
            repos: Arc::new(Vec::new()),
            candidates: Arc::new(Vec::new()),
            matches: Vec::new(),
            selected_index: 0,
            loading: false,
            error: None,
            loaded_token: None,
            parent_dir: remembered_clone_directory(cx),
            _load_task: None,
        };
        let picker = cx.new(|cx| Picker::list(delegate, window, cx).initial_width(rems(42.)));
        let subscription = GithubAccountStore::global(cx).map(|store| {
            let picker = picker.clone();
            cx.subscribe_in(&store, window, move |_, _, _, window, cx| {
                picker.update(cx, |picker, cx| {
                    picker.delegate.load_repos(window, cx);
                    picker.refresh(window, cx);
                });
            })
        });
        picker.update(cx, |picker, cx| picker.delegate.load_repos(window, cx));
        Self {
            picker,
            _subscription: subscription,
        }
    }
}

impl Render for CloneRepoModal {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .key_context("CloneRepoModal")
            .w(rems(42.))
            .child(self.picker.clone())
    }
}

impl Focusable for CloneRepoModal {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.picker.focus_handle(cx)
    }
}

impl EventEmitter<DismissEvent> for CloneRepoModal {}
impl ModalView for CloneRepoModal {}

#[derive(Clone, Debug)]
enum Entry {
    /// Clone what the user typed.
    Target(CloneTarget),
    Repo {
        ix: usize,
        positions: Vec<usize>,
    },
    /// No account: offer to add one.
    AddAccount,
}

pub struct CloneRepoPickerDelegate {
    modal: WeakEntity<CloneRepoModal>,
    workspace: WeakEntity<Workspace>,
    repos: Arc<Vec<GithubRepo>>,
    candidates: Arc<Vec<StringMatchCandidate>>,
    matches: Vec<Entry>,
    selected_index: usize,
    loading: bool,
    error: Option<SharedString>,
    /// The token the repository list was loaded with.
    loaded_token: Option<String>,
    parent_dir: PathBuf,
    _load_task: Option<Task<()>>,
}

impl CloneRepoPickerDelegate {
    fn load_repos(&mut self, window: &mut Window, cx: &mut Context<Picker<Self>>) {
        let store = GithubAccountStore::global(cx);
        let token = store
            .as_ref()
            .and_then(|store| store.read(cx).current_token());
        if token == self.loaded_token
            && (self.loading || !self.repos.is_empty() || self.error.is_some())
        {
            return;
        }
        self.loaded_token = token.clone();
        self.repos = Arc::new(Vec::new());
        self.candidates = Arc::new(Vec::new());
        self.error = None;
        let Some(token) = token else {
            // Still fetching the active account's token?
            self.loading =
                store.is_some_and(|store| store.read(cx).resolved(cx) != ResolvedAccount::None);
            self._load_task = None;
            return;
        };
        self.loading = true;
        let http = cx.http_client();
        let load =
            cx.background_spawn(async move { github_api::list_repositories(http, &token).await });
        self._load_task = Some(cx.spawn_in(window, async move |picker, cx| {
            let result = load.await;
            picker
                .update_in(cx, |picker, window, cx| {
                    let delegate = &mut picker.delegate;
                    delegate.loading = false;
                    match result {
                        Ok(repos) => {
                            delegate.candidates = Arc::new(
                                repos
                                    .iter()
                                    .enumerate()
                                    .map(|(ix, repo)| StringMatchCandidate::new(ix, &repo.full_name))
                                    .collect(),
                            );
                            delegate.repos = Arc::new(repos);
                        }
                        Err(error) => {
                            log::warn!("listing GitHub repositories failed: {error}");
                            delegate.error = Some(
                                match &error {
                                    GithubError::Unauthorized => {
                                        "GitHub rejected the account's token: it expired or was \
                                         revoked. Sign in again, or paste a URL."
                                            .to_string()
                                    }
                                    error => format!(
                                        "Couldn't list your repositories. {} You can still paste a URL.",
                                        error.message()
                                    ),
                                }
                                .into(),
                            );
                        }
                    }
                    picker.refresh(window, cx);
                })
                .ok();
        }));
    }

    fn account_label(&self, cx: &App) -> Option<String> {
        GithubAccountStore::global(cx)?.read(cx).active_label(cx)
    }

    fn choose_parent_dir(&mut self, window: &mut Window, cx: &mut Context<Picker<Self>>) {
        let prompt = cx.prompt_for_paths(PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: Some("Clone Into".into()),
        });
        cx.spawn_in(window, async move |picker, cx| {
            let Some(mut paths) = prompt.await.ok().and_then(|result| result.ok()).flatten() else {
                return;
            };
            let Some(path) = paths.pop() else {
                return;
            };
            picker
                .update_in(cx, |picker, window, cx| {
                    let kvp = KeyValueStore::global(cx);
                    let value = path.to_string_lossy().into_owned();
                    db::write_and_log(cx, move || async move {
                        kvp.write_kvp(CLONE_PARENT_KEY.to_string(), value).await
                    });
                    picker.delegate.parent_dir = path;
                    cx.notify();
                    window.focus(&picker.focus_handle(cx), cx);
                })
                .ok();
        })
        .detach();
    }

    fn render_account_menu(&self) -> impl IntoElement {
        let workspace = self.workspace.clone();
        PopoverMenu::new("clone-account-menu")
            .trigger_with_tooltip(
                Button::new("clone-switch-account", "Switch")
                    .style(ButtonStyle::Subtle)
                    .label_size(LabelSize::Small)
                    .end_icon(Icon::new(IconName::ChevronDown).size(IconSize::XSmall)),
                Tooltip::text("Choose the GitHub account to clone with"),
            )
            .anchor(gpui::Anchor::BottomRight)
            .menu(move |window, cx| {
                let store = GithubAccountStore::global(cx)?;
                let workspace = workspace.clone();
                Some(ContextMenu::build(window, cx, move |mut menu, _, cx| {
                    let store_ref = store.read(cx);
                    let resolved = store_ref.resolved(cx);
                    let active_othcloud = store_ref.active_account_id().map(ToString::to_string);
                    let locals = store_ref.local_accounts().to_vec();
                    let othcloud_accounts = store_ref
                        .othcloud_accounts()
                        .map(|accounts| accounts.to_vec())
                        .unwrap_or_default();
                    let signed_in = othcloud_signed_in(cx);

                    menu = menu.header("On this PC");
                    for account in locals.iter().cloned() {
                        let store = store.clone();
                        menu = menu.toggleable_entry(
                            account.login.clone(),
                            resolved == ResolvedAccount::Local(account.id),
                            IconPosition::Start,
                            None,
                            move |_, cx| {
                                store.update(cx, |store, cx| {
                                    store.use_local_account(account.id, cx)
                                });
                            },
                        );
                    }
                    let add_label = if oauth_client_id(cx).is_some() {
                        "Sign in with GitHub…"
                    } else {
                        "Add account with a token…"
                    };
                    menu = menu.entry(add_label, None, |window, cx| {
                        window.dispatch_action(Box::new(SignInToGithub), cx);
                    });

                    if signed_in {
                        menu = menu.header("OTHCloud");
                        for account in othcloud_accounts.iter().cloned() {
                            let workspace = workspace.clone();
                            let is_active = resolved == ResolvedAccount::Othcloud
                                && active_othcloud.as_deref() == Some(account.id.as_str());
                            let label = if account.kind == GithubTokenKind::Installation {
                                format!("{} (GitHub App)", account.label)
                            } else {
                                account.label.clone()
                            };
                            menu = menu.toggleable_entry(
                                label,
                                is_active,
                                IconPosition::Start,
                                None,
                                move |_, cx| {
                                    switch_to_othcloud_account(
                                        workspace.clone(),
                                        account.clone(),
                                        cx,
                                    );
                                },
                            );
                        }
                        if othcloud_accounts.is_empty() {
                            let store = store.clone();
                            menu = menu.toggleable_entry(
                                "OTHCloud account",
                                resolved == ResolvedAccount::Othcloud,
                                IconPosition::Start,
                                None,
                                move |_, cx| {
                                    store.update(cx, |store, cx| {
                                        store.use_othcloud_account(None, None, cx)
                                    });
                                },
                            );
                        }
                    }

                    menu = menu.separator();
                    let store = store.clone();
                    menu.toggleable_entry(
                        "No account (use git's credential manager)",
                        resolved == ResolvedAccount::None,
                        IconPosition::Start,
                        None,
                        move |_, cx| store.update(cx, |store, cx| store.use_no_account(cx)),
                    )
                }))
            })
    }
}

fn clone_into(
    workspace: WeakEntity<Workspace>,
    parent_dir: PathBuf,
    target: CloneTarget,
    cx: &mut App,
) {
    let Some(fs) = workspace
        .read_with(cx, |workspace, _| workspace.app_state().fs.clone())
        .ok()
    else {
        return;
    };
    let account = GithubAccountStore::global(cx).and_then(|store| {
        store
            .read(cx)
            .active_account()
            .map(|active| active.display_name())
    });
    let destination = parent_dir.join(&target.directory_name);
    let display = target.display.clone();

    workspace
        .update(cx, |workspace, cx| {
            show_progress(workspace, format!("Cloning {display}…"), cx)
        })
        .ok();

    cx.spawn(async move |cx| {
        let result: Result<bool> = async {
            fs.create_dir(&parent_dir)
                .await
                .with_context(|| format!("Couldn't create {}", parent_dir.display()))?;
            if fs.is_dir(&destination).await {
                if fs.is_dir(&destination.join(".git")).await {
                    // Already cloned: just open it.
                    return Ok(false);
                }
                let is_empty = match fs.read_dir(&destination).await {
                    Ok(mut entries) => entries.next().await.is_none(),
                    Err(_) => false,
                };
                anyhow::ensure!(
                    is_empty,
                    "{} already exists and isn't a git repository. Choose another folder.",
                    destination.display()
                );
            }
            clone_with_progress(fs.clone(), &parent_dir, &target, workspace.clone(), cx).await?;
            Ok(true)
        }
        .await;

        let cloned = match result {
            Ok(cloned) => cloned,
            Err(error) => {
                let raw = format!("{error:#}");
                let message = if raw.contains("git clone failed") {
                    friendly_clone_error(&raw, account.as_deref())
                } else {
                    raw
                };
                workspace
                    .update(cx, |workspace, cx| {
                        show_status(workspace, message, true, cx)
                    })
                    .ok();
                return;
            }
        };

        workspace
            .update(cx, |workspace, cx| {
                show_status(
                    workspace,
                    if cloned {
                        format!("Cloned {display} into {}", destination.display())
                    } else {
                        format!("{display} is already cloned — opening it")
                    },
                    false,
                    cx,
                )
            })
            .ok();

        let open = workspace.update_in(cx, |workspace, window, cx| {
            workspace.open_workspace_for_paths(
                OpenMode::Activate,
                vec![destination.clone()],
                window,
                cx,
            )
        });
        if let Ok(open) = open {
            open.await.log_err();
        }
    })
    .detach();
}

/// Runs `git clone` in `parent_dir`, showing git's progress as toasts.
async fn clone_with_progress(
    fs: Arc<dyn Fs>,
    parent_dir: &Path,
    target: &CloneTarget,
    workspace: WeakEntity<Workspace>,
    cx: &mut gpui::AsyncApp,
) -> Result<()> {
    let mut jobs = fs.subscribe_to_jobs();
    let job_message = format!("Cloning {}", target.url);
    let display = target.display.clone();
    let progress = cx.spawn(async move |cx| {
        let mut job_id = None;
        let mut last_toast: Option<Instant> = None;
        while let Some(event) = jobs.next().await {
            match event {
                JobEvent::Started { info } if info.message.as_ref() == job_message => {
                    job_id = Some(info.id);
                }
                JobEvent::Updated { id, message } if Some(id) == job_id => {
                    if last_toast.is_some_and(|last| last.elapsed() < PROGRESS_TOAST_INTERVAL) {
                        continue;
                    }
                    last_toast = Some(Instant::now());
                    let text = format!("Cloning {display}: {message}");
                    workspace
                        .update(cx, |workspace, cx| show_progress(workspace, text, cx))
                        .ok();
                }
                JobEvent::Completed { id } if Some(id) == job_id => break,
                _ => {}
            }
        }
    });
    let result = fs.git_clone(parent_dir, &target.url).await;
    drop(progress);
    result
}

impl PickerDelegate for CloneRepoPickerDelegate {
    type ListItem = AnyElement;

    fn name() -> &'static str {
        "clone repository"
    }

    fn placeholder_text(&self, _window: &mut Window, _cx: &mut App) -> Arc<str> {
        "Search your repositories, or paste a git URL…".into()
    }

    fn no_matches_text(&self, _window: &mut Window, _cx: &mut App) -> Option<SharedString> {
        if self.loading {
            return Some("Loading repositories…".into());
        }
        if let Some(error) = &self.error {
            return Some(error.clone());
        }
        if self.loaded_token.is_none() {
            return Some("Paste a repository URL to clone it.".into());
        }
        Some("No matching repositories. Paste a URL or owner/repo to clone anything else.".into())
    }

    fn match_count(&self) -> usize {
        self.matches.len()
    }

    fn selected_index(&self) -> usize {
        self.selected_index
    }

    fn set_selected_index(
        &mut self,
        ix: usize,
        _window: &mut Window,
        _cx: &mut Context<Picker<Self>>,
    ) {
        self.selected_index = ix;
    }

    fn update_matches(
        &mut self,
        query: String,
        window: &mut Window,
        cx: &mut Context<Picker<Self>>,
    ) -> Task<()> {
        let query = query.trim().to_string();
        let target = parse_clone_input(&query);
        let repos = self.repos.clone();
        let candidates = self.candidates.clone();
        let show_add_account = !self.loading
            && self.loaded_token.is_none()
            && GithubAccountStore::global(cx)
                .is_some_and(|store| store.read(cx).resolved(cx) == ResolvedAccount::None);
        let executor = cx.background_executor().clone();
        cx.spawn_in(window, async move |picker, cx| {
            let mut entries = Vec::new();
            let repo_matches: Vec<Entry> = if query.is_empty() {
                (0..repos.len())
                    .map(|ix| Entry::Repo {
                        ix,
                        positions: Vec::new(),
                    })
                    .collect()
            } else {
                // A pasted URL: offer it first, unless it names a listed repo.
                let fuzzy_query = target
                    .as_ref()
                    .filter(|target| target.url.starts_with(github_api::GITHUB_WEB))
                    .map(|target| target.display.clone())
                    .unwrap_or_else(|| query.clone());
                fuzzy::match_strings(
                    candidates.as_slice(),
                    &fuzzy_query,
                    false,
                    true,
                    200,
                    &AtomicBool::new(false),
                    executor,
                )
                .await
                .into_iter()
                .map(|found| Entry::Repo {
                    ix: found.candidate_id,
                    positions: found.positions,
                })
                .collect()
            };
            if let Some(target) = target {
                let listed = repos
                    .iter()
                    .any(|repo| repo.full_name.eq_ignore_ascii_case(&target.display));
                if !listed {
                    entries.push(Entry::Target(target));
                }
            }
            entries.extend(repo_matches);
            if show_add_account && query.is_empty() {
                entries.push(Entry::AddAccount);
            }
            picker
                .update(cx, |picker, _| {
                    picker.delegate.matches = entries;
                    picker.delegate.selected_index = 0;
                })
                .ok();
        })
    }

    fn confirm(&mut self, _secondary: bool, window: &mut Window, cx: &mut Context<Picker<Self>>) {
        let Some(entry) = self.matches.get(self.selected_index).cloned() else {
            return;
        };
        let target = match entry {
            Entry::Target(target) => target,
            Entry::Repo { ix, .. } => {
                let Some(repo) = self.repos.get(ix) else {
                    return;
                };
                CloneTarget {
                    url: repo.clone_url(),
                    display: repo.full_name.clone(),
                    directory_name: repo.directory_name(),
                }
            }
            Entry::AddAccount => {
                let workspace = self.workspace.clone();
                let action: Box<dyn gpui::Action> = if oauth_client_id(cx).is_some() {
                    Box::new(SignInToGithub)
                } else {
                    Box::new(AddGithubToken)
                };
                window.defer(cx, move |window, cx| {
                    if let Some(workspace) = workspace.upgrade() {
                        window.focus(&workspace.focus_handle(cx), cx);
                    }
                    window.dispatch_action(action, cx);
                });
                self.dismissed(window, cx);
                return;
            }
        };
        clone_into(self.workspace.clone(), self.parent_dir.clone(), target, cx);
        self.dismissed(window, cx);
    }

    fn dismissed(&mut self, _window: &mut Window, cx: &mut Context<Picker<Self>>) {
        self.modal.update(cx, |_, cx| cx.emit(DismissEvent)).ok();
    }

    fn render_match(
        &self,
        ix: usize,
        selected: bool,
        _window: &mut Window,
        _cx: &mut Context<Picker<Self>>,
    ) -> Option<Self::ListItem> {
        let entry = self.matches.get(ix)?;
        let item = ListItem::new(ix)
            .inset(true)
            .spacing(ListItemSpacing::Sparse)
            .toggle_state(selected);
        let item = match entry {
            Entry::Target(target) => item
                .start_slot(Icon::new(IconName::Download).color(Color::Accent))
                .child(
                    v_flex()
                        .child(Label::new(format!("Clone {}", target.display)).truncate())
                        .child(
                            Label::new(format!(
                                "into {}",
                                self.parent_dir.join(&target.directory_name).display()
                            ))
                            .size(LabelSize::Small)
                            .color(Color::Muted)
                            .truncate(),
                        ),
                ),
            Entry::Repo { ix, positions } => {
                let repo = self.repos.get(*ix)?;
                let mut tags = Vec::new();
                if repo.private {
                    tags.push("private");
                }
                if repo.fork {
                    tags.push("fork");
                }
                if repo.archived {
                    tags.push("archived");
                }
                let description = repo
                    .description
                    .clone()
                    .filter(|description| !description.trim().is_empty());
                item.start_slot(
                    Icon::new(if repo.private {
                        IconName::Lock
                    } else {
                        IconName::Github
                    })
                    .color(Color::Muted),
                )
                .child(
                    v_flex()
                        .child(HighlightedLabel::new(
                            repo.full_name.clone(),
                            positions.clone(),
                        ))
                        .when_some(description, |this, description| {
                            this.child(
                                Label::new(description)
                                    .size(LabelSize::Small)
                                    .color(Color::Muted)
                                    .truncate(),
                            )
                        }),
                )
                .when(!tags.is_empty(), |this| {
                    this.end_slot(
                        Label::new(tags.join(" · "))
                            .size(LabelSize::XSmall)
                            .color(Color::Muted),
                    )
                })
            }
            Entry::AddAccount => item
                .start_slot(Icon::new(IconName::Plus).color(Color::Muted))
                .child(
                    v_flex().child(Label::new("Add a GitHub account…")).child(
                        Label::new("to list and clone your private repositories")
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    ),
                ),
        };
        Some(item.into_any_element())
    }

    fn render_footer(
        &self,
        _window: &mut Window,
        cx: &mut Context<Picker<Self>>,
    ) -> Option<AnyElement> {
        let colors = cx.theme().colors();
        let account = self.account_label(cx);
        let problem =
            GithubAccountStore::global(cx).and_then(|store| store.read(cx).problem().cloned());
        let account_text: SharedString = match &account {
            Some(login) => format!("GitHub: {login}").into(),
            None => "No GitHub account — public repos, or your git credential manager".into(),
        };
        let parent = self.parent_dir.display().to_string();
        Some(
            v_flex()
                .w_full()
                .p_1p5()
                .gap_1()
                .border_t_1()
                .border_color(colors.border_variant)
                .child(
                    h_flex()
                        .w_full()
                        .gap_1p5()
                        .justify_between()
                        .child(
                            h_flex()
                                .gap_1p5()
                                .min_w_0()
                                .child(
                                    Icon::new(IconName::Github)
                                        .size(IconSize::Small)
                                        .color(Color::Muted),
                                )
                                .child(
                                    Label::new(account_text)
                                        .size(LabelSize::Small)
                                        .color(Color::Muted)
                                        .truncate(),
                                ),
                        )
                        .child(self.render_account_menu()),
                )
                .when_some(problem, |this, problem| {
                    this.child(
                        Label::new(problem)
                            .size(LabelSize::XSmall)
                            .color(Color::Warning),
                    )
                })
                .child(
                    h_flex()
                        .w_full()
                        .gap_1p5()
                        .justify_between()
                        .child(
                            h_flex()
                                .gap_1p5()
                                .min_w_0()
                                .child(
                                    Icon::new(IconName::Folder)
                                        .size(IconSize::Small)
                                        .color(Color::Muted),
                                )
                                .child(
                                    Label::new(format!("Clone into {parent}"))
                                        .size(LabelSize::Small)
                                        .color(Color::Muted)
                                        .truncate(),
                                ),
                        )
                        .child(
                            Button::new("clone-change-folder", "Change…")
                                .style(ButtonStyle::Subtle)
                                .label_size(LabelSize::Small)
                                .on_click(cx.listener(|picker, _, window, cx| {
                                    picker.delegate.choose_parent_dir(window, cx)
                                })),
                        ),
                )
                .into_any_element(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn target(url: &str, display: &str, directory_name: &str) -> Option<CloneTarget> {
        Some(CloneTarget {
            url: url.to_string(),
            display: display.to_string(),
            directory_name: directory_name.to_string(),
        })
    }

    #[test]
    fn parses_github_inputs() {
        let hello = target("https://github.com/octo/hello.git", "octo/hello", "hello");
        assert_eq!(parse_clone_input("https://github.com/octo/hello"), hello);
        assert_eq!(
            parse_clone_input("https://github.com/octo/hello.git"),
            hello
        );
        assert_eq!(parse_clone_input("https://github.com/octo/hello/"), hello);
        assert_eq!(
            parse_clone_input("https://github.com/octo/hello/tree/main/src"),
            hello
        );
        assert_eq!(parse_clone_input("github.com/octo/hello"), hello);
        assert_eq!(parse_clone_input("  octo/hello  "), hello);
        assert_eq!(
            parse_clone_input("octo/my.repo_2"),
            target(
                "https://github.com/octo/my.repo_2.git",
                "octo/my.repo_2",
                "my.repo_2"
            )
        );
    }

    #[test]
    fn parses_other_git_urls() {
        assert_eq!(
            parse_clone_input("git@github.com:octo/hello.git"),
            target(
                "git@github.com:octo/hello.git",
                "git@github.com:octo/hello.git",
                "hello"
            )
        );
        assert_eq!(
            parse_clone_input("https://gitlab.com/group/sub/project.git"),
            target(
                "https://gitlab.com/group/sub/project.git",
                "https://gitlab.com/group/sub/project.git",
                "project"
            )
        );
        assert_eq!(
            parse_clone_input("ssh://git@example.com:2222/team/app"),
            target(
                "ssh://git@example.com:2222/team/app",
                "ssh://git@example.com:2222/team/app",
                "app"
            )
        );
    }

    #[test]
    fn rejects_search_queries() {
        assert_eq!(parse_clone_input(""), None);
        assert_eq!(parse_clone_input("hello"), None);
        assert_eq!(parse_clone_input("hello world"), None);
        assert_eq!(parse_clone_input("a/b/c"), None);
        assert_eq!(parse_clone_input("https://github.com/octo"), None);
        assert_eq!(parse_clone_input("octo/.."), None);
    }

    #[test]
    fn friendly_clone_errors() {
        assert!(
            friendly_clone_error(
                "git clone failed: fatal: Authentication failed for 'https://github.com/o/r.git/'",
                Some("octocat")
            )
            .contains("GitHub rejected the token of octocat")
        );
        assert!(
            friendly_clone_error(
                "git clone failed: fatal: could not read Username for 'https://github.com': terminal prompts disabled",
                None
            )
            .contains("Add a GitHub account")
        );
        assert!(
            friendly_clone_error(
                "git clone failed: remote: Repository not found.",
                Some("octocat")
            )
            .contains("Signed in to GitHub as octocat")
        );
        assert!(
            friendly_clone_error(
                "git clone failed: fatal: unable to access 'https://github.com/o/r.git/': Could not resolve host: github.com",
                None
            )
            .contains("Can't reach")
        );
        assert!(
            friendly_clone_error(
                "git clone failed: fatal: destination path 'r' already exists and is not an empty directory.",
                None
            )
            .contains("already exists")
        );
        assert_eq!(
            friendly_clone_error("git clone failed: fatal: something odd", None),
            "Clone failed: something odd"
        );
    }
}
