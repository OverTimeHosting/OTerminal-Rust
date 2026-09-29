use std::{
    sync::Arc,
    time::{Duration, Instant},
};

use anyhow::{Result, anyhow};
use gpui::{
    App, AppContext as _, AsyncApp, Context, Entity, EventEmitter, Global, Task, TaskExt as _,
};
use util::ResultExt as _;
use workspace::notifications::{
    NotificationId, show_app_notification, simple_message_notification::MessageNotification,
};

use crate::{
    api::{ApiError, OthcloudApi, ServicesResponse, User},
    base_url::{absolute_url, base_url, host_of, pages},
    deep_link::{URL_SCHEME, parse_pairing_url},
    storage,
};

const RETRY_BACKOFF: [Duration; 5] = [
    Duration::from_secs(15),
    Duration::from_secs(30),
    Duration::from_secs(60),
    Duration::from_secs(120),
    Duration::from_secs(300),
];
const POLL_INTERVAL: Duration = Duration::from_secs(5 * 60);

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum LoadState {
    #[default]
    Idle,
    Loading,
    Loaded {
        at: Instant,
    },
    Error {
        message: String,
        /// Whether older data is still being shown.
        stale: bool,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum OthcloudAccountEvent {
    SignedIn,
    SignedOut,
    UserChanged,
    ServicesChanged,
}

struct GlobalOthcloudAccount(Entity<OthcloudAccount>);

impl Global for GlobalOthcloudAccount {}

/// The signed-in OTHCloud user and their desktop token.
///
/// Other crates call endpoints through [`OthcloudAccount::api`]; when one answers
/// 401 they must call [`OthcloudAccount::handle_unauthorized`]. They react to
/// session changes by subscribing to [`OthcloudAccountEvent`].
pub struct OthcloudAccount {
    user: Option<User>,
    token: Option<String>,
    api: Option<Arc<OthcloudApi>>,
    services: Option<ServicesResponse>,
    services_state: LoadState,
    last_refreshed: Option<Instant>,
    retry_backoff_ix: usize,
    retry_task: Option<Task<()>>,
    _poll_task: Task<()>,
}

impl EventEmitter<OthcloudAccountEvent> for OthcloudAccount {}

impl OthcloudAccount {
    pub fn global(cx: &App) -> Option<Entity<Self>> {
        cx.try_global::<GlobalOthcloudAccount>()
            .map(|global| global.0.clone())
    }

    pub(crate) fn init_global(cx: &mut App) -> Entity<Self> {
        if let Some(account) = Self::global(cx) {
            return account;
        }
        let account = cx.new(Self::new);
        cx.set_global(GlobalOthcloudAccount(account.clone()));
        account
    }

    fn new(cx: &mut Context<Self>) -> Self {
        let poll_task = cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(POLL_INTERVAL).await;
                let still_alive = this.update(cx, |this, cx| {
                    if this.is_signed_in() {
                        this.refresh(cx).detach();
                    }
                });
                if still_alive.is_err() {
                    break;
                }
            }
        });

        cx.spawn(async move |this, cx| {
            let Some(token) = storage::read_token(cx).await else {
                return;
            };
            let cached_user = storage::read_cached_user(cx).await;
            this.update(cx, |this, cx| {
                if this.token.is_some() {
                    return;
                }
                this.set_session(token, cached_user, cx);
                if this.is_signed_in() {
                    cx.emit(OthcloudAccountEvent::SignedIn);
                }
                this.refresh(cx).detach();
            })
            .log_err();
        })
        .detach();

        Self {
            user: None,
            token: None,
            api: None,
            services: None,
            services_state: LoadState::Idle,
            last_refreshed: None,
            retry_backoff_ix: 0,
            retry_task: None,
            _poll_task: poll_task,
        }
    }

    pub fn user(&self) -> Option<&User> {
        self.user.as_ref()
    }

    pub fn is_signed_in(&self) -> bool {
        self.user.is_some() && self.token.is_some()
    }

    /// The API client for the current session, available as soon as a token is
    /// known (even before the user has been fetched).
    pub fn api(&self) -> Option<Arc<OthcloudApi>> {
        self.api.clone()
    }

    pub fn services(&self) -> Option<&ServicesResponse> {
        self.services.as_ref()
    }

    pub fn services_state(&self) -> &LoadState {
        &self.services_state
    }

    pub fn last_refreshed(&self) -> Option<Instant> {
        self.last_refreshed
    }

    pub fn base_url(&self) -> String {
        self.api
            .as_ref()
            .map(|api| api.base().to_string())
            .unwrap_or_else(base_url)
    }

    /// Opens the OTHCloud pairing page; it hands a code back through an
    /// `othcloud-terminal://auth?code=...` link.
    pub fn begin_sign_in(&mut self, cx: &mut Context<Self>) {
        cx.open_url(&absolute_url(pages::pair()));
    }

    /// Accepts either a full `othcloud-terminal://auth?code=...` link or a bare
    /// pairing code pasted by the user.
    pub fn complete_pairing_from_link_or_code(
        &mut self,
        input: &str,
        cx: &mut Context<Self>,
    ) -> Task<Result<()>> {
        let input = input.trim();
        let scheme_prefix = format!("{URL_SCHEME}:");
        let code = if input
            .get(..scheme_prefix.len())
            .is_some_and(|prefix| prefix.eq_ignore_ascii_case(&scheme_prefix))
        {
            parse_pairing_url(input)
        } else {
            Some(input.to_string()).filter(|code| !code.is_empty())
        };
        match code {
            Some(code) => self.complete_pairing(code, cx),
            None => {
                let message = "OTHCloud sign-in link was missing a pairing code.";
                show_notification(message.to_string(), cx);
                Task::ready(Err(anyhow!(message)))
            }
        }
    }

    /// Exchanges a pairing code for a desktop token and signs in. Shows a
    /// notification with the outcome, so callers only need to log the result.
    pub fn complete_pairing(&mut self, code: String, cx: &mut Context<Self>) -> Task<Result<()>> {
        let http = cx.http_client();
        let base = base_url();
        cx.spawn(async move |this, cx| {
            let result = exchange_and_store(http, &base, &code, cx).await;
            match result {
                Ok((token, user)) => {
                    let name = user.display_name().to_string();
                    this.update(cx, |this, cx| {
                        this.set_session(token, Some(user), cx);
                        cx.emit(OthcloudAccountEvent::SignedIn);
                        cx.emit(OthcloudAccountEvent::UserChanged);
                        this.refresh(cx).detach();
                    })?;
                    cx.update(|cx| {
                        show_notification(format!("Signed in to OTHCloud as {name}"), cx)
                    });
                    Ok(())
                }
                Err(message) => {
                    cx.update(|cx| {
                        show_notification(format!("Could not sign in to OTHCloud: {message}"), cx)
                    });
                    Err(anyhow!(message))
                }
            }
        })
    }

    pub fn sign_out(&mut self, cx: &mut Context<Self>) -> Task<Result<()>> {
        self.clear_session(cx);
        cx.spawn(async move |_, cx| clear_stored_session(cx).await)
    }

    /// Call when any OTHCloud request answers 401: the desktop token was revoked
    /// or expired, so the session is dropped and the user must pair again.
    pub fn handle_unauthorized(&mut self, cx: &mut Context<Self>) {
        if self.token.is_none() && self.user.is_none() {
            return;
        }
        log::info!("OTHCloud rejected the desktop token; signing out");
        self.clear_session(cx);
        cx.spawn(async move |_, cx| clear_stored_session(cx).await)
            .detach_and_log_err(cx);
    }

    /// Re-fetches the user and their services. Failures other than 401 keep the
    /// previous data and retry with backoff.
    pub fn refresh(&mut self, cx: &mut Context<Self>) -> Task<Result<()>> {
        let (Some(api), Some(token)) = (self.api.clone(), self.token.clone()) else {
            return Task::ready(Ok(()));
        };
        self.retry_task = None;
        self.services_state = LoadState::Loading;
        cx.notify();

        let fetch = cx.background_spawn({
            let api = api.clone();
            async move { futures::join!(api.me(), api.services()) }
        });
        cx.spawn(async move |this, cx| {
            let (me, services) = fetch.await;
            this.update(cx, |this, cx| {
                if this.token.as_deref() != Some(token.as_str()) {
                    return Ok(());
                }
                this.apply_refresh(me, services, api.host(), cx)
            })?
        })
    }

    fn apply_refresh(
        &mut self,
        me: Result<User, ApiError>,
        services: Result<ServicesResponse, ApiError>,
        host: &str,
        cx: &mut Context<Self>,
    ) -> Result<()> {
        let unauthorized = matches!(&me, Err(error) if error.is_unauthorized())
            || matches!(&services, Err(error) if error.is_unauthorized());
        if unauthorized {
            self.handle_unauthorized(cx);
            return Err(anyhow!("OTHCloud session expired. Sign in again."));
        }

        let was_signed_in = self.is_signed_in();
        let mut first_error = None;

        match me {
            Ok(user) => {
                if self.user.as_ref() != Some(&user) {
                    cx.spawn({
                        let user = user.clone();
                        async move |_, cx| storage::write_cached_user(&user, cx).await
                    })
                    .detach_and_log_err(cx);
                    self.user = Some(user);
                    cx.emit(OthcloudAccountEvent::UserChanged);
                }
            }
            Err(error) => first_error = Some(error),
        }

        match services {
            Ok(services) => {
                let now = Instant::now();
                self.services = Some(services);
                self.services_state = LoadState::Loaded { at: now };
                self.last_refreshed = Some(now);
                cx.emit(OthcloudAccountEvent::ServicesChanged);
            }
            Err(error) => {
                self.services_state = LoadState::Error {
                    message: error.friendly_message(host),
                    stale: self.services.is_some(),
                };
                first_error.get_or_insert(error);
            }
        }

        if !was_signed_in && self.is_signed_in() {
            cx.emit(OthcloudAccountEvent::SignedIn);
        }
        cx.notify();

        match first_error {
            Some(error) => {
                self.schedule_retry(cx);
                Err(anyhow!(error.friendly_message(host)))
            }
            None => {
                self.retry_backoff_ix = 0;
                Ok(())
            }
        }
    }

    fn schedule_retry(&mut self, cx: &mut Context<Self>) {
        let delay = RETRY_BACKOFF
            .get(self.retry_backoff_ix)
            .or(RETRY_BACKOFF.last())
            .copied()
            .unwrap_or(POLL_INTERVAL);
        self.retry_backoff_ix = (self.retry_backoff_ix + 1).min(RETRY_BACKOFF.len() - 1);
        self.retry_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(delay).await;
            this.update(cx, |this, cx| {
                this.retry_task = None;
                this.refresh(cx).detach();
            })
            .ok();
        }));
    }

    fn set_session(&mut self, token: String, user: Option<User>, cx: &mut Context<Self>) {
        self.api = Some(Arc::new(OthcloudApi::new(
            cx.http_client(),
            base_url(),
            token.clone(),
        )));
        self.token = Some(token);
        self.user = user;
        self.retry_backoff_ix = 0;
        self.retry_task = None;
        cx.notify();
    }

    fn clear_session(&mut self, cx: &mut Context<Self>) {
        let had_session = self.token.is_some() || self.user.is_some();
        self.user = None;
        self.token = None;
        self.api = None;
        self.services = None;
        self.services_state = LoadState::Idle;
        self.last_refreshed = None;
        self.retry_backoff_ix = 0;
        self.retry_task = None;
        if had_session {
            cx.emit(OthcloudAccountEvent::SignedOut);
            cx.emit(OthcloudAccountEvent::ServicesChanged);
        }
        cx.notify();
    }
}

/// Returns the user-facing error message on failure.
async fn exchange_and_store(
    http: Arc<dyn http_client::HttpClient>,
    base: &str,
    code: &str,
    cx: &AsyncApp,
) -> std::result::Result<(String, User), String> {
    let host = host_of(base).to_string();
    let response = cx
        .background_spawn({
            let base = base.to_string();
            let code = code.to_string();
            async move { OthcloudApi::exchange_pairing_code(http, &base, &code).await }
        })
        .await
        .map_err(|error| pairing_error_message(&error, &host))?;
    let token = response.token.trim().to_string();
    if token.is_empty() {
        return Err("OTHCloud did not return a token.".to_string());
    }
    storage::write_token(&token, cx)
        .await
        .map_err(|error| format!("could not save the sign-in token: {error}"))?;
    storage::write_cached_user(&response.user, cx)
        .await
        .log_err();
    Ok((token, response.user))
}

fn pairing_error_message(error: &ApiError, host: &str) -> String {
    match error.code.as_str() {
        "invalid_code" | "missing_code" => "that pairing code is not valid.".to_string(),
        "code_expired" => "that pairing code has expired. Start the sign-in again.".to_string(),
        "code_already_used" => {
            "that pairing code was already used. Start the sign-in again.".to_string()
        }
        _ => error.friendly_message(host),
    }
}

async fn clear_stored_session(cx: &AsyncApp) -> Result<()> {
    let token_result = storage::delete_token(cx).await;
    storage::delete_cached_user(cx).await.log_err();
    token_result
}

fn show_notification(message: String, cx: &mut App) {
    show_app_notification(NotificationId::unique::<OthcloudAccount>(), cx, move |cx| {
        let message = message.clone();
        cx.new(|cx| MessageNotification::new(message, cx))
    });
}
