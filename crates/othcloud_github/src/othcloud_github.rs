//! GitHub for OTerminal: accounts, credentials and cloning.
//!
//! Two kinds of GitHub accounts are shown together in one switcher:
//!
//! - **OTHCloud accounts**: when signed in to OTHCloud, the GitHub accounts
//!   linked there. OTHCloud hands out short-lived tokens for them, which this
//!   crate keeps fresh.
//! - **Local accounts**: GitHub accounts stored on this PC, usable without
//!   OTHCloud. They are added by signing in with GitHub in the browser (OAuth
//!   device flow, when `othcloud.github_oauth_client_id` is configured) or by
//!   pasting a personal access token. Tokens live in the operating system's
//!   credential store (Windows Credential Manager); only a non-secret index
//!   is kept in the key-value store.
//!
//! The active account's token is installed as a process-wide git credential
//! override for `https://github.com` (see [`git::credential_override`]), so
//! every clone, fetch, pull and push OTerminal runs authenticates with it —
//! without touching the user's git configuration. With no active account,
//! git keeps using the user's own credential manager.

mod account_picker;
mod clone_picker;
mod device_flow_modal;
pub mod github_api;
mod github_settings;
pub mod local_accounts;
mod status_item;
mod token_modal;

use std::{
    sync::Arc,
    time::{Duration, Instant},
};

use anyhow::{Context as _, Result};
use chrono::{DateTime, Utc};
use db::kvp::KeyValueStore;
use gpui::{
    App, AppContext as _, Context, Entity, EventEmitter, Global, SharedString, Subscription, Task,
    WeakEntity, Window, actions,
};
use notifications::status_toast::StatusToast;
use othcloud_client::{
    ApiError, GithubAccount, GithubTokenKind, GithubTokenResponse, OthcloudAccount,
    OthcloudAccountEvent, OthcloudApi,
};
use settings::Settings as _;
use ui::{Color, Icon, IconName, IconSize};
use util::ResultExt as _;
use workspace::{
    Workspace,
    notifications::{NotificationId, simple_message_notification::MessageNotification},
};

pub use account_picker::GithubAccountPicker;
pub use clone_picker::{CloneRepoModal, default_clone_directory};
pub use device_flow_modal::GithubDeviceFlowModal;
pub use github_settings::OthcloudGithubSettings;
pub use local_accounts::{ActiveChoice, LocalGithubAccount, ResolvedAccount};
pub use status_item::GithubStatusItem;
pub use token_modal::GithubTokenModal;

use github_api::GithubError;

actions!(
    othcloud,
    [
        /// Connects your GitHub account on the OTHCloud website so OTerminal
        /// can clone and push as you.
        ConnectGithub,
        /// Switches the GitHub account OTerminal uses for git.
        SwitchGithubAccount,
        /// Adds a GitHub account on this PC: signs in with GitHub in the
        /// browser when an OAuth app is configured, else with a token.
        SignInToGithub,
        /// Adds a GitHub account on this PC by pasting a personal access token.
        AddGithubToken,
        /// Clones a repository — one of your GitHub repositories or any URL —
        /// and opens it as a new project tab.
        CloneRepository,
    ]
);

/// The OTHCloud page that links a GitHub account, when the server didn't say.
const DEFAULT_CONNECT_PATH: &str = "/desktop-github";
const CONNECT_OFFER_DISMISSED_KEY: &str = "othcloud.github.connectOfferDismissed";
/// How long [`ConnectGithub`] waits for the website flow to finish.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10 * 60);
const CONNECT_POLL_INTERVAL: Duration = Duration::from_secs(3);
/// Refresh this long before the token expires.
const REFRESH_MARGIN: Duration = Duration::from_secs(5 * 60);
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
    OthcloudGithubSettings::register(cx);

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
            .register_action(|workspace, _: &AddGithubToken, window, cx| {
                add_github_token(workspace, window, cx);
            })
            .register_action(|workspace, _: &CloneRepository, window, cx| {
                open_clone_modal(workspace, window, cx);
            });
    })
    .detach();
}

/// Where the active GitHub identity comes from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AccountSource {
    /// A GitHub account stored on this PC.
    Local { github_id: u64 },
    /// A token handed out by OTHCloud.
    Othcloud,
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
    /// A git provider's name in OTHCloud (Settings > Git), when OTHCloud sent it.
    pub label: Option<String>,
    pub kind: GithubTokenKind,
    pub source: AccountSource,
}

impl ActiveGithub {
    fn from_response(response: &GithubTokenResponse) -> Self {
        Self {
            token: response.token.clone(),
            expires_at: parse_expiry(response.expires_at.as_deref()),
            login: response.login.clone(),
            account_id: response.account_id.clone(),
            app_name: response.app_name.clone(),
            label: response
                .label
                .clone()
                .filter(|label| !label.trim().is_empty()),
            kind: response.kind,
            source: AccountSource::Othcloud,
        }
    }

    fn from_local(account: &LocalGithubAccount, token: String) -> Self {
        Self {
            token,
            expires_at: None,
            login: Some(account.login.clone()),
            account_id: None,
            app_name: None,
            label: None,
            kind: GithubTokenKind::User,
            source: AccountSource::Local {
                github_id: account.id,
            },
        }
    }

    /// A short human-readable name for this identity: the name OTHCloud gives
    /// it (a git provider's name in Settings > Git), else the GitHub login or
    /// App name.
    pub fn display_name(&self) -> String {
        if let Some(label) = &self.label {
            return label.clone();
        }
        match (self.kind, &self.login, &self.app_name) {
            (GithubTokenKind::User, Some(login), _) => login.clone(),
            (_, _, Some(app_name)) => app_name.clone(),
            (_, Some(login), _) => login.clone(),
            _ => "GitHub".to_string(),
        }
    }

    pub fn is_local(&self) -> bool {
        matches!(self.source, AccountSource::Local { .. })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GithubAccountStoreEvent {
    /// The active GitHub identity (or its token), or the list of accounts,
    /// changed.
    Changed,
}

/// Knows the user's GitHub accounts (local and OTHCloud), which one is
/// active, and installs its token as git's credentials for github.com.
pub struct GithubAccountStore {
    /// What the user picked (persisted).
    choice: ActiveChoice,
    local_accounts: Vec<LocalGithubAccount>,
    current: Option<ActiveGithub>,
    /// Something wrong with the active account the user should know about.
    problem: Option<SharedString>,
    local_task: Option<Task<()>>,
    /// The latest key-value store write; each write waits for the previous one.
    persist_task: Option<Task<()>>,

    // OTHCloud
    user_id: Option<String>,
    /// The OTHCloud account the user picked, persisted per OTHCloud user.
    /// `None` means "whatever OTHCloud prefers".
    selected: Option<String>,
    othcloud_accounts: Option<Vec<GithubAccount>>,
    othcloud_connect_available: bool,
    othcloud_accounts_loading: bool,
    othcloud_accounts_error: Option<SharedString>,
    othcloud_accounts_task: Option<Task<()>>,
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
            log::warn!("othcloud_github initialized before othcloud_client; OTHCloud disabled");
        }
        let subscription = account
            .as_ref()
            .map(|account| cx.subscribe(account, Self::handle_account_event));

        let kvp = KeyValueStore::global(cx);
        let local_accounts = kvp
            .read_kvp(local_accounts::LOCAL_ACCOUNTS_KEY)
            .log_err()
            .flatten()
            .map(|json| local_accounts::parse_index(&json))
            .unwrap_or_default();
        let choice = ActiveChoice::parse(
            kvp.read_kvp(local_accounts::ACTIVE_CHOICE_KEY)
                .log_err()
                .flatten()
                .as_deref(),
        );

        let mut this = Self {
            choice,
            local_accounts,
            current: None,
            problem: None,
            local_task: None,
            persist_task: None,
            user_id: None,
            selected: None,
            othcloud_accounts: None,
            othcloud_connect_available: true,
            othcloud_accounts_loading: false,
            othcloud_accounts_error: None,
            othcloud_accounts_task: None,
            connect_path: None,
            connect_offer_shown: false,
            refresh_task: None,
            refresh_timer: None,
            connect_task: None,
            _subscription: subscription,
        };
        if account.is_some_and(|account| account.read(cx).is_signed_in()) {
            this.on_signed_in(cx);
        } else {
            this.apply(cx);
        }
        this
    }

    // -----------------------------------------------------------------
    // Queries
    // -----------------------------------------------------------------

    pub fn active_account(&self) -> Option<&ActiveGithub> {
        self.current.as_ref()
    }

    pub fn current_token(&self) -> Option<String> {
        self.current.as_ref().map(|current| current.token.clone())
    }

    pub fn choice(&self) -> ActiveChoice {
        self.choice
    }

    pub fn resolved(&self, cx: &App) -> ResolvedAccount {
        local_accounts::resolve(self.choice, &self.local_accounts, othcloud_signed_in(cx))
    }

    pub fn local_accounts(&self) -> &[LocalGithubAccount] {
        &self.local_accounts
    }

    pub fn problem(&self) -> Option<&SharedString> {
        self.problem.as_ref()
    }

    /// The OTHCloud GitHub accounts, once loaded (`None` also for older
    /// OTHCloud servers that can't list them).
    pub fn othcloud_accounts(&self) -> Option<&[GithubAccount]> {
        self.othcloud_accounts.as_deref()
    }

    pub fn othcloud_accounts_loading(&self) -> bool {
        self.othcloud_accounts_loading
    }

    pub fn othcloud_accounts_error(&self) -> Option<&SharedString> {
        self.othcloud_accounts_error.as_ref()
    }

    pub fn othcloud_connect_available(&self) -> bool {
        self.othcloud_connect_available
    }

    /// The account id the user explicitly picked on OTHCloud, if any.
    pub fn selected_account_id(&self) -> Option<&str> {
        self.selected.as_deref()
    }

    /// The OTHCloud account id in use: the picked one, else the one OTHCloud chose.
    pub fn active_account_id(&self) -> Option<&str> {
        self.current
            .as_ref()
            .filter(|current| current.source == AccountSource::Othcloud)
            .and_then(|current| current.account_id.as_deref())
            .or(self.selected.as_deref())
    }

    /// A short label for the account git uses right now ("octocat"), or
    /// `None` when git uses the user's own credential manager.
    pub fn active_label(&self, cx: &App) -> Option<String> {
        if let Some(current) = &self.current {
            // The name OTHCloud lists it under, for servers whose token
            // response doesn't carry it.
            if current.source == AccountSource::Othcloud
                && current.label.is_none()
                && let Some(account) = current.account_id.as_deref().and_then(|id| {
                    self.othcloud_accounts
                        .as_ref()?
                        .iter()
                        .find(|account| account.id == id)
                })
                && !account.label.trim().is_empty()
            {
                return Some(account.label.clone());
            }
            return Some(current.display_name());
        }
        match self.resolved(cx) {
            ResolvedAccount::None => None,
            ResolvedAccount::Local(id) => self
                .local_accounts
                .iter()
                .find(|account| account.id == id)
                .map(|account| account.login.clone()),
            ResolvedAccount::Othcloud => Some("OTHCloud".to_string()),
        }
    }

    pub fn connect_path(&self) -> String {
        self.connect_path
            .clone()
            .unwrap_or_else(|| DEFAULT_CONNECT_PATH.to_string())
    }

    // -----------------------------------------------------------------
    // Switching accounts
    // -----------------------------------------------------------------

    /// Uses the local account `github_id` for git.
    pub fn use_local_account(&mut self, github_id: u64, cx: &mut Context<Self>) {
        self.set_choice(ActiveChoice::Local(github_id), cx);
        self.apply(cx);
    }

    /// Uses no account: git falls back to the user's own credential manager.
    pub fn use_no_account(&mut self, cx: &mut Context<Self>) {
        self.set_choice(ActiveChoice::None, cx);
        self.apply(cx);
    }

    /// Uses the OTHCloud GitHub account `account_id` (or OTHCloud's default
    /// with `None`). `response` is a token already fetched for it, if any.
    pub fn use_othcloud_account(
        &mut self,
        account_id: Option<String>,
        response: Option<GithubTokenResponse>,
        cx: &mut Context<Self>,
    ) {
        self.set_choice(ActiveChoice::Othcloud, cx);
        self.set_selected(account_id, cx);
        match response.filter(|response| !response.token.is_empty()) {
            Some(response) if self.resolved(cx) == ResolvedAccount::Othcloud => {
                self.local_task = None;
                self.install_othcloud(response, cx);
            }
            _ => {
                if self
                    .current
                    .as_ref()
                    .is_some_and(|current| current.is_local())
                {
                    self.uninstall_token(cx);
                }
                self.refresh_token(cx);
            }
        }
    }

    /// Stores `token` for the local account `account` in the credential
    /// store, remembers the account, and makes it the active one.
    pub fn add_local_account(
        &mut self,
        account: LocalGithubAccount,
        token: String,
        cx: &mut Context<Self>,
    ) -> Task<Result<()>> {
        cx.spawn(async move |this, cx| {
            local_accounts::write_token(&account, &token, cx)
                .await
                .context("Couldn't save the token to the system credential store")?;
            this.update(cx, |this, cx| {
                local_accounts::upsert(&mut this.local_accounts, account.clone());
                this.persist_local_accounts(cx);
                this.set_choice(ActiveChoice::Local(account.id), cx);
                this.local_task = None;
                this.refresh_task = None;
                this.refresh_timer = None;
                this.install_local(&account, token, cx);
            })?;
            Ok(())
        })
    }

    /// Forgets the local account `github_id` and deletes its token.
    pub fn remove_local_account(&mut self, github_id: u64, cx: &mut Context<Self>) -> Task<()> {
        local_accounts::remove(&mut self.local_accounts, github_id);
        self.persist_local_accounts(cx);
        if self.choice == ActiveChoice::Local(github_id) {
            self.set_choice(ActiveChoice::Auto, cx);
        }
        if self
            .current
            .as_ref()
            .is_some_and(|current| current.source == AccountSource::Local { github_id })
        {
            self.local_task = None;
            self.uninstall_token(cx);
        }
        self.apply(cx);
        cx.emit(GithubAccountStoreEvent::Changed);
        cx.notify();
        cx.spawn(async move |_, cx| {
            local_accounts::delete_token(github_id, cx)
                .await
                .context("deleting a GitHub token from the credential store")
                .log_err();
        })
    }

    /// Reloads the list of OTHCloud GitHub accounts.
    pub fn refresh_othcloud_accounts(&mut self, cx: &mut Context<Self>) {
        let Some(api) = othcloud_api(cx) else {
            self.othcloud_accounts = None;
            self.othcloud_accounts_loading = false;
            self.othcloud_accounts_error = None;
            return;
        };
        self.othcloud_accounts_loading = true;
        let host = api.host().to_string();
        let load = cx.background_spawn(async move { api.github_accounts().await });
        self.othcloud_accounts_task = Some(cx.spawn(async move |this, cx| {
            let result = load.await;
            this.update(cx, |this, cx| {
                this.othcloud_accounts_loading = false;
                match result {
                    Ok(Some(response)) => {
                        this.othcloud_accounts = Some(response.accounts);
                        this.othcloud_connect_available = response.connect_available;
                        if let Some(path) = response.connect_path {
                            this.connect_path = Some(path);
                        }
                        this.othcloud_accounts_error = None;
                    }
                    Ok(None) => {
                        // An older OTHCloud: only the current account is known.
                        this.othcloud_accounts = None;
                        this.othcloud_connect_available = true;
                        this.othcloud_accounts_error = Some(
                            format!(
                                "Couldn't load OTHCloud GitHub accounts: {host} can't list \
                                 them yet (update OTHCloud)."
                            )
                            .into(),
                        );
                    }
                    Err(error) => {
                        log::warn!("listing OTHCloud GitHub accounts failed: {error}");
                        if error.is_unauthorized() {
                            handle_othcloud_unauthorized(cx);
                        }
                        this.othcloud_accounts_error = Some(
                            format!(
                                "Couldn't load OTHCloud GitHub accounts: {}",
                                error.friendly_message(&host)
                            )
                            .into(),
                        );
                    }
                }
                cx.emit(GithubAccountStoreEvent::Changed);
                cx.notify();
            })
            .ok();
        }));
    }

    // -----------------------------------------------------------------
    // Internals
    // -----------------------------------------------------------------

    fn set_choice(&mut self, choice: ActiveChoice, cx: &mut Context<Self>) {
        if self.choice == choice {
            return;
        }
        self.choice = choice;
        self.persist(
            local_accounts::ACTIVE_CHOICE_KEY,
            Some(choice.serialize()),
            cx,
        );
    }

    fn persist_local_accounts(&mut self, cx: &mut Context<Self>) {
        let json = local_accounts::serialize_index(&self.local_accounts);
        self.persist(local_accounts::LOCAL_ACCOUNTS_KEY, Some(json), cx);
    }

    /// Writes (or with `None` deletes) a key-value store entry. Writes run
    /// one after another, so the last change always wins.
    fn persist(&mut self, key: &str, value: Option<String>, cx: &mut Context<Self>) {
        let kvp = KeyValueStore::global(cx);
        let key = key.to_string();
        let previous = self.persist_task.take();
        self.persist_task = Some(cx.background_spawn(async move {
            if let Some(previous) = previous {
                previous.await;
            }
            let result = match value {
                Some(value) => kvp.write_kvp(key, value).await,
                None => kvp.delete_kvp(key).await,
            };
            result.log_err();
        }));
    }

    /// Makes the installed credentials match the resolved choice.
    fn apply(&mut self, cx: &mut Context<Self>) {
        match self.resolved(cx) {
            ResolvedAccount::None => {
                self.local_task = None;
                self.refresh_task = None;
                self.refresh_timer = None;
                self.problem = None;
                self.uninstall_token(cx);
            }
            ResolvedAccount::Local(github_id) => {
                self.refresh_task = None;
                self.refresh_timer = None;
                let already_active = self
                    .current
                    .as_ref()
                    .is_some_and(|current| current.source == AccountSource::Local { github_id });
                if !already_active {
                    self.load_local(github_id, cx);
                }
            }
            ResolvedAccount::Othcloud => {
                self.local_task = None;
                let already_active = self
                    .current
                    .as_ref()
                    .is_some_and(|current| current.source == AccountSource::Othcloud);
                if !already_active {
                    if self.current.is_some() {
                        self.uninstall_token(cx);
                    }
                    self.refresh_token(cx);
                }
            }
        }
    }

    fn load_local(&mut self, github_id: u64, cx: &mut Context<Self>) {
        let Some(account) = self
            .local_accounts
            .iter()
            .find(|account| account.id == github_id)
            .cloned()
        else {
            return;
        };
        // Never keep another identity's token while this one loads.
        if self.current.is_some() {
            self.uninstall_token(cx);
        }
        let http = cx.http_client();
        self.local_task = Some(cx.spawn(async move |this, cx| {
            let token = local_accounts::read_token(github_id, cx).await;
            let Some(token) = token else {
                let message = format!(
                    "The GitHub token of {} is missing from the system credential store. \
                     Sign in to GitHub again.",
                    account.login
                );
                let still_active = this
                    .update(cx, |this, cx| {
                        let still_active = this.resolved(cx) == ResolvedAccount::Local(github_id);
                        if still_active {
                            this.problem = Some(message.clone().into());
                            cx.emit(GithubAccountStoreEvent::Changed);
                            cx.notify();
                        }
                        still_active
                    })
                    .unwrap_or(false);
                if still_active {
                    cx.update(|cx| notify_account_problem(message, cx));
                }
                return;
            };

            let installed = this
                .update(cx, |this, cx| {
                    if this.resolved(cx) != ResolvedAccount::Local(github_id) {
                        return false;
                    }
                    this.install_local(&account, token.clone(), cx);
                    true
                })
                .unwrap_or(false);
            if !installed {
                return;
            }

            // Check that GitHub still accepts the token, so a revoked token
            // is reported now rather than at the next push.
            match github_api::validate_token(http, &token).await {
                Ok(validated) => {
                    this.update(cx, |this, cx| {
                        this.refresh_local_profile(github_id, &validated, cx)
                    })
                    .ok();
                }
                Err(GithubError::Unauthorized) => {
                    let message = format!(
                        "GitHub no longer accepts the token of {}: it expired or was revoked. \
                         Sign in again.",
                        account.login
                    );
                    let still_active = this
                        .update(cx, |this, cx| {
                            let still_active = this.current.as_ref().is_some_and(|current| {
                                current.source == AccountSource::Local { github_id }
                            });
                            if still_active {
                                this.problem = Some(message.clone().into());
                                cx.emit(GithubAccountStoreEvent::Changed);
                                cx.notify();
                            }
                            still_active
                        })
                        .unwrap_or(false);
                    if still_active {
                        cx.update(|cx| notify_account_problem(message, cx));
                    }
                }
                Err(error) => {
                    log::info!(
                        "couldn't check the GitHub token of {}: {error}",
                        account.login
                    )
                }
            }
        }));
    }

    /// Keeps the stored login/avatar of a local account up to date.
    fn refresh_local_profile(
        &mut self,
        github_id: u64,
        validated: &github_api::ValidatedToken,
        cx: &mut Context<Self>,
    ) {
        let Some(existing) = self
            .local_accounts
            .iter_mut()
            .find(|account| account.id == github_id)
        else {
            return;
        };
        let mut updated = LocalGithubAccount::from_validated(validated, "");
        updated.saved_to_othcloud = existing.saved_to_othcloud;
        updated.source = existing.source.clone();
        if *existing != updated {
            *existing = updated;
            self.persist_local_accounts(cx);
            if let Some(current) = self.current.as_mut()
                && current.source == (AccountSource::Local { github_id })
            {
                current.login = Some(validated.user.login.clone());
            }
            cx.emit(GithubAccountStoreEvent::Changed);
            cx.notify();
        }
    }

    fn install_local(
        &mut self,
        account: &LocalGithubAccount,
        token: String,
        cx: &mut Context<Self>,
    ) {
        self.install(ActiveGithub::from_local(account, token), cx);
    }

    fn install_othcloud(&mut self, response: GithubTokenResponse, cx: &mut Context<Self>) {
        if response.token.is_empty() {
            return;
        }
        if let Some(path) = response.connect_path.clone() {
            self.connect_path = Some(path);
        }
        let active = ActiveGithub::from_response(&response);
        let delay = refresh_delay(active.expires_at, Utc::now());
        self.install(active, cx);
        self.schedule_refresh(delay, cx);
    }

    fn install(&mut self, active: ActiveGithub, cx: &mut Context<Self>) {
        git::set_git_credential_override(Some(Arc::new(git::GithubTokenCredentials {
            hosts: vec![GITHUB_HOST_URL.to_string()],
            token: active.token.clone(),
        })));
        git::set_github_api_token(Some(active.token.clone()));
        self.current = Some(active);
        self.problem = None;
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
            OthcloudAccountEvent::SignedOut => self.on_signed_out(cx),
            _ => {}
        }
    }

    fn on_signed_in(&mut self, cx: &mut Context<Self>) {
        let user_id = OthcloudAccount::global(cx)
            .and_then(|account| account.read(cx).user().map(|user| user.id.clone()));
        let user_changed = user_id != self.user_id;
        if user_changed {
            self.selected = user_id.as_deref().and_then(|user_id| {
                KeyValueStore::global(cx)
                    .read_kvp(&selected_account_key(user_id))
                    .log_err()
                    .flatten()
                    .filter(|id| !id.is_empty())
            });
            self.user_id = user_id;
            self.othcloud_accounts = None;
            self.refresh_othcloud_accounts(cx);
            if self
                .current
                .as_ref()
                .is_some_and(|current| current.source == AccountSource::Othcloud)
            {
                // Another OTHCloud user: drop the previous user's token.
                self.uninstall_token(cx);
            }
        }
        self.apply(cx);
    }

    fn on_signed_out(&mut self, cx: &mut Context<Self>) {
        self.user_id = None;
        self.selected = None;
        self.othcloud_accounts = None;
        self.othcloud_accounts_error = None;
        self.othcloud_accounts_task = None;
        self.connect_offer_shown = false;
        self.refresh_task = None;
        self.refresh_timer = None;
        self.connect_task = None;
        if self
            .current
            .as_ref()
            .is_some_and(|current| current.source == AccountSource::Othcloud)
        {
            self.uninstall_token(cx);
        }
        self.apply(cx);
        cx.emit(GithubAccountStoreEvent::Changed);
        cx.notify();
    }

    /// Remembers `account_id` as the user's OTHCloud choice (or forgets it).
    fn set_selected(&mut self, account_id: Option<String>, cx: &mut Context<Self>) {
        let account_id = account_id.filter(|id| !id.is_empty());
        if self.selected == account_id {
            return;
        }
        self.selected = account_id.clone();
        if let Some(user_id) = self.user_id.as_deref() {
            let key = selected_account_key(user_id);
            self.persist(&key, account_id, cx);
        }
    }

    /// Asks OTHCloud for a fresh token for the selected account (or its
    /// default) and installs it — when OTHCloud is the active source.
    pub fn refresh_token(&mut self, cx: &mut Context<Self>) {
        if self.resolved(cx) != ResolvedAccount::Othcloud {
            return;
        }
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
        if let Err(error) = &result
            && error.is_unauthorized()
        {
            handle_othcloud_unauthorized(cx);
            return;
        }
        // The user may have switched to a local account meanwhile.
        if self.resolved(cx) != ResolvedAccount::Othcloud {
            return;
        }
        match result {
            Ok(response) if !response.token.is_empty() => {
                let needs_connect = response.needs_connect;
                self.install_othcloud(response, cx);
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
            Err(error) if error.status == 404 => {
                // `no_github_connected`: neither a linked account nor an org app.
                self.uninstall_token(cx);
                self.maybe_offer_connect(api, cx);
                self.schedule_refresh(NO_TOKEN_RECHECK_INTERVAL, cx);
            }
            Err(error) => {
                log::warn!("fetching a GitHub token from OTHCloud failed: {error}");
                let still_valid = self.current.as_ref().is_some_and(|current| {
                    current.source == AccountSource::Othcloud
                        && current
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
                            // Don't drop `connect_task` here: this closure
                            // runs inside it, and it ends right after anyway.
                            this.use_othcloud_account(
                                response.account_id.clone(),
                                Some(response),
                                cx,
                            );
                            this.refresh_othcloud_accounts(cx);
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
                        cx.update(handle_othcloud_unauthorized);
                        return;
                    }
                    _ => {}
                }
            }
        }));
    }
}

struct ConnectGithubOffer;
struct GithubAccountProblem;

fn dismiss_connect_offer(cx: &mut App) {
    workspace::notifications::dismiss_app_notification(
        &NotificationId::unique::<ConnectGithubOffer>(),
        cx,
    );
}

/// Tells the user the active GitHub account needs attention.
fn notify_account_problem(message: String, cx: &mut App) {
    workspace::notifications::show_app_notification(
        NotificationId::unique::<GithubAccountProblem>(),
        cx,
        move |cx| {
            let message = message.clone();
            cx.new(move |cx| {
                MessageNotification::new(message, cx)
                    .primary_message("Sign In to GitHub")
                    .primary_icon(IconName::Github)
                    .primary_on_click(|window, cx| {
                        window.dispatch_action(Box::new(SignInToGithub), cx);
                    })
                    .secondary_message("Switch Account")
                    .secondary_on_click(|window, cx| {
                        window.dispatch_action(Box::new(SwitchGithubAccount), cx);
                    })
            })
        },
    );
}

pub(crate) fn handle_othcloud_unauthorized(cx: &mut App) {
    if let Some(account) = OthcloudAccount::global(cx) {
        account.update(cx, |account, cx| account.handle_unauthorized(cx));
    }
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

pub(crate) fn othcloud_signed_in(cx: &App) -> bool {
    OthcloudAccount::global(cx).is_some_and(|account| account.read(cx).is_signed_in())
}

pub(crate) fn othcloud_api(cx: &App) -> Option<Arc<OthcloudApi>> {
    OthcloudAccount::global(cx)?.read(cx).api()
}

pub(crate) fn oauth_client_id(cx: &App) -> Option<String> {
    OthcloudGithubSettings::get_global(cx)
        .github_oauth_client_id
        .clone()
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
    let toast = StatusToast::new("Sign in to OTHCloud first.", cx, |this, _| {
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
    let workspace_handle = cx.weak_entity();
    workspace.toggle_modal(window, cx, move |window, cx| {
        GithubAccountPicker::new(workspace_handle, window, cx)
    });
}

fn sign_in_to_github(workspace: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
    let workspace_handle = cx.weak_entity();
    match oauth_client_id(cx) {
        Some(client_id) => workspace.toggle_modal(window, cx, move |window, cx| {
            GithubDeviceFlowModal::new(workspace_handle, client_id, window, cx)
        }),
        None => workspace.toggle_modal(window, cx, move |window, cx| {
            GithubTokenModal::new(workspace_handle, window, cx)
        }),
    }
}

fn add_github_token(workspace: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
    let workspace_handle = cx.weak_entity();
    workspace.toggle_modal(window, cx, move |window, cx| {
        GithubTokenModal::new(workspace_handle, window, cx)
    });
}

/// Opens the "Clone Repository" modal. Also used for `git: clone`.
pub fn open_clone_modal(
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let workspace_handle = cx.weak_entity();
    workspace.toggle_modal(window, cx, move |window, cx| {
        CloneRepoModal::new(workspace_handle, window, cx)
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
        assert_eq!(delay, Duration::from_secs(55 * 60));
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

    #[test]
    fn display_names() {
        let local = LocalGithubAccount {
            id: 1,
            login: "octocat".into(),
            name: None,
            avatar_url: None,
            scopes: None,
            saved_to_othcloud: false,
            source: None,
        };
        let active = ActiveGithub::from_local(&local, "tok".into());
        assert_eq!(active.display_name(), "octocat");
        assert!(active.is_local());

        let app = ActiveGithub::from_response(&GithubTokenResponse {
            kind: GithubTokenKind::Installation,
            token: "ghs_x".into(),
            app_name: Some("OTHCloud App".into()),
            ..Default::default()
        });
        assert_eq!(app.display_name(), "OTHCloud App");
        assert!(!app.is_local());

        // The git provider's name in OTHCloud wins over the App's name.
        let named = ActiveGithub::from_response(&GithubTokenResponse {
            kind: GithubTokenKind::Installation,
            token: "ghs_x".into(),
            app_name: Some("othcloud-2026-04-01".into()),
            label: Some("My deploy app".into()),
            ..Default::default()
        });
        assert_eq!(named.display_name(), "My deploy app");
    }

    fn local(id: u64, login: &str) -> LocalGithubAccount {
        LocalGithubAccount {
            id,
            login: login.into(),
            name: None,
            avatar_url: None,
            scopes: Some(vec!["repo".into()]),
            saved_to_othcloud: false,
            source: Some("token".into()),
        }
    }

    /// Adding, persisting, restoring and removing local accounts. (The test
    /// platform's credential store never returns secrets, which also covers
    /// "token missing from the credential store" on restore.)
    #[gpui::test]
    async fn store_persists_local_accounts(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| cx.set_global(db::AppDatabase::test_new()));

        let store = cx.new(GithubAccountStore::new);
        cx.read(|cx| {
            let store = store.read(cx);
            assert!(store.local_accounts().is_empty());
            assert_eq!(store.resolved(cx), ResolvedAccount::None);
            assert!(store.active_account().is_none());
        });

        store
            .update(cx, |store, cx| {
                store.add_local_account(local(7, "octocat"), "ghp_seven".into(), cx)
            })
            .await
            .expect("adding a local account");
        store
            .update(cx, |store, cx| {
                store.add_local_account(local(8, "hubot"), "ghp_eight".into(), cx)
            })
            .await
            .expect("adding a second local account");
        cx.run_until_parked();

        cx.read(|cx| {
            let store = store.read(cx);
            assert_eq!(store.choice(), ActiveChoice::Local(8));
            assert_eq!(store.resolved(cx), ResolvedAccount::Local(8));
            let active = store.active_account().expect("an active account");
            assert_eq!(active.display_name(), "hubot");
            assert_eq!(active.source, AccountSource::Local { github_id: 8 });
        });
        assert!(git::git_credential_override_active());
        assert_eq!(git::github_api_token().as_deref(), Some("ghp_eight"));

        // Switching between accounts and to "no account".
        store.update(cx, |store, cx| store.use_no_account(cx));
        assert!(!git::git_credential_override_active());
        assert_eq!(git::github_api_token(), None);
        store.update(cx, |store, cx| store.use_local_account(7, cx));
        cx.run_until_parked();

        // The non-secret index and the choice survive a restart.
        let kvp = cx.read(|cx| KeyValueStore::global(cx));
        let index = kvp
            .read_kvp(local_accounts::LOCAL_ACCOUNTS_KEY)
            .unwrap()
            .expect("index persisted");
        assert!(!index.contains("ghp_"), "tokens must never be in the index");
        assert_eq!(
            kvp.read_kvp(local_accounts::ACTIVE_CHOICE_KEY)
                .unwrap()
                .as_deref(),
            Some("local:7")
        );

        let restored = cx.new(GithubAccountStore::new);
        cx.run_until_parked();
        cx.read(|cx| {
            let restored = restored.read(cx);
            assert_eq!(
                restored
                    .local_accounts()
                    .iter()
                    .map(|account| account.login.as_str())
                    .collect::<Vec<_>>(),
                vec!["octocat", "hubot"]
            );
            assert_eq!(restored.resolved(cx), ResolvedAccount::Local(7));
            // The test credential store has no token: reported, not installed.
            assert!(restored.active_account().is_none());
            assert!(
                restored
                    .problem()
                    .is_some_and(|problem| problem.contains("missing"))
            );
        });

        // Removing the active account falls back to the next local one.
        drop(restored);
        store.update(cx, |store, cx| {
            store.remove_local_account(7, cx).detach();
        });
        cx.run_until_parked();
        cx.read(|cx| {
            let store = store.read(cx);
            assert_eq!(store.choice(), ActiveChoice::Auto);
            assert_eq!(store.local_accounts().len(), 1);
            assert_eq!(store.resolved(cx), ResolvedAccount::Local(8));
        });
        store.update(cx, |store, cx| {
            store.remove_local_account(8, cx).detach();
        });
        cx.run_until_parked();
        cx.read(|cx| assert_eq!(store.read(cx).resolved(cx), ResolvedAccount::None));
        assert!(!git::git_credential_override_active());
        let index = kvp
            .read_kvp(local_accounts::LOCAL_ACCOUNTS_KEY)
            .unwrap()
            .expect("index persisted");
        assert_eq!(local_accounts::parse_index(&index), Vec::new());
    }
}
