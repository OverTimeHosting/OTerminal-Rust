//! Terminal profiles for OTerminal: keeps the "OTHCloud" section of the terminal
//! panel's new-terminal menu in sync with the profiles saved on OTHCloud, and
//! provides one-keystroke Claude Code sessions.
//!
//! Synced profiles are handed to the terminal panel at runtime through
//! [`terminal_panel::set_extra_terminal_profiles`]; nothing is ever written to the
//! user's settings files.

use std::{sync::Arc, time::Duration};

use gpui::{
    App, Context, DismissEvent, Global, Subscription, Task, WeakEntity, Window,
    actions,
};
use othcloud_client::{
    ApiError, NewTerminalProfile, OthcloudAccount, OthcloudAccountEvent, OthcloudApi,
    ServerTerminalProfile,
};
use picker::{Picker, PickerDelegate};
use terminal::terminal_settings::TerminalProfile;
use terminal_view::terminal_panel::{self, CLAUDE_CODE_PROFILE, NewTerminalWithProfile};
use ui::{ListItem, ListItemSpacing, prelude::*};
use workspace::{Toast, Workspace, notifications::NotificationId};

/// The terminal panel menu section that lists profiles synced from OTHCloud.
pub const OTHCLOUD_SECTION: &str = "OTHCloud";

/// How long to wait after a sign-in or user change before fetching profiles, so a
/// burst of account events results in a single request.
const SYNC_DEBOUNCE: Duration = Duration::from_millis(500);

actions!(
    othcloud,
    [
        /// Fetches the terminal profiles saved on OTHCloud and lists them in the
        /// terminal panel's new-terminal menu.
        SyncTerminalProfiles,
        /// Opens a new Claude Code session in the terminal panel.
        NewClaudeCodeSession,
        /// Saves one of the local terminal profiles to OTHCloud.
        SaveTerminalProfileToOthcloud,
        /// Removes one of the synced terminal profiles from OTHCloud.
        RemoveTerminalProfileFromOthcloud,
    ]
);

/// Process-wide sync state.
struct ProfileSync {
    account_subscription: Option<Subscription>,
    pending_sync: Task<()>,
    /// `(server id, name)` of the profiles from the last successful sync.
    synced: Vec<(String, String)>,
}

impl Global for ProfileSync {}

pub fn init(cx: &mut App) {
    cx.set_global(ProfileSync {
        account_subscription: None,
        pending_sync: Task::ready(()),
        synced: Vec::new(),
    });
    subscribe_to_account(cx);

    cx.observe_new(
        |workspace: &mut Workspace, _window, cx: &mut Context<Workspace>| {
            // The account may be created after this crate is initialized.
            subscribe_to_account(cx);

            workspace.register_action(|workspace, _: &SyncTerminalProfiles, _, cx| {
                sync(Some(workspace.weak_handle()), cx).detach();
            });
            workspace.register_action(new_claude_code_session);
            workspace.register_action(save_profile_to_othcloud);
            workspace.register_action(remove_profile_from_othcloud);
        },
    )
    .detach();
}

fn subscribe_to_account(cx: &mut App) {
    if cx
        .try_global::<ProfileSync>()
        .is_none_or(|state| state.account_subscription.is_some())
    {
        return;
    }
    let Some(account) = OthcloudAccount::global(cx) else {
        return;
    };

    let subscription = cx.subscribe(
        &account,
        |_, event: &OthcloudAccountEvent, cx| match event {
            OthcloudAccountEvent::SignedIn | OthcloudAccountEvent::UserChanged => {
                schedule_sync(cx);
            }
            OthcloudAccountEvent::SignedOut => {
                let state = cx.global_mut::<ProfileSync>();
                state.pending_sync = Task::ready(());
                state.synced.clear();
                terminal_panel::set_extra_terminal_profiles(OTHCLOUD_SECTION, Vec::new(), cx);
            }
            OthcloudAccountEvent::ServicesChanged => {}
        },
    );
    cx.global_mut::<ProfileSync>().account_subscription = Some(subscription);

    if account.read(cx).is_signed_in() {
        schedule_sync(cx);
    }
}

/// Syncs profiles after a short delay, replacing any sync scheduled earlier.
fn schedule_sync(cx: &mut App) {
    let task = cx.spawn(async move |cx| {
        cx.background_executor().timer(SYNC_DEBOUNCE).await;
        let sync = cx.update(|cx| sync(None, cx));
        sync.await;
    });
    cx.global_mut::<ProfileSync>().pending_sync = task;
}

/// Fetches the OTHCloud profiles for this platform and publishes them to the
/// terminal panel. When `notify` is set (a manual sync), the outcome is shown as a
/// toast in that workspace.
pub fn sync(notify: Option<WeakEntity<Workspace>>, cx: &mut App) -> Task<()> {
    let Some(account) = OthcloudAccount::global(cx) else {
        return Task::ready(());
    };
    let Some(api) = account.read(cx).api() else {
        cx.global_mut::<ProfileSync>().synced.clear();
        terminal_panel::set_extra_terminal_profiles(OTHCLOUD_SECTION, Vec::new(), cx);
        if let Some(workspace) = notify {
            show_toast(
                &workspace,
                "Sign in to OTHCloud to sync terminal profiles",
                cx,
            );
        }
        return Task::ready(());
    };

    cx.spawn(async move |cx| {
        let result = api.profiles().await;
        cx.update(|cx| match result {
            Ok(server_profiles) => {
                let synced = profiles_for_current_platform(server_profiles);
                let count = synced.len();
                cx.global_mut::<ProfileSync>().synced = synced
                    .iter()
                    .map(|(id, name, _)| (id.clone(), name.clone()))
                    .collect();
                terminal_panel::set_extra_terminal_profiles(
                    OTHCLOUD_SECTION,
                    synced
                        .into_iter()
                        .map(|(_, name, profile)| (name, profile))
                        .collect(),
                    cx,
                );
                if let Some(workspace) = notify {
                    let noun = if count == 1 { "profile" } else { "profiles" };
                    show_toast(
                        &workspace,
                        format!("Synced {count} terminal {noun} from OTHCloud"),
                        cx,
                    );
                }
            }
            Err(error) => {
                log::warn!("failed to sync OTHCloud terminal profiles: {error}");
                report_api_error(&account, &api, &error, notify.as_ref(), cx);
            }
        })
    })
}

/// Converts server profiles into terminal profiles: drops other platforms, orders
/// by `sortOrder`, removes unset env values and duplicate names.
/// Returns `(server id, name, profile)`.
fn profiles_for_current_platform(
    mut server_profiles: Vec<ServerTerminalProfile>,
) -> Vec<(String, String, TerminalProfile)> {
    server_profiles
        .retain(|profile| profile.applies_to_current_platform() && !profile.name.trim().is_empty());
    server_profiles.sort_by_key(|profile| profile.sort_order.unwrap_or(i64::MAX));

    let mut result: Vec<(String, String, TerminalProfile)> = Vec::new();
    for profile in server_profiles {
        let name = profile.name.trim().to_string();
        if result.iter().any(|(_, existing, _)| *existing == name) {
            continue;
        }
        let program = Some(profile.path.trim().to_string()).filter(|path| !path.is_empty());
        let env = profile
            .env
            .unwrap_or_default()
            .into_iter()
            .filter_map(|(key, value)| Some((key, value?)))
            .collect();
        result.push((
            profile.id,
            name,
            TerminalProfile {
                program,
                args: profile.args.unwrap_or_default(),
                env,
                working_directory: None,
                icon: profile.icon,
            },
        ));
    }
    result
}

fn new_claude_code_session(
    workspace: &mut Workspace,
    _: &NewClaudeCodeSession,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    // Call the handler directly rather than dispatching through the focus tree, so
    // this also works when nothing inside the workspace is focused. The handler
    // reveals and focuses the terminal panel.
    terminal_panel::TerminalPanel::new_terminal_with_profile(
        workspace,
        &NewTerminalWithProfile {
            profile: CLAUDE_CODE_PROFILE.to_string(),
        },
        window,
        cx,
    );
}

fn save_profile_to_othcloud(
    workspace: &mut Workspace,
    _: &SaveTerminalProfileToOthcloud,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let Some((account, api)) = signed_in_api(cx) else {
        show_toast(
            &workspace.weak_handle(),
            "Sign in to OTHCloud to save terminal profiles",
            cx,
        );
        return;
    };
    let items = terminal_panel::local_terminal_profile_names(cx)
        .into_iter()
        .map(|name| (name.clone(), name))
        .collect::<Vec<_>>();
    let weak_workspace = workspace.weak_handle();

    open_profile_picker(
        workspace,
        "Save a terminal profile to OTHCloud…",
        items,
        Box::new(move |name: String, _window: &mut Window, cx: &mut App| {
            let Some(profile) = terminal_panel::resolve_profile(&name, cx) else {
                return;
            };
            let new_profile = new_server_profile(name.clone(), profile);
            cx.spawn(async move |cx| {
                let result = api.save_profile(new_profile).await;
                cx.update(|cx| match result {
                    Ok(_) => {
                        show_toast(&weak_workspace, format!("Saved \"{name}\" to OTHCloud"), cx);
                        sync(None, cx).detach();
                    }
                    Err(error) => {
                        log::warn!("failed to save terminal profile {name:?}: {error}");
                        report_api_error(&account, &api, &error, Some(&weak_workspace), cx);
                    }
                })
            })
            .detach();
        }),
        window,
        cx,
    );
}

fn remove_profile_from_othcloud(
    workspace: &mut Workspace,
    _: &RemoveTerminalProfileFromOthcloud,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let Some((account, api)) = signed_in_api(cx) else {
        show_toast(
            &workspace.weak_handle(),
            "Sign in to OTHCloud to manage terminal profiles",
            cx,
        );
        return;
    };
    let items = cx
        .try_global::<ProfileSync>()
        .map(|state| {
            state
                .synced
                .iter()
                .map(|(id, name)| (name.clone(), id.clone()))
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    if items.is_empty() {
        show_toast(
            &workspace.weak_handle(),
            "No terminal profiles are synced from OTHCloud",
            cx,
        );
        return;
    }
    let weak_workspace = workspace.weak_handle();

    open_profile_picker(
        workspace,
        "Remove a terminal profile from OTHCloud…",
        items,
        Box::new(move |id: String, _window: &mut Window, cx: &mut App| {
            cx.spawn(async move |cx| {
                let result = api.delete_profile(&id).await;
                cx.update(|cx| match result {
                    Ok(()) => {
                        show_toast(&weak_workspace, "Removed the profile from OTHCloud", cx);
                        sync(None, cx).detach();
                    }
                    Err(error) => {
                        log::warn!("failed to remove terminal profile {id:?}: {error}");
                        report_api_error(&account, &api, &error, Some(&weak_workspace), cx);
                    }
                })
            })
            .detach();
        }),
        window,
        cx,
    );
}

/// Describes a local profile the way OTHCloud stores it. Profiles that point at an
/// absolute path only make sense on this platform; bare program names (like
/// `claude`) are shared with every platform.
fn new_server_profile(name: String, profile: TerminalProfile) -> NewTerminalProfile {
    let (path, platform) = match profile.program {
        Some(program) => {
            let platform = if std::path::Path::new(&program).is_absolute() {
                othcloud_client::current_platform()
            } else {
                "all"
            };
            (program, platform)
        }
        None => (
            util::shell::get_system_shell(),
            othcloud_client::current_platform(),
        ),
    };
    NewTerminalProfile {
        name,
        platform: platform.to_string(),
        path,
        args: Some(profile.args).filter(|args| !args.is_empty()),
        env: Some(
            profile
                .env
                .into_iter()
                .map(|(key, value)| (key, Some(value)))
                .collect(),
        )
        .filter(|env: &std::collections::HashMap<String, Option<String>>| !env.is_empty()),
        icon: profile.icon,
        color: None,
    }
}

fn signed_in_api(cx: &App) -> Option<(gpui::Entity<OthcloudAccount>, Arc<OthcloudApi>)> {
    let account = OthcloudAccount::global(cx)?;
    let api = account.read(cx).api()?;
    Some((account, api))
}

fn report_api_error(
    account: &gpui::Entity<OthcloudAccount>,
    api: &OthcloudApi,
    error: &ApiError,
    notify: Option<&WeakEntity<Workspace>>,
    cx: &mut App,
) {
    if error.is_unauthorized() {
        account.update(cx, |account, cx| account.handle_unauthorized(cx));
    }
    if let Some(workspace) = notify {
        show_toast(workspace, error.friendly_message(api.host()), cx);
    }
}

fn show_toast(
    workspace: &WeakEntity<Workspace>,
    message: impl Into<std::borrow::Cow<'static, str>>,
    cx: &mut App,
) {
    let message = message.into();
    workspace
        .update(cx, |workspace, cx| {
            workspace.show_toast(
                Toast::new(NotificationId::unique::<ProfileSync>(), message).autohide(),
                cx,
            );
        })
        .ok();
}

type OnConfirm = Box<dyn FnOnce(String, &mut Window, &mut App)>;

fn open_profile_picker(
    workspace: &mut Workspace,
    placeholder: &'static str,
    items: Vec<(String, String)>,
    on_confirm: OnConfirm,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let delegate = ProfilePickerDelegate {
        placeholder: placeholder.into(),
        matches: (0..items.len()).collect(),
        items,
        selected_index: 0,
        on_confirm: Some(on_confirm),
    };
    workspace.toggle_modal(window, cx, move |window, cx| {
        Picker::uniform_list(delegate, window, cx)
    });
}

/// A filterable list of profile names; confirming calls `on_confirm` with the
/// selected item's value.
struct ProfilePickerDelegate {
    placeholder: Arc<str>,
    /// `(label, value)` pairs.
    items: Vec<(String, String)>,
    /// Indices into `items` matching the current query.
    matches: Vec<usize>,
    selected_index: usize,
    on_confirm: Option<OnConfirm>,
}

impl PickerDelegate for ProfilePickerDelegate {
    type ListItem = ListItem;

    fn name() -> &'static str {
        "othcloud terminal profile picker"
    }

    fn placeholder_text(&self, _window: &mut Window, _cx: &mut App) -> Arc<str> {
        self.placeholder.clone()
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
        cx: &mut Context<Picker<Self>>,
    ) -> Task<()> {
        let query = query.trim().to_lowercase();
        self.matches = self
            .items
            .iter()
            .enumerate()
            .filter(|(_, (label, _))| query.is_empty() || label.to_lowercase().contains(&query))
            .map(|(ix, _)| ix)
            .collect();
        self.selected_index = self
            .selected_index
            .min(self.matches.len().saturating_sub(1));
        cx.notify();
        Task::ready(())
    }

    fn confirm(&mut self, _secondary: bool, window: &mut Window, cx: &mut Context<Picker<Self>>) {
        let Some(value) = self
            .matches
            .get(self.selected_index)
            .and_then(|ix| self.items.get(*ix))
            .map(|(_, value)| value.clone())
        else {
            return;
        };
        if let Some(on_confirm) = self.on_confirm.take() {
            on_confirm(value, window, cx);
        }
        cx.emit(DismissEvent);
    }

    fn dismissed(&mut self, _window: &mut Window, cx: &mut Context<Picker<Self>>) {
        cx.emit(DismissEvent);
    }

    fn render_match(
        &self,
        ix: usize,
        selected: bool,
        _window: &mut Window,
        _cx: &mut Context<Picker<Self>>,
    ) -> Option<Self::ListItem> {
        let (label, _) = self.items.get(*self.matches.get(ix)?)?;
        Some(
            ListItem::new(ix)
                .inset(true)
                .spacing(ListItemSpacing::Sparse)
                .toggle_state(selected)
                .start_slot(Icon::new(IconName::Terminal).color(Color::Muted))
                .child(Label::new(label.clone())),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn server_profile(
        id: &str,
        name: &str,
        platform: &str,
        sort: Option<i64>,
    ) -> ServerTerminalProfile {
        ServerTerminalProfile {
            id: id.to_string(),
            name: name.to_string(),
            platform: platform.to_string(),
            path: "claude".to_string(),
            sort_order: sort,
            ..Default::default()
        }
    }

    #[test]
    fn filters_sorts_and_dedupes_server_profiles() {
        let other_platform = if cfg!(windows) { "linux" } else { "windows" };
        let mut with_env = server_profile("1", "B", "all", Some(2));
        with_env.env = Some(
            [
                ("KEEP".to_string(), Some("1".to_string())),
                ("DROP".to_string(), None),
            ]
            .into_iter()
            .collect(),
        );
        let profiles = profiles_for_current_platform(vec![
            with_env,
            server_profile("2", "A", "all", Some(1)),
            server_profile("3", "Other", other_platform, Some(0)),
            server_profile("4", "A", "all", Some(3)),
        ]);
        let names = profiles
            .iter()
            .map(|(_, name, _)| name.as_str())
            .collect::<Vec<_>>();
        assert_eq!(names, ["A", "B"]);
        let env = &profiles[1].2.env;
        assert_eq!(env.get("KEEP").map(String::as_str), Some("1"));
        assert!(!env.contains_key("DROP"));
    }
}
