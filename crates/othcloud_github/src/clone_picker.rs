//! "Clone from GitHub": lists the repositories the current GitHub token can
//! reach, clones the picked one into `~/Documents/GitHub` and opens it as a new
//! project tab.

use std::{path::PathBuf, sync::Arc};

use anyhow::{Context as _, Result};
use futures::AsyncReadExt as _;
use gpui::{
    App, AppContext as _, Context, DismissEvent, Entity, EventEmitter, FocusHandle, Focusable,
    Render, SharedString, Task, WeakEntity, Window,
};
use http_client::{AsyncBody, HttpClient, Request};
use picker::{Picker, PickerDelegate};
use serde::Deserialize;
use ui::{ListItem, ListItemSpacing, prelude::*};
use util::ResultExt as _;
use workspace::{ModalView, OpenMode, Workspace};

use crate::{show_progress, show_status};

const GITHUB_API: &str = "https://api.github.com";
const PER_PAGE: usize = 100;
const MAX_PAGES: usize = 5;

#[derive(Clone, Debug, Deserialize)]
pub struct GithubRepo {
    pub full_name: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub private: bool,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub clone_url: Option<String>,
}

impl GithubRepo {
    fn clone_url(&self) -> String {
        self.clone_url
            .clone()
            .unwrap_or_else(|| format!("{}/{}.git", crate::GITHUB_HOST_URL, self.full_name))
    }

    fn directory_name(&self) -> String {
        if !self.name.is_empty() {
            return self.name.clone();
        }
        self.full_name
            .rsplit('/')
            .next()
            .unwrap_or(&self.full_name)
            .to_string()
    }
}

pub struct CloneRepoPicker {
    picker: Entity<Picker<CloneRepoPickerDelegate>>,
}

impl CloneRepoPicker {
    pub fn new(
        workspace: WeakEntity<Workspace>,
        http: Arc<dyn HttpClient>,
        token: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let delegate = CloneRepoPickerDelegate {
            modal: cx.entity().downgrade(),
            workspace,
            repos: Vec::new(),
            matches: Vec::new(),
            selected_index: 0,
            loading: true,
            error: None,
            _load_task: None,
        };
        let picker = cx.new(|cx| Picker::list(delegate, window, cx).initial_width(rems(38.)));

        let load = cx.background_spawn(async move { fetch_repos(http, &token).await });
        let picker_handle = picker.clone();
        let task = cx.spawn_in(window, async move |_, cx| {
            let result = load.await;
            picker_handle
                .update_in(cx, |picker, window, cx| {
                    picker.delegate.loading = false;
                    match result {
                        Ok(repos) => picker.delegate.repos = repos,
                        Err(error) => {
                            log::warn!("listing GitHub repositories failed: {error:#}");
                            picker.delegate.error =
                                Some("Couldn't list your GitHub repositories.".into());
                        }
                    }
                    picker.refresh(window, cx);
                })
                .ok();
        });
        picker.update(cx, |picker, _| picker.delegate._load_task = Some(task));
        Self { picker }
    }
}

impl Render for CloneRepoPicker {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .key_context("CloneRepoPicker")
            .w(rems(38.))
            .child(self.picker.clone())
    }
}

impl Focusable for CloneRepoPicker {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.picker.focus_handle(cx)
    }
}

impl EventEmitter<DismissEvent> for CloneRepoPicker {}
impl ModalView for CloneRepoPicker {}

pub struct CloneRepoPickerDelegate {
    modal: WeakEntity<CloneRepoPicker>,
    workspace: WeakEntity<Workspace>,
    repos: Vec<GithubRepo>,
    matches: Vec<usize>,
    selected_index: usize,
    loading: bool,
    error: Option<SharedString>,
    _load_task: Option<Task<()>>,
}

impl PickerDelegate for CloneRepoPickerDelegate {
    type ListItem = ListItem;

    fn name() -> &'static str {
        "clone from github"
    }

    fn placeholder_text(&self, _window: &mut Window, _cx: &mut App) -> Arc<str> {
        "Clone a GitHub repository…".into()
    }

    fn no_matches_text(&self, _window: &mut Window, _cx: &mut App) -> Option<SharedString> {
        if self.loading {
            Some("Loading repositories…".into())
        } else if let Some(error) = &self.error {
            Some(error.clone())
        } else {
            Some("No repositories".into())
        }
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
        _window: &mut Window,
        _cx: &mut Context<Picker<Self>>,
    ) -> Task<()> {
        let query = query.trim().to_lowercase();
        self.matches = self
            .repos
            .iter()
            .enumerate()
            .filter(|(_, repo)| query.is_empty() || repo.full_name.to_lowercase().contains(&query))
            .map(|(ix, _)| ix)
            .collect();
        self.selected_index = 0;
        Task::ready(())
    }

    fn confirm(&mut self, _secondary: bool, window: &mut Window, cx: &mut Context<Picker<Self>>) {
        let Some(repo) = self
            .matches
            .get(self.selected_index)
            .and_then(|ix| self.repos.get(*ix))
            .cloned()
        else {
            return;
        };
        clone_and_open_tab(self.workspace.clone(), repo, cx);
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
        let repo = self.repos.get(*self.matches.get(ix)?)?;
        Some(
            ListItem::new(ix)
                .inset(true)
                .spacing(ListItemSpacing::Sparse)
                .toggle_state(selected)
                .start_slot(
                    Icon::new(if repo.private {
                        IconName::Lock
                    } else {
                        IconName::Github
                    })
                    .color(Color::Muted),
                )
                .child(
                    v_flex()
                        .child(Label::new(repo.full_name.clone()))
                        .when_some(
                            repo.description.clone().filter(|d| !d.trim().is_empty()),
                            |this, description| {
                                this.child(
                                    Label::new(description)
                                        .size(LabelSize::Small)
                                        .color(Color::Muted)
                                        .truncate(),
                                )
                            },
                        ),
                ),
        )
    }
}

/// `~/Documents/GitHub`, where OTerminal clones repositories.
pub fn default_clone_directory() -> PathBuf {
    util::paths::home_dir().join("Documents").join("GitHub")
}

fn clone_and_open_tab(workspace: WeakEntity<Workspace>, repo: GithubRepo, cx: &mut App) {
    let Some(fs) = workspace
        .read_with(cx, |workspace, _| workspace.app_state().fs.clone())
        .ok()
    else {
        return;
    };
    let parent = default_clone_directory();
    let destination = parent.join(repo.directory_name());
    let url = repo.clone_url();
    let full_name = repo.full_name.clone();

    workspace
        .update(cx, |workspace, cx| {
            show_progress(workspace, format!("Cloning {full_name}…"), cx)
        })
        .ok();

    cx.spawn(async move |cx| {
        let result: Result<()> = async {
            fs.create_dir(&parent)
                .await
                .with_context(|| format!("creating {}", parent.display()))?;
            if fs.is_dir(&destination).await {
                // Already cloned: just open it.
                return Ok(());
            }
            fs.git_clone(&parent, &url).await
        }
        .await;

        if let Err(error) = result {
            workspace
                .update(cx, |workspace, cx| {
                    show_status(workspace, format!("{error:#}"), true, cx)
                })
                .ok();
            return;
        }

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

async fn fetch_repos(http: Arc<dyn HttpClient>, token: &str) -> Result<Vec<GithubRepo>> {
    let mut repos = Vec::new();
    for page in 1..=MAX_PAGES {
        let url = format!(
            "{GITHUB_API}/user/repos?sort=pushed&per_page={PER_PAGE}&page={page}&affiliation=owner,collaborator,organization_member"
        );
        let batch: Vec<GithubRepo> = match github_get(&http, &url, token).await {
            Ok(batch) => batch,
            // Installation tokens can't call /user/repos.
            Err(error) if page == 1 => {
                log::debug!("GitHub /user/repos failed ({error:#}); trying installation repos");
                break;
            }
            Err(error) => return Err(error),
        };
        let done = batch.len() < PER_PAGE;
        repos.extend(batch);
        if done {
            break;
        }
    }

    if repos.is_empty() {
        #[derive(Deserialize)]
        struct InstallationRepos {
            #[serde(default)]
            repositories: Vec<GithubRepo>,
        }
        for page in 1..=MAX_PAGES {
            let url =
                format!("{GITHUB_API}/installation/repositories?per_page={PER_PAGE}&page={page}");
            let batch = match github_get::<InstallationRepos>(&http, &url, token).await {
                Ok(batch) => batch.repositories,
                Err(error) if page == 1 && repos.is_empty() => {
                    log::debug!("GitHub /installation/repositories failed: {error:#}");
                    break;
                }
                Err(error) => return Err(error),
            };
            let done = batch.len() < PER_PAGE;
            repos.extend(batch);
            if done {
                break;
            }
        }
    }
    Ok(repos)
}

async fn github_get<T: for<'de> Deserialize<'de>>(
    http: &Arc<dyn HttpClient>,
    url: &str,
    token: &str,
) -> Result<T> {
    let request = Request::get(url)
        .header("Authorization", format!("Bearer {token}"))
        .header("Accept", "application/vnd.github+json")
        .header("X-GitHub-Api-Version", "2022-11-28")
        .header("User-Agent", "OTerminal")
        .body(AsyncBody::default())?;
    let mut response = http.send(request).await?;
    let mut body = Vec::new();
    response.body_mut().read_to_end(&mut body).await?;
    anyhow::ensure!(
        response.status().is_success(),
        "GitHub returned {} for {url}",
        response.status().as_u16()
    );
    Ok(serde_json::from_slice(&body)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repo_names_and_urls() {
        let repos: Vec<GithubRepo> = serde_json::from_str(
            r#"[{ "full_name": "octo/hello", "name": "hello", "private": true, "clone_url": "https://github.com/octo/hello.git" },
                { "full_name": "octo/world" }]"#,
        )
        .expect("parse");
        assert_eq!(repos[0].directory_name(), "hello");
        assert_eq!(repos[0].clone_url(), "https://github.com/octo/hello.git");
        assert_eq!(repos[1].directory_name(), "world");
        assert_eq!(repos[1].clone_url(), "https://github.com/octo/world.git");
    }
}
