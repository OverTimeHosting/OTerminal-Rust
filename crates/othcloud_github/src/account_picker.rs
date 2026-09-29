//! "Switch GitHub account": lists the GitHub accounts OTHCloud knows for the
//! user and makes OTerminal use the picked one for git.

use std::sync::Arc;

use gpui::{
    App, AppContext as _, Context, DismissEvent, Entity, EventEmitter, FocusHandle, Focusable,
    Render, SharedString, Task, WeakEntity, Window,
};
use othcloud_client::{GithubAccount, GithubTokenKind, OthcloudApi};
use picker::{Picker, PickerDelegate};
use ui::{IconButton, ListItem, ListItemSpacing, Tooltip, prelude::*};
use workspace::{ModalView, Workspace};

use crate::{ConnectGithub, GithubAccountStore, SignInToGithub, show_status};

pub struct GithubAccountPicker {
    picker: Entity<Picker<GithubAccountPickerDelegate>>,
}

impl GithubAccountPicker {
    pub fn new(
        workspace: WeakEntity<Workspace>,
        api: Arc<OthcloudApi>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let delegate =
            GithubAccountPickerDelegate::new(cx.entity().downgrade(), workspace, api.clone(), cx);
        let picker = cx.new(|cx| Picker::list(delegate, window, cx).initial_width(rems(34.)));
        let this = Self { picker };
        this.load_accounts(window, cx);
        this
    }

    fn load_accounts(&self, window: &mut Window, cx: &mut Context<Self>) {
        let picker = self.picker.clone();
        let api = picker.read(cx).delegate.api.clone();
        let load = cx.background_spawn(async move { api.github_accounts().await });
        let task = cx.spawn_in(window, async move |_, cx| {
            let result = load.await;
            picker
                .update_in(cx, |picker, window, cx| {
                    let delegate = &mut picker.delegate;
                    delegate.loading = false;
                    match result {
                        Ok(Some(response)) => {
                            delegate.accounts = Some(response.accounts);
                            delegate.connect_available = response.connect_available;
                            delegate.load_error = None;
                        }
                        Ok(None) => {
                            // An older OTHCloud: only the current account is known.
                            delegate.accounts = None;
                            delegate.connect_available = true;
                            delegate.load_error = None;
                        }
                        Err(error) => {
                            if error.is_unauthorized() {
                                handle_unauthorized(cx);
                            }
                            delegate.load_error =
                                Some(error.friendly_message(delegate.api.host()).into());
                        }
                    }
                    picker.refresh(window, cx);
                })
                .ok();
        });
        self.picker.update(cx, |picker, _| {
            picker.delegate._load_task = Some(task);
        });
    }
}

impl Render for GithubAccountPicker {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .key_context("GithubAccountPicker")
            .w(rems(34.))
            .child(self.picker.clone())
    }
}

impl Focusable for GithubAccountPicker {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.picker.focus_handle(cx)
    }
}

impl EventEmitter<DismissEvent> for GithubAccountPicker {}
impl ModalView for GithubAccountPicker {}

#[derive(Clone, Debug)]
enum Entry {
    Account(GithubAccount),
    /// The account in use, when the server can't list accounts.
    Current {
        label: String,
    },
    SignIn,
    ConnectOnWebsite,
}

pub struct GithubAccountPickerDelegate {
    picker: WeakEntity<GithubAccountPicker>,
    workspace: WeakEntity<Workspace>,
    api: Arc<OthcloudApi>,
    accounts: Option<Vec<GithubAccount>>,
    connect_available: bool,
    loading: bool,
    load_error: Option<SharedString>,
    /// The account id in use when the picker opened.
    active_id: Option<String>,
    current_label: Option<String>,
    matches: Vec<Entry>,
    selected_index: usize,
    _load_task: Option<Task<()>>,
}

impl GithubAccountPickerDelegate {
    fn new(
        picker: WeakEntity<GithubAccountPicker>,
        workspace: WeakEntity<Workspace>,
        api: Arc<OthcloudApi>,
        cx: &App,
    ) -> Self {
        let store = GithubAccountStore::global(cx);
        let (active_id, current_label) = store
            .map(|store| {
                let store = store.read(cx);
                (
                    store.active_account_id().map(ToString::to_string),
                    store.active_account().map(|active| active.display_name()),
                )
            })
            .unwrap_or_default();
        Self {
            picker,
            workspace,
            api,
            accounts: None,
            connect_available: true,
            loading: true,
            load_error: None,
            active_id,
            current_label,
            matches: Vec::new(),
            selected_index: 0,
            _load_task: None,
        }
    }

    fn all_entries(&self) -> Vec<Entry> {
        let mut entries = Vec::new();
        match &self.accounts {
            Some(accounts) => {
                entries.extend(accounts.iter().cloned().map(Entry::Account));
            }
            None if !self.loading => {
                if let Some(label) = &self.current_label {
                    entries.push(Entry::Current {
                        label: label.clone(),
                    });
                }
            }
            None => {}
        }
        entries.push(Entry::SignIn);
        if self.connect_available {
            entries.push(Entry::ConnectOnWebsite);
        }
        entries
    }

    fn remove_account(&mut self, account: GithubAccount, cx: &mut Context<Picker<Self>>) {
        let api = self.api.clone();
        let workspace = self.workspace.clone();
        let id = account.id.clone();
        let removal = cx.background_spawn(async move { api.remove_github_account(&id).await });
        cx.spawn(async move |picker, cx| {
            let result = removal.await;
            let message = match &result {
                Ok(()) => None,
                Err(error) if error.status == 409 => Some(
                    "That GitHub account is how you sign in to OTHCloud, so it can't be removed."
                        .to_string(),
                ),
                Err(error) if error.is_unauthorized() => {
                    cx.update(handle_unauthorized);
                    return;
                }
                Err(error) => {
                    let host = picker
                        .read_with(cx, |picker, _| picker.delegate.api.host().to_string())
                        .unwrap_or_default();
                    Some(error.friendly_message(&host))
                }
            };
            if let Some(message) = message {
                workspace
                    .update(cx, |workspace, cx| {
                        show_status(workspace, message, true, cx)
                    })
                    .ok();
                return;
            }

            // Removed: drop it from the list, and stop using it if it was active.
            let was_active = picker
                .update(cx, |picker, cx| {
                    let delegate = &mut picker.delegate;
                    if let Some(accounts) = delegate.accounts.as_mut() {
                        accounts.retain(|existing| existing.id != account.id);
                    }
                    let was_active = delegate.active_id.as_deref() == Some(account.id.as_str());
                    if was_active {
                        delegate.active_id = None;
                    }
                    delegate.matches = delegate.all_entries();
                    delegate.selected_index = delegate
                        .selected_index
                        .min(delegate.matches.len().saturating_sub(1));
                    cx.notify();
                    was_active
                })
                .unwrap_or(false);
            cx.update(|cx| {
                if was_active && let Some(store) = GithubAccountStore::global(cx) {
                    store.update(cx, |store, cx| store.use_default_account(cx));
                }
            });
            workspace
                .update(cx, |workspace, cx| {
                    show_status(
                        workspace,
                        format!("Removed GitHub account {}", account.label),
                        false,
                        cx,
                    )
                })
                .ok();
        })
        .detach();
    }

    fn switch_to(&self, account: GithubAccount, cx: &mut Context<Picker<Self>>) {
        let api = self.api.clone();
        let workspace = self.workspace.clone();
        let id = account.id.clone();
        let fetch = cx.background_spawn(async move { api.github_token(Some(&id)).await });
        let host = self.api.host().to_string();
        cx.spawn(async move |_, cx| match fetch.await {
            Ok(response) if !response.token.is_empty() => {
                cx.update(|cx| {
                    if let Some(store) = GithubAccountStore::global(cx) {
                        store.update(cx, |store, cx| {
                            store.use_account(account.id.clone(), response, cx)
                        });
                    }
                });
                workspace
                    .update(cx, |workspace, cx| {
                        show_status(
                            workspace,
                            format!("OTerminal now uses GitHub as {}", account.label),
                            false,
                            cx,
                        )
                    })
                    .ok();
            }
            Ok(_) => {
                workspace
                    .update(cx, |workspace, cx| {
                        show_status(
                            workspace,
                            "OTHCloud has no GitHub token for that account.",
                            true,
                            cx,
                        )
                    })
                    .ok();
            }
            Err(error) => {
                if error.is_unauthorized() {
                    cx.update(handle_unauthorized);
                    return;
                }
                let message = match error.status {
                    409 => format!(
                        "GitHub no longer accepts the token of {}. Sign in to it again.",
                        account.label
                    ),
                    404 => "That GitHub account is no longer on OTHCloud.".to_string(),
                    _ => error.friendly_message(&host),
                };
                workspace
                    .update(cx, |workspace, cx| {
                        show_status(workspace, message, true, cx)
                    })
                    .ok();
            }
        })
        .detach();
    }
}

fn handle_unauthorized(cx: &mut App) {
    if let Some(account) = othcloud_client::OthcloudAccount::global(cx) {
        account.update(cx, |account, cx| account.handle_unauthorized(cx));
    }
}

fn account_description(account: &GithubAccount) -> String {
    let mut description =
        if account.kind == GithubTokenKind::Installation || account.id.starts_with("app:") {
            "organization GitHub App".to_string()
        } else if account.can_push {
            "clone and push".to_string()
        } else {
            "read-only".to_string()
        };
    if account.kind == GithubTokenKind::Installation && !account.can_push {
        description.push_str(", read-only");
    }
    if account.deployable {
        description.push_str(" · deploys");
    }
    description
}

impl PickerDelegate for GithubAccountPickerDelegate {
    type ListItem = ListItem;

    fn name() -> &'static str {
        "github account picker"
    }

    fn placeholder_text(&self, _window: &mut Window, _cx: &mut App) -> Arc<str> {
        "Switch GitHub account…".into()
    }

    fn no_matches_text(&self, _window: &mut Window, _cx: &mut App) -> Option<SharedString> {
        if self.loading {
            Some("Loading GitHub accounts…".into())
        } else {
            self.load_error
                .clone()
                .or_else(|| Some("No GitHub accounts".into()))
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
        let entries = self.all_entries();
        self.matches = if query.is_empty() {
            entries
        } else {
            entries
                .into_iter()
                .filter(|entry| match entry {
                    Entry::Account(account) => {
                        account.label.to_lowercase().contains(&query)
                            || account
                                .login
                                .as_deref()
                                .is_some_and(|login| login.to_lowercase().contains(&query))
                    }
                    Entry::Current { label } => label.to_lowercase().contains(&query),
                    Entry::SignIn | Entry::ConnectOnWebsite => true,
                })
                .collect()
        };
        let active_ix = self.matches.iter().position(|entry| match entry {
            Entry::Account(account) => self.active_id.as_deref() == Some(account.id.as_str()),
            _ => false,
        });
        self.selected_index = if query.is_empty() {
            active_ix.unwrap_or(0)
        } else {
            0
        };
        Task::ready(())
    }

    fn confirm(&mut self, _secondary: bool, window: &mut Window, cx: &mut Context<Picker<Self>>) {
        let Some(entry) = self.matches.get(self.selected_index).cloned() else {
            return;
        };
        match entry {
            Entry::Account(account) => {
                if self.active_id.as_deref() != Some(account.id.as_str()) {
                    self.switch_to(account, cx);
                }
            }
            Entry::Current { .. } => {}
            Entry::SignIn => {
                let workspace = self.workspace.clone();
                window.defer(cx, move |window, cx| {
                    if let Some(workspace) = workspace.upgrade() {
                        window.focus(&workspace.focus_handle(cx), cx);
                    }
                    window.dispatch_action(Box::new(SignInToGithub), cx);
                });
            }
            Entry::ConnectOnWebsite => {
                window.dispatch_action(Box::new(ConnectGithub), cx);
            }
        }
        self.dismissed(window, cx);
    }

    fn dismissed(&mut self, _window: &mut Window, cx: &mut Context<Picker<Self>>) {
        self.picker.update(cx, |_, cx| cx.emit(DismissEvent)).ok();
    }

    fn render_match(
        &self,
        ix: usize,
        selected: bool,
        _window: &mut Window,
        cx: &mut Context<Picker<Self>>,
    ) -> Option<Self::ListItem> {
        let entry = self.matches.get(ix)?;
        let item = ListItem::new(ix)
            .inset(true)
            .spacing(ListItemSpacing::Sparse)
            .toggle_state(selected);
        let item = match entry {
            Entry::Account(account) => {
                let is_active = self.active_id.as_deref() == Some(account.id.as_str());
                let icon = if account.kind == GithubTokenKind::Installation {
                    IconName::Server
                } else {
                    IconName::Github
                };
                let mut item = item.start_slot(Icon::new(icon).color(Color::Muted)).child(
                    v_flex().child(Label::new(account.label.clone())).child(
                        Label::new(account_description(account))
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    ),
                );
                let removable = account.id.starts_with("user:");
                let end = h_flex()
                    .gap_1()
                    .when(is_active, |this| {
                        this.child(Icon::new(IconName::Check).color(Color::Accent))
                    })
                    .when(removable, |this| {
                        let account = account.clone();
                        this.child(
                            IconButton::new(("remove-github-account", ix), IconName::Trash)
                                .icon_size(IconSize::Small)
                                .icon_color(Color::Muted)
                                .tooltip(Tooltip::text("Remove this GitHub account from OTHCloud"))
                                .on_click(cx.listener(move |picker, _, _, cx| {
                                    cx.stop_propagation();
                                    picker.delegate.remove_account(account.clone(), cx);
                                })),
                        )
                    });
                item = item.end_slot(end);
                item
            }
            Entry::Current { label } => item
                .start_slot(Icon::new(IconName::Github).color(Color::Muted))
                .child(Label::new(label.clone()))
                .end_slot(Icon::new(IconName::Check).color(Color::Accent)),
            Entry::SignIn => item
                .start_slot(Icon::new(IconName::Plus).color(Color::Muted))
                .child(Label::new("Sign in to GitHub in OTerminal…")),
            Entry::ConnectOnWebsite => item
                .start_slot(Icon::new(IconName::ArrowUpRight).color(Color::Muted))
                .child(Label::new(
                    "Connect a GitHub account on the OTHCloud website…",
                )),
        };
        Some(item)
    }

    fn separators_after_indices(&self) -> Vec<usize> {
        let accounts = self
            .matches
            .iter()
            .take_while(|entry| matches!(entry, Entry::Account(_) | Entry::Current { .. }))
            .count();
        if accounts > 0 && accounts < self.matches.len() {
            vec![accounts - 1]
        } else {
            Vec::new()
        }
    }
}
