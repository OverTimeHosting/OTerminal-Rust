//! "Switch GitHub account": one list of the GitHub accounts stored on this PC
//! and the ones linked on OTHCloud, plus ways to add more. The picked account
//! is what OTerminal uses for git on github.com.

use std::sync::Arc;

use gpui::{
    App, AppContext as _, Context, DismissEvent, Entity, EventEmitter, FocusHandle, Focusable,
    Render, SharedString, Subscription, Task, WeakEntity, Window,
};
use othcloud_client::{GithubAccount, GithubTokenKind, OthcloudAccount};
use picker::{Picker, PickerDelegate};
use ui::{Avatar, IconButton, ListItem, ListItemSpacing, Tooltip, prelude::*};
use workspace::{ModalView, Workspace};

use crate::{
    AddGithubToken, ConnectGithub, GithubAccountStore, LocalGithubAccount, ResolvedAccount,
    SignInToGithub, handle_othcloud_unauthorized, oauth_client_id, othcloud_api,
    othcloud_signed_in, show_status,
};

pub struct GithubAccountPicker {
    picker: Entity<Picker<GithubAccountPickerDelegate>>,
    _subscription: Option<Subscription>,
}

impl GithubAccountPicker {
    pub fn new(
        workspace: WeakEntity<Workspace>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let delegate = GithubAccountPickerDelegate {
            picker: cx.entity().downgrade(),
            workspace,
            matches: Vec::new(),
            selected_index: 0,
        };
        let picker = cx.new(|cx| Picker::list(delegate, window, cx).initial_width(rems(36.)));
        let store = GithubAccountStore::global(cx);
        let subscription = store.as_ref().map(|store| {
            let picker = picker.clone();
            cx.subscribe_in(store, window, move |_, _, _, window, cx| {
                picker.update(cx, |picker, cx| picker.refresh(window, cx));
            })
        });
        if let Some(store) = store
            && othcloud_signed_in(cx)
        {
            store.update(cx, |store, cx| store.refresh_othcloud_accounts(cx));
        }
        Self {
            picker,
            _subscription: subscription,
        }
    }
}

impl Render for GithubAccountPicker {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .key_context("GithubAccountPicker")
            .w(rems(36.))
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
    Header(SharedString),
    /// A status line (loading, error) under a header.
    Note(SharedString),
    Local(LocalGithubAccount),
    Othcloud(GithubAccount),
    /// The OTHCloud account in use, when the server can't list accounts.
    OthcloudCurrent {
        label: String,
    },
    NoAccount,
    SignInWithBrowser,
    AddToken,
    ConnectOnWebsite,
    SignInToOthcloud,
}

impl Entry {
    fn is_selectable(&self) -> bool {
        !matches!(self, Entry::Header(_) | Entry::Note(_))
    }

    fn search_text(&self) -> Option<String> {
        match self {
            Entry::Local(account) => Some(format!(
                "{} {}",
                account.login,
                account.name.as_deref().unwrap_or_default()
            )),
            Entry::Othcloud(account) => Some(format!(
                "{} {}",
                account.label,
                account.login.as_deref().unwrap_or_default()
            )),
            Entry::OthcloudCurrent { label } => Some(label.clone()),
            _ => None,
        }
    }
}

pub struct GithubAccountPickerDelegate {
    picker: WeakEntity<GithubAccountPicker>,
    workspace: WeakEntity<Workspace>,
    matches: Vec<Entry>,
    selected_index: usize,
}

fn github_id_string(value: &serde_json::Value) -> Option<String> {
    match value {
        serde_json::Value::String(id) if !id.is_empty() => Some(id.clone()),
        serde_json::Value::Number(id) => Some(id.to_string()),
        _ => None,
    }
}

impl GithubAccountPickerDelegate {
    fn all_entries(&self, cx: &App) -> Vec<Entry> {
        let mut entries = Vec::new();
        let Some(store) = GithubAccountStore::global(cx) else {
            return entries;
        };
        let store = store.read(cx);
        let signed_in = othcloud_signed_in(cx);

        entries.push(Entry::Header("On this PC".into()));
        let local_ids: Vec<String> = store
            .local_accounts()
            .iter()
            .map(|account| account.id.to_string())
            .collect();
        entries.extend(store.local_accounts().iter().cloned().map(Entry::Local));
        if oauth_client_id(cx).is_some() {
            entries.push(Entry::SignInWithBrowser);
        }
        entries.push(Entry::AddToken);

        entries.push(Entry::Header("OTHCloud".into()));
        if signed_in {
            match store.othcloud_accounts() {
                Some(accounts) => {
                    // An account saved both here and on OTHCloud shows once,
                    // as the local one.
                    let accounts = accounts.iter().filter(|account| {
                        !(account.kind == GithubTokenKind::User
                            && github_id_string(&account.github_id)
                                .is_some_and(|id| local_ids.contains(&id)))
                    });
                    entries.extend(accounts.cloned().map(Entry::Othcloud));
                }
                None if store.othcloud_accounts_loading() => {
                    entries.push(Entry::Note("Loading OTHCloud accounts…".into()));
                }
                None => {
                    if let Some(active) = store.active_account().filter(|active| !active.is_local())
                    {
                        entries.push(Entry::OthcloudCurrent {
                            label: active.display_name(),
                        });
                    }
                }
            }
            if let Some(error) = store.othcloud_accounts_error() {
                entries.push(Entry::Note(error.clone()));
            }
            if store.othcloud_connect_available() {
                entries.push(Entry::ConnectOnWebsite);
            }
        } else {
            entries.push(Entry::SignInToOthcloud);
        }

        entries.push(Entry::Header("Without an account".into()));
        entries.push(Entry::NoAccount);
        entries
    }

    fn is_active(&self, entry: &Entry, cx: &App) -> bool {
        let Some(store) = GithubAccountStore::global(cx) else {
            return false;
        };
        let store = store.read(cx);
        match (entry, store.resolved(cx)) {
            (Entry::Local(account), ResolvedAccount::Local(id)) => account.id == id,
            (Entry::Othcloud(account), ResolvedAccount::Othcloud) => {
                store.active_account_id() == Some(account.id.as_str())
            }
            (Entry::OthcloudCurrent { .. }, ResolvedAccount::Othcloud) => true,
            (Entry::NoAccount, ResolvedAccount::None) => true,
            _ => false,
        }
    }

    fn toast(&self, message: impl Into<SharedString>, is_error: bool, cx: &mut App) {
        let message = message.into();
        self.workspace
            .update(cx, |workspace, cx| {
                show_status(workspace, message, is_error, cx)
            })
            .ok();
    }
}

/// Fetches a token for the OTHCloud GitHub account `account` and makes it the
/// active account, reporting the outcome as a toast in `workspace`.
pub(crate) fn switch_to_othcloud_account(
    workspace: WeakEntity<Workspace>,
    account: GithubAccount,
    cx: &mut App,
) {
    let Some(api) = othcloud_api(cx) else {
        return;
    };
    let id = account.id.clone();
    let host = api.host().to_string();
    let fetch = cx.background_spawn(async move { api.github_token(Some(&id)).await });
    cx.spawn(async move |cx| {
        let message = match fetch.await {
            Ok(response) if !response.token.is_empty() => {
                cx.update(|cx| {
                    if let Some(store) = GithubAccountStore::global(cx) {
                        store.update(cx, |store, cx| {
                            store.use_othcloud_account(Some(account.id.clone()), Some(response), cx)
                        });
                    }
                });
                Ok(format!("OTerminal now uses GitHub as {}", account.label))
            }
            Ok(_) => Err("OTHCloud has no GitHub token for that account.".to_string()),
            Err(error) if error.is_unauthorized() => {
                cx.update(handle_othcloud_unauthorized);
                return;
            }
            Err(error) => Err(match error.status {
                409 => format!(
                    "GitHub no longer accepts the token of {}. Sign in to it again.",
                    account.label
                ),
                404 => "That GitHub account is no longer on OTHCloud.".to_string(),
                _ => error.friendly_message(&host),
            }),
        };
        workspace
            .update(cx, |workspace, cx| match message {
                Ok(message) => show_status(workspace, message, false, cx),
                Err(message) => show_status(workspace, message, true, cx),
            })
            .ok();
    })
    .detach();
}

impl GithubAccountPickerDelegate {
    fn remove_othcloud_account(&mut self, account: GithubAccount, cx: &mut Context<Picker<Self>>) {
        let Some(api) = othcloud_api(cx) else {
            return;
        };
        let workspace = self.workspace.clone();
        let host = api.host().to_string();
        let id = account.id.clone();
        let removal = cx.background_spawn(async move { api.remove_github_account(&id).await });
        cx.spawn(async move |_, cx| {
            let result = removal.await;
            let error = match &result {
                Ok(()) => None,
                Err(error) if error.status == 409 => Some(
                    "That GitHub account is how you sign in to OTHCloud, so it can't be removed."
                        .to_string(),
                ),
                Err(error) if error.is_unauthorized() => {
                    cx.update(handle_othcloud_unauthorized);
                    return;
                }
                Err(error) => Some(error.friendly_message(&host)),
            };
            if let Some(message) = error {
                workspace
                    .update(cx, |workspace, cx| {
                        show_status(workspace, message, true, cx)
                    })
                    .ok();
                return;
            }
            cx.update(|cx| {
                if let Some(store) = GithubAccountStore::global(cx) {
                    store.update(cx, |store, cx| {
                        let was_active = store.resolved(cx) == ResolvedAccount::Othcloud
                            && store.active_account_id() == Some(account.id.as_str());
                        if was_active {
                            store.use_othcloud_account(None, None, cx);
                        }
                        store.refresh_othcloud_accounts(cx);
                    });
                }
            });
            workspace
                .update(cx, |workspace, cx| {
                    show_status(
                        workspace,
                        format!("Removed GitHub account {} from OTHCloud", account.label),
                        false,
                        cx,
                    )
                })
                .ok();
        })
        .detach();
    }

    fn remove_local_account(
        &mut self,
        account: LocalGithubAccount,
        cx: &mut Context<Picker<Self>>,
    ) {
        if let Some(store) = GithubAccountStore::global(cx) {
            store
                .update(cx, |store, cx| store.remove_local_account(account.id, cx))
                .detach();
        }
        self.toast(
            format!(
                "Removed GitHub account {} and deleted its token from this PC",
                account.login
            ),
            false,
            cx,
        );
    }

    fn first_selectable(&self, from: usize) -> usize {
        self.matches
            .iter()
            .enumerate()
            .skip(from)
            .find(|(_, entry)| entry.is_selectable())
            .map_or(0, |(ix, _)| ix)
    }

    fn dispatch_after_dismiss(
        &self,
        action: Box<dyn gpui::Action>,
        window: &mut Window,
        cx: &mut Context<Picker<Self>>,
    ) {
        let workspace = self.workspace.clone();
        window.defer(cx, move |window, cx| {
            if let Some(workspace) = workspace.upgrade() {
                window.focus(&workspace.focus_handle(cx), cx);
            }
            window.dispatch_action(action, cx);
        });
    }
}

fn othcloud_account_description(account: &GithubAccount) -> String {
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

fn local_account_description(account: &LocalGithubAccount) -> String {
    let mut parts = Vec::new();
    if let Some(name) = account
        .name
        .as_deref()
        .filter(|name| *name != account.login)
    {
        parts.push(name.to_string());
    }
    parts.push(
        match account.source.as_deref() {
            Some("device") => "signed in with GitHub",
            _ => "personal access token",
        }
        .to_string(),
    );
    if account.saved_to_othcloud {
        parts.push("also on OTHCloud".to_string());
    }
    parts.join(" · ")
}

impl PickerDelegate for GithubAccountPickerDelegate {
    type ListItem = AnyElement;

    fn name() -> &'static str {
        "github account picker"
    }

    fn placeholder_text(&self, _window: &mut Window, _cx: &mut App) -> Arc<str> {
        "Switch GitHub account…".into()
    }

    fn no_matches_text(&self, _window: &mut Window, _cx: &mut App) -> Option<SharedString> {
        Some("No GitHub accounts match".into())
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

    fn can_select(&self, ix: usize, _window: &mut Window, _cx: &mut Context<Picker<Self>>) -> bool {
        self.matches.get(ix).is_some_and(Entry::is_selectable)
    }

    fn update_matches(
        &mut self,
        query: String,
        _window: &mut Window,
        cx: &mut Context<Picker<Self>>,
    ) -> Task<()> {
        let query = query.trim().to_lowercase();
        let entries = self.all_entries(cx);
        self.matches = if query.is_empty() {
            entries
        } else {
            entries
                .into_iter()
                .filter(|entry| {
                    entry
                        .search_text()
                        .is_some_and(|text| text.to_lowercase().contains(&query))
                })
                .collect()
        };
        let active_ix = self
            .matches
            .iter()
            .position(|entry| self.is_active(entry, cx));
        self.selected_index = match active_ix {
            Some(ix) if query.is_empty() => ix,
            _ => self.first_selectable(0),
        };
        Task::ready(())
    }

    fn confirm(&mut self, _secondary: bool, window: &mut Window, cx: &mut Context<Picker<Self>>) {
        let Some(entry) = self.matches.get(self.selected_index).cloned() else {
            return;
        };
        let already_active = self.is_active(&entry, cx);
        match entry {
            Entry::Header(_) | Entry::Note(_) => return,
            Entry::Local(account) => {
                if !already_active && let Some(store) = GithubAccountStore::global(cx) {
                    store.update(cx, |store, cx| store.use_local_account(account.id, cx));
                    self.toast(
                        format!("OTerminal now uses GitHub as {}", account.login),
                        false,
                        cx,
                    );
                }
            }
            Entry::Othcloud(account) => {
                if !already_active {
                    switch_to_othcloud_account(self.workspace.clone(), account, cx);
                }
            }
            Entry::OthcloudCurrent { .. } => {
                if !already_active && let Some(store) = GithubAccountStore::global(cx) {
                    store.update(cx, |store, cx| store.use_othcloud_account(None, None, cx));
                }
            }
            Entry::NoAccount => {
                if !already_active && let Some(store) = GithubAccountStore::global(cx) {
                    store.update(cx, |store, cx| store.use_no_account(cx));
                    self.toast(
                        "Git now uses your own credential manager for GitHub",
                        false,
                        cx,
                    );
                }
            }
            Entry::SignInWithBrowser => {
                self.dispatch_after_dismiss(Box::new(SignInToGithub), window, cx)
            }
            Entry::AddToken => self.dispatch_after_dismiss(Box::new(AddGithubToken), window, cx),
            Entry::ConnectOnWebsite => {
                self.dispatch_after_dismiss(Box::new(ConnectGithub), window, cx)
            }
            Entry::SignInToOthcloud => {
                if let Some(account) = OthcloudAccount::global(cx) {
                    account.update(cx, |account, cx| account.begin_sign_in(cx));
                }
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
        if let Entry::Header(title) = entry {
            return Some(
                div()
                    .px_2p5()
                    .pt_2()
                    .pb_0p5()
                    .child(
                        Label::new(title.clone())
                            .size(LabelSize::XSmall)
                            .color(Color::Muted),
                    )
                    .into_any_element(),
            );
        }
        if let Entry::Note(note) = entry {
            return Some(
                div()
                    .px_2p5()
                    .py_1()
                    .child(
                        Label::new(note.clone())
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    )
                    .into_any_element(),
            );
        }

        let is_active = self.is_active(entry, cx);
        let check = || Icon::new(IconName::Check).color(Color::Accent);
        let item = ListItem::new(ix)
            .inset(true)
            .spacing(ListItemSpacing::Sparse)
            .toggle_state(selected);
        let two_lines = |title: SharedString, subtitle: String| {
            v_flex().child(Label::new(title)).child(
                Label::new(subtitle)
                    .size(LabelSize::Small)
                    .color(Color::Muted),
            )
        };
        let item = match entry {
            Entry::Header(_) | Entry::Note(_) => return None,
            Entry::Local(account) => {
                let start = match account.avatar_url.clone() {
                    Some(url) => Avatar::new(url).size(rems(1.25)).into_any_element(),
                    None => Icon::new(IconName::Github)
                        .color(Color::Muted)
                        .into_any_element(),
                };
                let account_for_remove = account.clone();
                item.start_slot(start)
                    .child(two_lines(
                        account.login.clone().into(),
                        local_account_description(account),
                    ))
                    .end_slot(
                        h_flex()
                            .gap_1()
                            .when(is_active, |this| this.child(check()))
                            .child(
                                IconButton::new(
                                    ("remove-local-github-account", ix),
                                    IconName::Trash,
                                )
                                .icon_size(IconSize::Small)
                                .icon_color(Color::Muted)
                                .tooltip(Tooltip::text(
                                    "Remove this account and delete its token from this PC",
                                ))
                                .on_click(cx.listener(
                                    move |picker, _, _, cx| {
                                        cx.stop_propagation();
                                        picker
                                            .delegate
                                            .remove_local_account(account_for_remove.clone(), cx);
                                    },
                                )),
                            ),
                    )
            }
            Entry::Othcloud(account) => {
                let start = match account.avatar_url.clone() {
                    Some(url) => Avatar::new(url).size(rems(1.25)).into_any_element(),
                    None => Icon::new(if account.kind == GithubTokenKind::Installation {
                        IconName::Server
                    } else {
                        IconName::Github
                    })
                    .color(Color::Muted)
                    .into_any_element(),
                };
                let removable = account.id.starts_with("user:");
                let account_for_remove = account.clone();
                item.start_slot(start)
                    .child(two_lines(
                        account.label.clone().into(),
                        othcloud_account_description(account),
                    ))
                    .end_slot(
                        h_flex()
                            .gap_1()
                            .when(is_active, |this| this.child(check()))
                            .when(removable, |this| {
                                this.child(
                                    IconButton::new(
                                        ("remove-othcloud-github-account", ix),
                                        IconName::Trash,
                                    )
                                    .icon_size(IconSize::Small)
                                    .icon_color(Color::Muted)
                                    .tooltip(Tooltip::text(
                                        "Remove this GitHub account from OTHCloud",
                                    ))
                                    .on_click(cx.listener(
                                        move |picker, _, _, cx| {
                                            cx.stop_propagation();
                                            picker.delegate.remove_othcloud_account(
                                                account_for_remove.clone(),
                                                cx,
                                            );
                                        },
                                    )),
                                )
                            }),
                    )
            }
            Entry::OthcloudCurrent { label } => item
                .start_slot(Icon::new(IconName::Github).color(Color::Muted))
                .child(two_lines(label.clone().into(), "via OTHCloud".to_string()))
                .when(is_active, |this| this.end_slot(check())),
            Entry::NoAccount => item
                .start_slot(Icon::new(IconName::Close).color(Color::Muted))
                .child(two_lines(
                    "No account".into(),
                    "Git uses your own credential manager for GitHub".to_string(),
                ))
                .when(is_active, |this| this.end_slot(check())),
            Entry::SignInWithBrowser => item
                .start_slot(Icon::new(IconName::Github).color(Color::Muted))
                .child(Label::new("Sign in with GitHub in the browser…")),
            Entry::AddToken => item
                .start_slot(Icon::new(IconName::Plus).color(Color::Muted))
                .child(Label::new("Add a GitHub account with a token…")),
            Entry::ConnectOnWebsite => item
                .start_slot(Icon::new(IconName::ArrowUpRight).color(Color::Muted))
                .child(Label::new(
                    "Connect a GitHub account on the OTHCloud website…",
                )),
            Entry::SignInToOthcloud => item
                .start_slot(Icon::new(IconName::ArrowUpRight).color(Color::Muted))
                .child(two_lines(
                    "Sign in to OTHCloud…".into(),
                    "Use the GitHub accounts linked to your OTHCloud account".to_string(),
                )),
        };
        Some(item.into_any_element())
    }
}
