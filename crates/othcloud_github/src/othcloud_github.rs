//! GitHub through OTHCloud.
//!
//! OTHCloud hands out short-lived GitHub tokens for the signed-in user (their
//! own linked GitHub account, or their organization's GitHub App). This crate
//! keeps the current token fresh and installs it as a process-wide git
//! credential override (see [`git::credential_override`]), so every fetch,
//! pull, push and clone Zed runs authenticates with it — without touching the
//! user's git configuration or credential manager.
//!
//! It also provides the UI to switch between the user's GitHub accounts, to
//! sign in to another GitHub account with a token (saved on OTHCloud), to
//! connect a GitHub account on the OTHCloud website, and to clone one of the
//! user's repositories into a new project tab.

mod account_picker;
mod clone_picker;
mod token_modal;

use std::{
    sync::Arc,
    time::{Duration, Instant},
};

use chrono::{DateTime, Utc};
use db::kvp::KeyValueStore;
use gpui::{
    App, AppContext as _, Context, Entity, EventEmitter, Global, SharedString, Subscription, Task,
    WeakEntity, Window, actions,
};
use notifications::status_toast::StatusToast;
use othcloud_client::{
    ApiError, GithubTokenKind, GithubTokenResponse, OthcloudAccount, OthcloudAccountEvent,
    OthcloudApi,
};
use ui::{Color, Icon, IconName, IconSize};
use util::ResultExt as _;
use workspace::{
    Workspace,
    notifications::{NotificationId, simple_message_notification::MessageNotification},
};

pub use account_picker::GithubAccountPicker;
pub use clone_picker::CloneRepoPicker;
pub use token_modal::GithubTokenModal;

actions!(
    othcloud,
    [
        /// Connects your GitHub account on the OTHCloud website so OTerminal
        /// can clone and push as you.
        ConnectGithub,
        /// Switches the GitHub account OTerminal uses for git.
        SwitchGithubAccount,
        /// Signs in to a GitHub account with a token and saves it on OTHCloud.
        SignInToGithub,
        /// Clones one of your GitHub repositories into a new project tab.
        CloneFromGithub,
    ]
);

/// The OTHCloud page that links a GitHub account, when the server didn't say.
const DEFAULT_CONNECT_PATH: &str = "/desktop-github";
const CONNECT_OFFER_DISMISSED_KEY: &str = "othcloud.github.connectOfferDismissed";
/// How long [`ConnectGithub`] waits for the website flow to finish.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10 * 60);
const CONNECT_POLL_INTERVAL: Duration = Duration::from_secs(3);
/// Refresh this long before the token expires.
const REFRESH_MARGIN: Duration = Duration::from_secs(60);
/// Refresh interval when the server gave no (parsable) expiry.
const DEFAULT_REFRESH_INTERVAL: Duration = Duration::from_secs(50 * 60);
const MIN_REFRESH_INTERVAL: Duration = Duration::from_secs(30);
/// Retry interval after a transient failure.
const RETRY_INTERVAL: Duration = Duration::from_secs(2 * 60);
/// How often to look again when OTHCloud has no GitHub token for the user
/// (they may connect GitHub on the website outside of [`ConnectGithub`]).
const NO_TOKEN_RECHECK_INTERVAL: Duration = Duration::from_secs(10 * 60);

pub(crate) const GITHUB_HOST_URL: &str = "https://github.com";

fn selected_account_key(user_id: &str) -> String {
    format!("othcloud.github.selectedAccount.{user_id}")
}

pub fn init(cx: &mut App) {
    let store = cx.new(GithubAccountStore::new);
    cx.set_global(GlobalGithubAccountStore(store));

    cx.observe_new(|workspace: &mut Workspace, _window, _cx| {
        workspace
            .register_action(|workspace, _: &ConnectGithub, window, cx| {
                connect_github(workspace, window, cx);
            })
            .register_action(|workspace, _: &SwitchGithubAccount, window, cx| {
                switch_github_account(workspace, window, cx);
            })
            .register_action(|workspace, _: &SignInToGithub, window, cx| {
                sign_in_to_github(workspace, window, cx);
            })
            .register_action(|workspace, _: &CloneFromGithub, window, cx| {
                clone_from_github(workspace, window, cx);
            });
    })
    .detach();
}

/// The GitHub identity OTerminal currently uses for git.
#[derive(Clone, Debug)]
pub struct ActiveGithub {
    pub token: String,
    pub expires_at: Option<DateTime<Utc>>,
    pub login: Option<String>,
    /// The OTHCloud GitHub account id (`user:<id>` or `app:<id>`).
    pub account_id: Option<String>,
    pub app_name: Option<String>,
    pub kind: GithubTokenKind,
}

impl ActiveGithub {
    fn from_response(response: &GithubTokenResponse) -> Self {
        Self {
            token: response.token.clone(),
            expires_at: parse_expiry(response.expires_at.as_deref()),
            login: response.login.clone(),
            account_id: response.account_id.clone(),
            app_name: response.app_name.clone(),
            kind: response.kind,
        }
    }

    /// A short human-readable name for this identity.
    pub fn display_name(&self) -> String {
        match (self.kind, &self.login, &self.app_name) {
            (GithubTokenKind::User, Some(login), _) => login.clone(),
            (_, _, Some(app_name)) => app_name.clone(),
            (_, Some(login), _) => login.clone(),
            _ => "GitHub".to_string(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GithubAccountStoreEvent {
    /// The active GitHub identity (or its token) changed.
    Changed,
}

/// Keeps the GitHub token OTHCloud hands out fresh, and installs it as git's
/// credentials.
pub struct GithubAccountStore {
    user_id: Option<String>,
    /// The account the user picked, persisted per OTHCloud user. `None` means
    /// "whatever OTHCloud prefers" (their own account, else the org's app).
    selected: Option<String>,
    current: Option<ActiveGithub>,
    connect_path: Option<String>,
    connect_offer_shown: bool,
    refresh_task: Option<Task<()>>,
    refresh_timer: Option<Task<()>>,
    connect_task: Option<Task<()>>,
    _subscription: Option<Subscription>,
}

struct GlobalGithubAccountStore(Entity<GithubAccountStore>);

impl Global for GlobalGithubAccountStore {}

impl EventEmitter<GithubAccountStoreEvent> for GithubAccountStore {}

impl GithubAccountStore {
    pub fn global(cx: &App) -> Option<Entity<Self>> {
        cx.try_global::<GlobalGithubAccountStore>()
            .map(|global| global.0.clone())
    }

    fn new(cx: &mut Context<Self>) -> Self {
        let account = OthcloudAccount::global(cx);
        if account.is_none() {
            log::warn!(
                "othcloud_github initialized before othcloud_client; GitHub tokens disabled"
            );
        }
        let subscription = account
            .as_ref()
            .map(|account| cx.subscribe(account, Self::handle_account_event));
        let mut this = Self {
            user_id: None,
            selected: None,
            current: None,
            connect_path: None,
            connect_offer_shown: false,
            refresh_task: None,
            refresh_timer: None,
            connect_task: None,
            _subscription: subscription,
        };
        if account.is_some_and(|account| account.read(cx).is_signed_in()) {
            this.on_signed_in(cx);
        }
        this
    }

    pub fn active_account(&self) -> Option<&ActiveGithub> {
        self.current.as_ref()
    }

    pub fn current_token(&self) -> Option<String> {
        self.current.as_ref().map(|current| current.token.clone())
    }

    /// The account id the user explicitly picked, if any.
    pub fn selected_account_id(&self) -> Option<&str> {
        self.selected.as_deref()
    }

    /// The id of the account in use: the picked one, else the one OTHCloud chose.
    pub fn active_account_id(&self) -> Option<&str> {
        self.current
            .as_ref()
            .and_then(|current| current.account_id.as_deref())
            .or(self.selected.as_deref())
    }

    pub fn connect_path(&self) -> String {
        self.connect_path
            .clone()
            .unwrap_or_else(|| DEFAULT_CONNECT_PATH.to_string())
    }

    fn handle_account_event(
        &mut self,
        _account: Entity<OthcloudAccount>,
        event: &OthcloudAccountEvent,
        cx: &mut Context<Self>,
    ) {
        match event {
            OthcloudAccountEvent::SignedIn | OthcloudAccountEvent::UserChanged => {
                self.on_signed_in(cx)
            }
            OthcloudAccountEvent::SignedOut => self.clear(cx),
            _ => {}
        }
    }

    fn on_signed_in(&mut self, cx: &mut Context<Self>) {
        let user_id = OthcloudAccount::global(cx)
            .and_then(|account| account.read(cx).user().map(|user| user.id.clone()));
        let user_changed = user_id != self.user_id;
        if !user_changed && self.current.is_some() {
            return;
        }
        if user_changed {
            self.selected = user_id.as_deref().and_then(|user_id| {
                KeyValueStore::global(cx)
                    .read_kvp(&selected_account_key(user_id))
                    .log_err()
                    .flatten()
                    .filter(|id| !id.is_empty())
            });
            self.user_id = user_id;
        }
        self.refresh_token(cx);
    }

    fn clear(&mut self, cx: &mut Context<Self>) {
        self.user_id = None;
        self.selected = None;
        self.current = None;
        self.connect_offer_shown = false;
        self.refresh_task = None;
        self.refresh_timer = None;
        self.connect_task = None;
        git::set_git_credential_override(None);
        git::set_github_api_token(None);
        cx.emit(GithubAccountStoreEvent::Changed);
        cx.notify();
    }

    fn uninstall_token(&mut self, cx: &mut Context<Self>) {
        if self.current.take().is_some() {
            git::set_git_credential_override(None);
            git::set_github_api_token(None);
            cx.emit(GithubAccountStoreEvent::Changed);
            cx.notify();
        }
    }

    /// Remembers `account_id` as the user's choice (or forgets it with `None`).
    fn set_selected(&mut self, account_id: Option<String>, cx: &mut Context<Self>) {
        let account_id = account_id.filter(|id| !id.is_empty());
        if self.selected == account_id {
            return;
        }
        self.selected = account_id.clone();
        if let Some(user_id) = self.user_id.as_deref() {
            let key = selected_account_key(user_id);
            let kvp = KeyValueStore::global(cx);
            db::write_and_log(cx, move || async move {
                match account_id {
                    Some(id) => kvp.write_kvp(key, id).await,
                    None => kvp.delete_kvp(key).await,
                }
            });
        }
    }

    /// Selects `account_id` and installs `response`, a token already fetched
    /// for it. Used after the user picks or adds an account.
    pub fn use_account(
        &mut self,
        account_id: String,
        response: GithubTokenResponse,
        cx: &mut Context<Self>,
    ) {
        self.set_selected(Some(account_id), cx);
        self.install(response, cx);
    }

    /// Selects `account_id` and fetches a token for it.
    pub fn select_account(&mut self, account_id: String, cx: &mut Context<Self>) {
        self.set_selected(Some(account_id), cx);
        self.refresh_token(cx);
    }

    /// Forgets the user's choice and goes back to the account OTHCloud prefers.
    pub fn use_default_account(&mut self, cx: &mut Context<Self>) {
        self.set_selected(None, cx);
        self.refresh_token(cx);
    }

    /// Asks OTHCloud for a fresh token for the selected account (or its
    /// default) and installs it.
    pub fn refresh_token(&mut self, cx: &mut Context<Self>) {
        let Some(api) = othcloud_api(cx) else {
            self.uninstall_token(cx);
            return;
        };
        let selected = self.selected.clone();
        self.refresh_task = Some(cx.spawn(async move |this, cx| {
            let mut forget_selection = false;
            let mut result = None;
            if let Some(account_id) = selected {
                let request_api = api.clone();
                let response = cx
                    .background_spawn(
                        async move { request_api.github_token(Some(&account_id)).await },
                    )
                    .await;
                match response {
                    Ok(response) => result = Some(Ok(response)),
                    // The account is gone (404) or GitHub refuses its token
                    // (409): fall back to OTHCloud's default.
                    Err(error) if error.status == 404 || error.status == 409 => {
                        log::info!(
                            "selected GitHub account is unavailable ({error}); using default"
                        );
                        forget_selection = true;
                    }
                    Err(error) if error.is_unauthorized() => result = Some(Err(error)),
                    // Keep the selection; use the default until it answers again.
                    Err(error) => log::warn!("fetching the selected GitHub token failed: {error}"),
                }
            }
            let result = match result {
                Some(result) => result,
                None => {
                    let request_api = api.clone();
                    cx.background_spawn(async move { request_api.github_token(None).await })
                        .await
                }
            };
            this.update(cx, |this, cx| {
                if forget_selection {
                    this.set_selected(None, cx);
                }
                this.handle_token_result(result, api, cx);
            })
            .ok();
        }));
    }

    fn handle_token_result(
        &mut self,
        result: Result<GithubTokenResponse, ApiError>,
        api: Arc<OthcloudApi>,
        cx: &mut Context<Self>,
    ) {
        match result {
            Ok(response) if !response.token.is_empty() => {
                let needs_connect = response.needs_connect;
                self.install(response, cx);
                if needs_connect {
                    self.maybe_offer_connect(api, cx);
                }
            }
            Ok(response) => {
                // No token: nothing connected yet.
                if let Some(path) = response.connect_path {
                    self.connect_path = Some(path);
                }
                self.uninstall_token(cx);
                self.maybe_offer_connect(api, cx);
                self.schedule_refresh(NO_TOKEN_RECHECK_INTERVAL, cx);
            }
            Err(error) if error.is_unauthorized() => {
                if let Some(account) = OthcloudAccount::global(cx) {
                    account.update(cx, |account, cx| account.handle_unauthorized(cx));
                }
            }
            Err(error) if error.status == 404 => {
                // `no_github_connected`: neither a linked account nor an org app.
                self.uninstall_token(cx);
                self.maybe_offer_connect(api, cx);
                self.schedule_refresh(NO_TOKEN_RECHECK_INTERVAL, cx);
            }
            Err(error) => {
                log::warn!("fetching a GitHub token from OTHCloud failed: {error}");
                let still_valid = self.current.as_ref().is_some_and(|current| {
                    current
                        .expires_at
                        .is_none_or(|expires_at| expires_at > Utc::now())
                });
                if !still_valid {
                    self.uninstall_token(cx);
                }
                self.schedule_refresh(RETRY_INTERVAL, cx);
            }
        }
    }

    fn install(&mut self, response: GithubTokenResponse, cx: &mut Context<Self>) {
        if response.token.is_empty() {
            return;
        }
        if let Some(path) = response.connect_path.clone() {
            self.connect_path = Some(path);
        }
        let active = ActiveGithub::from_response(&response);
        git::set_git_credential_override(Some(Arc::new(git::GithubTokenCredentials {
            hosts: vec![GITHUB_HOST_URL.to_string()],
            token: active.token.clone(),
        })));
        git::set_github_api_token(Some(active.token.clone()));
        let delay = refresh_delay(active.expires_at, Utc::now());
        self.current = Some(active);
        self.schedule_refresh(delay, cx);
        cx.emit(GithubAccountStoreEvent::Changed);
        cx.notify();
    }

    fn schedule_refresh(&mut self, delay: Duration, cx: &mut Context<Self>) {
        self.refresh_timer = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(delay).await;
            this.update(cx, |this, cx| this.refresh_token(cx)).ok();
        }));
    }

    /// Once per session, offers to connect a GitHub account on OTHCloud.
    fn maybe_offer_connect(&mut self, api: Arc<OthcloudApi>, cx: &mut Context<Self>) {
        if self.connect_offer_shown {
            return;
        }
        self.connect_offer_shown = true;
        let dismissed = KeyValueStore::global(cx)
            .read_kvp(CONNECT_OFFER_DISMISSED_KEY)
            .log_err()
            .flatten()
            .is_some();
        if dismissed {
            return;
        }
        cx.spawn(async move |_, cx| {
            let github_sign_in_available = cx
                .background_spawn(async move { api.social_providers_github().await })
                .await;
            if !github_sign_in_available {
                return;
            }
            cx.update(|cx| {
                workspace::notifications::show_app_notification(
                    NotificationId::unique::<ConnectGithubOffer>(),
                    cx,
                    |cx| {
                        cx.new(|cx| {
                            MessageNotification::new(
                                "Connect GitHub to clone and push with OTHCloud",
                                cx,
                            )
                            .primary_message("Connect GitHub")
                            .primary_icon(IconName::Github)
                            .primary_on_click(|window, cx| {
                                window.dispatch_action(Box::new(ConnectGithub), cx);
                            })
                            .secondary_message("Don't Show Again")
                            .secondary_on_click(|_, cx| {
                                let kvp = KeyValueStore::global(cx);
                                db::write_and_log(cx, move || async move {
                                    kvp.write_kvp(
                                        CONNECT_OFFER_DISMISSED_KEY.to_string(),
                                        "1".to_string(),
                                    )
                                    .await
                                });
                            })
                        })
                    },
                );
            });
        })
        .detach();
    }

    /// Opens OTHCloud's "connect GitHub" page and waits until the user's own
    /// GitHub account is linked, then switches to it.
    fn connect(&mut self, workspace: WeakEntity<Workspace>, cx: &mut Context<Self>) {
        let Some(api) = othcloud_api(cx) else {
            return;
        };
        cx.open_url(&othcloud_client::absolute_url(&self.connect_path()));
        self.connect_task = Some(cx.spawn(async move |this, cx| {
            let started = Instant::now();
            loop {
                cx.background_executor().timer(CONNECT_POLL_INTERVAL).await;
                if started.elapsed() > CONNECT_TIMEOUT {
                    workspace
                        .update(cx, |workspace, cx| {
                            show_status(
                                workspace,
                                "Timed out waiting for GitHub to be connected on OTHCloud.",
                                true,
                                cx,
                            )
                        })
                        .ok();
                    return;
                }
                let request_api = api.clone();
                let result = cx
                    .background_spawn(async move { request_api.github_token(None).await })
                    .await;
                match result {
                    Ok(response)
                        if response.kind == GithubTokenKind::User && !response.token.is_empty() =>
                    {
                        let login = response
                            .login
                            .clone()
                            .unwrap_or_else(|| "your account".to_string());
                        this.update(cx, |this, cx| {
                            this.set_selected(response.account_id.clone(), cx);
                            // Don't drop `connect_task` here: this closure
                            // runs inside it, and it ends right after anyway.
                            this.install(response, cx);
                        })
                        .ok();
                        cx.update(|cx| {
                            dismiss_connect_offer(cx);
                        });
                        workspace
                            .update(cx, |workspace, cx| {
                                show_status(
                                    workspace,
                                    format!("GitHub connected as {login}"),
                                    false,
                                    cx,
                                )
                            })
                            .ok();
                        return;
                    }
                    Err(error) if error.is_unauthorized() => {
                        cx.update(|cx| {
                            if let Some(account) = OthcloudAccount::global(cx) {
                                account.update(cx, |account, cx| account.handle_unauthorized(cx));
                            }
                        });
                        return;
                    }
                    _ => {}
                }
            }
        }));
    }
}

struct ConnectGithubOffer;

fn dismiss_connect_offer(cx: &mut App) {
    workspace::notifications::dismiss_app_notification(
        &NotificationId::unique::<ConnectGithubOffer>(),
        cx,
    );
}

/// How long to wait before refreshing a token that expires at `expires_at`.
fn refresh_delay(expires_at: Option<DateTime<Utc>>, now: DateTime<Utc>) -> Duration {
    let Some(expires_at) = expires_at else {
        return DEFAULT_REFRESH_INTERVAL;
    };
    let remaining = (expires_at - now).to_std().unwrap_or(Duration::ZERO);
    remaining
        .saturating_sub(REFRESH_MARGIN)
        .max(MIN_REFRESH_INTERVAL)
}

fn parse_expiry(expires_at: Option<&str>) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(expires_at?.trim())
        .ok()
        .map(|time| time.with_timezone(&Utc))
}

pub(crate) fn othcloud_api(cx: &App) -> Option<Arc<OthcloudApi>> {
    OthcloudAccount::global(cx)?.read(cx).api()
}

/// Shows a short status toast in `workspace`.
pub(crate) fn show_status(
    workspace: &mut Workspace,
    message: impl Into<SharedString>,
    is_error: bool,
    cx: &mut Context<Workspace>,
) {
    let (icon, color) = if is_error {
        (IconName::XCircle, Color::Error)
    } else {
        (IconName::Check, Color::Success)
    };
    show_status_with_icon(workspace, message, icon, color, is_error, cx);
}

/// Shows a neutral "in progress" toast in `workspace`.
pub(crate) fn show_progress(
    workspace: &mut Workspace,
    message: impl Into<SharedString>,
    cx: &mut Context<Workspace>,
) {
    show_status_with_icon(
        workspace,
        message,
        IconName::ArrowCircle,
        Color::Muted,
        false,
        cx,
    );
}

fn show_status_with_icon(
    workspace: &mut Workspace,
    message: impl Into<SharedString>,
    icon: IconName,
    color: Color,
    is_error: bool,
    cx: &mut Context<Workspace>,
) {
    let toast = StatusToast::new(message, cx, move |this, _| {
        this.icon(Icon::new(icon).size(IconSize::Small).color(color))
            .dismiss_button(is_error)
    });
    workspace.toggle_status_toast(toast, cx);
}

/// Tells the user to sign in to OTHCloud first, with a button that does.
pub(crate) fn show_sign_in_required(workspace: &mut Workspace, cx: &mut Context<Workspace>) {
    let toast = StatusToast::new("Sign in to OTHCloud to use GitHub.", cx, |this, _| {
        this.icon(
            Icon::new(IconName::Github)
                .size(IconSize::Small)
                .color(Color::Muted),
        )
        .action("Sign In", |_, cx| {
            if let Some(account) = OthcloudAccount::global(cx) {
                account.update(cx, |account, cx| account.begin_sign_in(cx));
            }
        })
    });
    workspace.toggle_status_toast(toast, cx);
}

fn connect_github(workspace: &mut Workspace, _window: &mut Window, cx: &mut Context<Workspace>) {
    if othcloud_api(cx).is_none() {
        show_sign_in_required(workspace, cx);
        return;
    }
    let Some(store) = GithubAccountStore::global(cx) else {
        return;
    };
    let workspace_handle = cx.weak_entity();
    store.update(cx, |store, cx| store.connect(workspace_handle, cx));
}

fn switch_github_account(
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let Some(api) = othcloud_api(cx) else {
        show_sign_in_required(workspace, cx);
        return;
    };
    let workspace_handle = cx.weak_entity();
    workspace.toggle_modal(window, cx, move |window, cx| {
        GithubAccountPicker::new(workspace_handle, api, window, cx)
    });
}

fn sign_in_to_github(workspace: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
    let Some(api) = othcloud_api(cx) else {
        show_sign_in_required(workspace, cx);
        return;
    };
    let workspace_handle = cx.weak_entity();
    workspace.toggle_modal(window, cx, move |window, cx| {
        GithubTokenModal::new(workspace_handle, api, window, cx)
    });
}

fn clone_from_github(workspace: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
    let token = GithubAccountStore::global(cx).and_then(|store| store.read(cx).current_token());
    let Some(token) = token else {
        if othcloud_api(cx).is_none() {
            show_sign_in_required(workspace, cx);
        } else {
            let toast = StatusToast::new(
                "Connect GitHub to clone your repositories.",
                cx,
                |this, _| {
                    this.icon(
                        Icon::new(IconName::Github)
                            .size(IconSize::Small)
                            .color(Color::Muted),
                    )
                    .action("Connect GitHub", |window, cx| {
                        window.dispatch_action(Box::new(ConnectGithub), cx);
                    })
                },
            );
            workspace.toggle_status_toast(toast, cx);
        }
        return;
    };
    let workspace_handle = cx.weak_entity();
    let http = cx.http_client();
    workspace.toggle_modal(window, cx, move |window, cx| {
        CloneRepoPicker::new(workspace_handle, http, token, window, cx)
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refresh_delay_uses_expiry_minus_margin() {
        let now = Utc::now();
        let expires = now + chrono::Duration::minutes(60);
        let delay = refresh_delay(Some(expires), now);
        assert_eq!(delay, Duration::from_secs(59 * 60));
    }

    #[test]
    fn refresh_delay_has_floor_and_default() {
        let now = Utc::now();
        assert_eq!(
            refresh_delay(Some(now - chrono::Duration::minutes(5)), now),
            MIN_REFRESH_INTERVAL
        );
        assert_eq!(refresh_delay(None, now), DEFAULT_REFRESH_INTERVAL);
    }

    #[test]
    fn parses_server_expiry() {
        let parsed = parse_expiry(Some("2026-01-01T00:00:00.000Z")).expect("parse");
        assert_eq!(parsed.to_rfc3339(), "2026-01-01T00:00:00+00:00");
        assert_eq!(parse_expiry(Some("soon")), None);
        assert_eq!(parse_expiry(None), None);
    }
}
