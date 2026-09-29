use std::{collections::HashMap, fmt, sync::Arc};

use futures::AsyncReadExt as _;
use http_client::{AsyncBody, HttpClient, Method, Request};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::{Value, json};

use crate::base_url::join_url;

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct User {
    pub id: String,
    pub email: String,
    pub name: Option<String>,
    pub avatar_url: Option<String>,
    pub role: Option<String>,
    pub org_role: Option<String>,
    pub is_owner: bool,
    pub is_admin: bool,
    pub is_platform_admin: bool,
    pub is_developer: bool,
}

impl User {
    pub fn display_name(&self) -> &str {
        self.name
            .as_deref()
            .map(str::trim)
            .filter(|name| !name.is_empty())
            .unwrap_or(&self.email)
    }

    pub fn role_label(&self) -> Option<String> {
        if self.is_platform_admin {
            return Some("Platform Admin".to_string());
        }
        if self.is_owner {
            return Some("Owner".to_string());
        }
        if self.is_admin {
            return Some("Admin".to_string());
        }
        if self.is_developer {
            return Some("Developer".to_string());
        }
        let role = self
            .org_role
            .as_deref()
            .or(self.role.as_deref())
            .map(str::trim)
            .filter(|role| !role.is_empty())?;
        if role.eq_ignore_ascii_case("user") || role.eq_ignore_ascii_case("member") {
            return None;
        }
        let mut chars = role.chars();
        let first = chars.next()?;
        Some(first.to_uppercase().chain(chars).collect())
    }

    /// OTHCloud sometimes prefixes an inline `data:image/...` avatar with its own
    /// origin (`https://othcloud.xyz/data:image/png;base64,...`); strip that prefix.
    pub fn fixed_avatar_url(&self) -> Option<String> {
        let url = self.avatar_url.as_deref()?.trim();
        if url.is_empty() {
            return None;
        }
        if url.contains("/data:image")
            && let Some(start) = url.find("data:")
        {
            return Some(url[start..].to_string());
        }
        Some(url.to_string())
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Row {
    pub id: String,
    pub name: String,
    pub status: Option<String>,
    pub meta: HashMap<String, String>,
    pub url: Option<String>,
    pub children: Vec<Row>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ServicesResponse {
    pub projects: Vec<Row>,
    pub applications: Option<Vec<Row>>,
    pub game_servers: Option<Vec<Row>>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "state",
    rename_all = "lowercase",
    rename_all_fields = "camelCase"
)]
pub enum DevEnvState {
    Running {
        #[serde(default)]
        version: Option<String>,
        #[serde(default)]
        installed_versions: Vec<String>,
    },
    Stopped {
        #[serde(default)]
        installed_versions: Vec<String>,
    },
    Unavailable {
        #[serde(default)]
        reason: String,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DevEnvStatus {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub toolchain: Option<String>,
    #[serde(flatten)]
    pub state: DevEnvState,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ServerTerminalProfile {
    pub id: String,
    pub name: String,
    /// One of `all`, `linux`, `osx` or `windows`.
    pub platform: String,
    pub path: String,
    pub args: Option<Vec<String>>,
    pub env: Option<HashMap<String, Option<String>>>,
    pub icon: Option<String>,
    pub color: Option<String>,
    pub sort_order: Option<i64>,
    pub updated_at: Option<String>,
}

impl ServerTerminalProfile {
    pub fn applies_to_current_platform(&self) -> bool {
        let platform = self.platform.trim();
        platform.is_empty()
            || platform.eq_ignore_ascii_case("all")
            || platform.eq_ignore_ascii_case(current_platform())
    }
}

/// The platform name OTHCloud uses for terminal profiles on this OS.
pub fn current_platform() -> &'static str {
    if cfg!(target_os = "windows") {
        "windows"
    } else if cfg!(target_os = "macos") {
        "osx"
    } else {
        "linux"
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NewTerminalProfile {
    pub name: String,
    pub platform: String,
    pub path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub args: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub env: Option<HashMap<String, Option<String>>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub icon: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub color: Option<String>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum GithubTokenKind {
    User,
    #[default]
    Installation,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct GithubTokenResponse {
    pub kind: GithubTokenKind,
    pub token: String,
    pub expires_at: Option<String>,
    pub github_id: Option<Value>,
    pub login: Option<String>,
    pub account_id: Option<String>,
    pub app_name: Option<String>,
    /// For a git provider: its name in OTHCloud (Settings > Git).
    pub label: Option<String>,
    pub needs_connect: bool,
    pub connect_path: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct GithubAccount {
    pub id: String,
    pub kind: GithubTokenKind,
    /// What OTHCloud calls it: a git provider's name in Settings > Git, or a
    /// linked GitHub account's login.
    pub label: String,
    pub login: Option<String>,
    pub github_id: Value,
    pub avatar_url: Option<String>,
    pub can_push: bool,
    pub deployable: bool,
    /// Git providers (`app:` ids): the provider's name in OTHCloud.
    pub provider_name: Option<String>,
    /// GitHub App installations: the App's name.
    pub app_name: Option<String>,
    /// Git providers: the organization they belong to, when the user's
    /// providers span several.
    pub organization_name: Option<String>,
}

impl GithubAccount {
    /// Whether this entry is a git provider from Settings > Git (as opposed
    /// to a GitHub account linked to the OTHCloud user).
    pub fn is_git_provider(&self) -> bool {
        self.id.starts_with("app:")
    }
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct GithubAccountsResponse {
    pub accounts: Vec<GithubAccount>,
    pub connect_path: Option<String>,
    pub connect_available: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PairResponse {
    pub token: String,
    pub user: User,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ApiError {
    /// The HTTP status, or 0 when the request never got a response.
    pub status: u16,
    /// The `error` field of OTHCloud's JSON error body, `network` for transport
    /// failures, or `HTTP <status>` when the body had none.
    pub code: String,
    /// The `message` field of OTHCloud's JSON error body, when it sent one.
    pub message: Option<String>,
}

impl ApiError {
    pub fn new(status: u16, code: impl Into<String>) -> Self {
        Self {
            status,
            code: code.into(),
            message: None,
        }
    }

    pub fn network() -> Self {
        Self::new(0, "network")
    }

    pub fn is_unauthorized(&self) -> bool {
        self.status == 401
    }

    pub fn is_network(&self) -> bool {
        self.status == 0
    }

    pub fn friendly_message(&self, host: &str) -> String {
        let message = self
            .message
            .as_deref()
            .map(str::trim)
            .filter(|message| !message.is_empty());
        match (self.status, message) {
            (0, _) => format!("Can't reach {host}"),
            (403, _) => "You don't have access to this on OTHCloud.".to_string(),
            (500..=599, Some(message)) => {
                format!("OTHCloud is having trouble right now ({message}).")
            }
            (500..=599, None) => "OTHCloud is having trouble right now.".to_string(),
            (_, Some(message)) => message.to_string(),
            (_, None) => self.code.clone(),
        }
    }
}

impl fmt::Display for ApiError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.status == 0 {
            write!(formatter, "OTHCloud request failed: {}", self.code)
        } else {
            write!(
                formatter,
                "OTHCloud request failed with HTTP {}: {}",
                self.status, self.code
            )
        }
    }
}

impl std::error::Error for ApiError {}

/// An authenticated client for OTHCloud's `/api/desktop/*` endpoints.
///
/// Every method is `Send`, so callers typically clone the `Arc` and run requests
/// with `cx.background_spawn`. A 401 means the desktop token was revoked; callers
/// must then call [`crate::OthcloudAccount::handle_unauthorized`].
pub struct OthcloudApi {
    http: Arc<dyn HttpClient>,
    base: String,
    token: String,
}

impl OthcloudApi {
    pub fn new(http: Arc<dyn HttpClient>, base: String, token: String) -> Self {
        Self { http, base, token }
    }

    pub fn base(&self) -> &str {
        &self.base
    }

    pub fn host(&self) -> &str {
        crate::base_url::host_of(&self.base)
    }

    pub fn http_client(&self) -> Arc<dyn HttpClient> {
        self.http.clone()
    }

    pub fn url(&self, path_or_url: &str) -> String {
        join_url(&self.base, path_or_url)
    }

    pub async fn request<T: DeserializeOwned>(
        &self,
        method: Method,
        path: &str,
        body: Option<Value>,
    ) -> Result<T, ApiError> {
        send_json(
            self.http.as_ref(),
            method,
            self.url(path),
            Some(&self.token),
            body,
        )
        .await
    }

    pub async fn me(&self) -> Result<User, ApiError> {
        self.request(Method::GET, "/api/desktop/me", None).await
    }

    pub async fn services(&self) -> Result<ServicesResponse, ApiError> {
        self.request(Method::GET, "/api/desktop/services", None)
            .await
    }

    pub async fn profiles(&self) -> Result<Vec<ServerTerminalProfile>, ApiError> {
        #[derive(Deserialize)]
        struct ProfilesResponse {
            #[serde(default)]
            profiles: Vec<ServerTerminalProfile>,
        }

        let response: ProfilesResponse = self
            .request(Method::GET, "/api/desktop/profiles", None)
            .await?;
        Ok(response.profiles)
    }

    pub async fn save_profile(
        &self,
        profile: NewTerminalProfile,
    ) -> Result<ServerTerminalProfile, ApiError> {
        let body = serde_json::to_value(profile)
            .map_err(|error| ApiError::new(0, format!("invalid_profile: {error}")))?;
        self.request(Method::POST, "/api/desktop/profiles", Some(body))
            .await
    }

    pub async fn delete_profile(&self, id: &str) -> Result<(), ApiError> {
        let path = format!("/api/desktop/profiles/{}", urlencoding::encode(id));
        self.request::<serde::de::IgnoredAny>(Method::DELETE, &path, None)
            .await?;
        Ok(())
    }

    pub async fn dev_env_status(&self, application_id: &str) -> Result<DevEnvStatus, ApiError> {
        self.request(Method::GET, &dev_env_path(application_id, ""), None)
            .await
    }

    pub async fn dev_env_start(
        &self,
        application_id: &str,
        version: Option<String>,
        recreate: bool,
    ) -> Result<DevEnvStatus, ApiError> {
        let mut body = json!({ "recreate": recreate });
        if let Some(version) = version {
            body["version"] = Value::String(version);
        }
        self.request(
            Method::POST,
            &dev_env_path(application_id, "/start"),
            Some(body),
        )
        .await
    }

    pub async fn dev_env_stop(&self, application_id: &str) -> Result<(), ApiError> {
        self.request::<serde::de::IgnoredAny>(
            Method::POST,
            &dev_env_path(application_id, "/stop"),
            Some(json!({})),
        )
        .await?;
        Ok(())
    }

    /// A short-lived GitHub token for git operations. `account` is a GitHub
    /// account id from [`Self::github_accounts`] (`user:<id>` / `app:<id>`).
    pub async fn github_token(
        &self,
        account: Option<&str>,
    ) -> Result<GithubTokenResponse, ApiError> {
        let path = match account {
            Some(account) => format!(
                "/api/desktop/github-token?account={}",
                urlencoding::encode(account)
            ),
            None => "/api/desktop/github-token".to_string(),
        };
        self.request(Method::GET, &path, None).await
    }

    /// Returns `None` when this OTHCloud deployment predates multi-account support.
    pub async fn github_accounts(&self) -> Result<Option<GithubAccountsResponse>, ApiError> {
        match self
            .request(Method::GET, "/api/desktop/github-accounts", None)
            .await
        {
            Ok(response) => Ok(Some(response)),
            Err(error) if error.status == 404 || error.status == 405 => Ok(None),
            Err(error) => Err(error),
        }
    }

    /// Saves a GitHub token (OAuth, classic or fine-grained PAT) as one of the
    /// user's GitHub accounts on OTHCloud. With `deploy_provider`, OTHCloud
    /// also registers it as a GitHub deploy provider so it can deploy from it.
    pub async fn add_github_account(
        &self,
        token: &str,
        deploy_provider: bool,
    ) -> Result<GithubAccount, ApiError> {
        #[derive(Deserialize)]
        struct AddAccountResponse {
            account: GithubAccount,
        }

        let response: AddAccountResponse = self
            .request(
                Method::POST,
                "/api/desktop/github-accounts",
                Some(json!({ "token": token, "deployProvider": deploy_provider })),
            )
            .await?;
        Ok(response.account)
    }

    pub async fn remove_github_account(&self, id: &str) -> Result<(), ApiError> {
        let path = format!(
            "/api/desktop/github-accounts?account={}",
            urlencoding::encode(id)
        );
        self.request::<serde::de::IgnoredAny>(Method::DELETE, &path, None)
            .await?;
        Ok(())
    }

    /// Whether "Sign in with GitHub" is configured on this OTHCloud deployment.
    /// Any failure answers `true` so the UI keeps offering the option.
    pub async fn social_providers_github(&self) -> bool {
        match self
            .request::<Value>(Method::GET, "/api/trpc/settings.socialProviders", None)
            .await
        {
            Ok(response) => response
                .pointer("/result/data/json/github")
                .or_else(|| response.pointer("/result/data/github"))
                .and_then(Value::as_bool)
                .unwrap_or(true),
            Err(error) => {
                log::debug!("could not read OTHCloud social providers: {error}");
                true
            }
        }
    }

    /// Trades a one-shot pairing code from `/desktop-pair` for a long-lived
    /// desktop token. Unauthenticated: the code is the credential.
    pub async fn exchange_pairing_code(
        http: Arc<dyn HttpClient>,
        base: &str,
        code: &str,
    ) -> Result<PairResponse, ApiError> {
        send_json(
            http.as_ref(),
            Method::POST,
            join_url(base, "/api/desktop/token"),
            None,
            Some(json!({ "code": code })),
        )
        .await
    }
}

fn dev_env_path(application_id: &str, suffix: &str) -> String {
    format!(
        "/api/desktop/dev-environments/{}{suffix}",
        urlencoding::encode(application_id)
    )
}

async fn send_json<T: DeserializeOwned>(
    http: &dyn HttpClient,
    method: Method,
    url: String,
    token: Option<&str>,
    body: Option<Value>,
) -> Result<T, ApiError> {
    let mut builder = Request::builder()
        .method(method.clone())
        .uri(url.as_str())
        .header("Accept", "application/json");
    if let Some(token) = token {
        builder = builder.header("Authorization", format!("Bearer {token}"));
    }
    let body = match body {
        Some(body) => {
            builder = builder.header("Content-Type", "application/json");
            AsyncBody::from(body.to_string())
        }
        None => AsyncBody::empty(),
    };
    let request = builder
        .body(body)
        .map_err(|error| ApiError::new(0, format!("invalid_request: {error}")))?;

    let mut response = http.send(request).await.map_err(|error| {
        log::warn!("OTHCloud {method} {url} failed: {error:#}");
        ApiError::network()
    })?;
    let status = response.status();
    let mut bytes = Vec::new();
    response
        .body_mut()
        .read_to_end(&mut bytes)
        .await
        .map_err(|error| {
            log::warn!("OTHCloud {method} {url}: reading the response failed: {error}");
            ApiError::network()
        })?;

    if !status.is_success() {
        let error = parse_error_body(status.as_u16(), &bytes);
        log::debug!(
            "OTHCloud {method} {url} returned {}: {} {}",
            status.as_u16(),
            error.code,
            error.message.as_deref().unwrap_or_default()
        );
        return Err(error);
    }

    parse_success_body(status.as_u16(), &bytes).map_err(|error| {
        log::warn!("OTHCloud {method} {url}: unexpected response: {error}");
        ApiError {
            status: status.as_u16(),
            code: "invalid_response".to_string(),
            message: Some(format!("OTHCloud sent an unexpected response: {error}")),
        }
    })
}

/// An error response: `{ error, message? }` from OTHCloud, or whatever else
/// the server (or a proxy in front of it) sent.
fn parse_error_body(status: u16, bytes: &[u8]) -> ApiError {
    #[derive(Deserialize)]
    struct ErrorBody {
        #[serde(default)]
        error: Option<String>,
        #[serde(default)]
        message: Option<String>,
    }

    let body = serde_json::from_slice::<ErrorBody>(bytes).ok();
    let (code, message) = body
        .map(|body| (body.error, body.message))
        .unwrap_or_default();
    ApiError {
        status,
        code: code
            .filter(|code| !code.is_empty())
            .unwrap_or_else(|| format!("HTTP {status}")),
        message: message.filter(|message| !message.trim().is_empty()),
    }
}

fn parse_success_body<T: DeserializeOwned>(status: u16, bytes: &[u8]) -> serde_json::Result<T> {
    if status == 204 || bytes.iter().all(u8::is_ascii_whitespace) {
        serde_json::from_slice(b"null")
    } else {
        serde_json::from_slice(bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_services_with_and_without_applications() {
        let with_applications: ServicesResponse = serde_json::from_str(
            r#"{
                "projects": [{
                    "id": "p1",
                    "name": "Website",
                    "url": "/dashboard/project/p1",
                    "children": [{ "id": "e1", "name": "production", "url": "/dashboard/project/p1/environment/e1" }]
                }],
                "applications": [{
                    "id": "a1",
                    "name": "api",
                    "meta": { "project": "Website / production", "toolchain": "node" },
                    "url": "/dashboard/x?tab=editor"
                }],
                "gameServers": [{
                    "id": "g1",
                    "name": "Survival",
                    "status": "deployed",
                    "meta": { "type": "minecraft", "address": "mc.example.com:25565" },
                    "url": "/dashboard/games/g1"
                }]
            }"#,
        )
        .expect("services with applications should parse");
        assert_eq!(with_applications.projects.len(), 1);
        assert_eq!(with_applications.projects[0].children[0].name, "production");
        assert_eq!(with_applications.projects[0].status, None);
        let applications = with_applications
            .applications
            .as_ref()
            .expect("applications");
        assert_eq!(
            applications[0].meta.get("toolchain").map(String::as_str),
            Some("node")
        );
        let game_servers = with_applications.game_servers.as_ref().expect("games");
        assert_eq!(game_servers[0].status.as_deref(), Some("deployed"));

        let round_trip: ServicesResponse =
            serde_json::from_value(serde_json::to_value(&with_applications).expect("serialize"))
                .expect("deserialize");
        assert_eq!(round_trip, with_applications);

        let without_applications: ServicesResponse =
            serde_json::from_str(r#"{ "projects": [{ "id": "p1", "name": "Website" }] }"#)
                .expect("services without applications should parse");
        assert_eq!(without_applications.applications, None);
        assert_eq!(without_applications.game_servers, None);
        assert!(without_applications.projects[0].children.is_empty());
        assert!(without_applications.projects[0].meta.is_empty());
    }

    #[test]
    fn test_github_token_without_kind() {
        let response: GithubTokenResponse = serde_json::from_str(
            r#"{ "token": "ghs_abc", "expiresAt": "2026-01-01T00:00:00.000Z", "githubId": 42 }"#,
        )
        .expect("parse");
        assert_eq!(response.kind, GithubTokenKind::Installation);
        assert_eq!(response.token, "ghs_abc");
        assert_eq!(response.github_id, Some(json!(42)));
        assert!(!response.needs_connect);

        let user: GithubTokenResponse = serde_json::from_str(
            r#"{ "kind": "user", "token": "gho_x", "login": "octocat", "accountId": "user:1", "githubId": "583231", "connectPath": "/desktop-github" }"#,
        )
        .expect("parse");
        assert_eq!(user.kind, GithubTokenKind::User);
        assert_eq!(user.login.as_deref(), Some("octocat"));
        assert_eq!(user.account_id.as_deref(), Some("user:1"));
    }

    #[test]
    fn test_github_accounts() {
        let response: GithubAccountsResponse = serde_json::from_str(
            r#"{
                "accounts": [
                    { "id": "user:1", "kind": "user", "label": "octocat", "login": "octocat", "githubId": "583231", "canPush": true, "deployable": true },
                    { "id": "app:9", "kind": "installation", "label": "OTHCloud App", "githubId": "9", "canPush": true, "deployable": false },
                    { "id": "app:-O1pyAh7dhUoSKhmDZMVx", "kind": "installation", "label": "My deploy app", "providerName": "My deploy app", "appName": "othcloud-2026-04-01", "organizationName": "Acme", "githubId": "-O1pyAh7dhUoSKhmDZMVx", "canPush": false, "deployable": true }
                ],
                "connectPath": "/desktop-github",
                "connectAvailable": true
            }"#,
        )
        .expect("parse");
        assert_eq!(response.accounts.len(), 3);
        assert_eq!(response.accounts[0].kind, GithubTokenKind::User);
        assert!(!response.accounts[0].is_git_provider());
        assert_eq!(response.accounts[1].kind, GithubTokenKind::Installation);
        assert!(response.accounts[1].is_git_provider());
        assert_eq!(response.accounts[1].provider_name, None);
        let named = &response.accounts[2];
        assert_eq!(named.provider_name.as_deref(), Some("My deploy app"));
        assert_eq!(named.app_name.as_deref(), Some("othcloud-2026-04-01"));
        assert_eq!(named.organization_name.as_deref(), Some("Acme"));
        assert!(response.connect_available);
    }

    #[test]
    fn test_dev_env_status_variants() {
        let running: DevEnvStatus = serde_json::from_str(
            r#"{ "name": "api", "toolchain": "node", "state": "running", "version": "1.2.3", "installedVersions": ["1.2.3", "1.2.2"] }"#,
        )
        .expect("running");
        assert_eq!(
            running.state,
            DevEnvState::Running {
                version: Some("1.2.3".into()),
                installed_versions: vec!["1.2.3".into(), "1.2.2".into()],
            }
        );
        assert_eq!(running.toolchain.as_deref(), Some("node"));

        let stopped: DevEnvStatus = serde_json::from_str(
            r#"{ "name": "api", "toolchain": null, "state": "stopped", "installedVersions": [] }"#,
        )
        .expect("stopped");
        assert_eq!(
            stopped.state,
            DevEnvState::Stopped {
                installed_versions: Vec::new()
            }
        );
        assert_eq!(stopped.toolchain, None);

        let unavailable: DevEnvStatus = serde_json::from_str(
            r#"{ "name": "api", "state": "unavailable", "reason": "no_server" }"#,
        )
        .expect("unavailable");
        assert_eq!(
            unavailable.state,
            DevEnvState::Unavailable {
                reason: "no_server".into()
            }
        );

        for status in [running, stopped, unavailable] {
            let round_trip: DevEnvStatus =
                serde_json::from_value(serde_json::to_value(&status).expect("serialize"))
                    .expect("deserialize");
            assert_eq!(round_trip, status);
        }
    }

    #[test]
    fn test_user_helpers() {
        let user: User = serde_json::from_str(
            r#"{ "id": "u1", "email": "a@b.c", "name": "", "avatarUrl": "https://othcloud.xyz/data:image/png;base64,AAA", "role": "user", "orgRole": "member", "isOwner": false, "isAdmin": false, "isPlatformAdmin": false, "isDeveloper": false }"#,
        )
        .expect("parse");
        assert_eq!(user.display_name(), "a@b.c");
        assert_eq!(user.role_label(), None);
        assert_eq!(
            user.fixed_avatar_url().as_deref(),
            Some("data:image/png;base64,AAA")
        );

        let owner = User {
            is_owner: true,
            is_admin: true,
            ..user.clone()
        };
        assert_eq!(owner.role_label().as_deref(), Some("Owner"));

        let billing = User {
            org_role: Some("billing".into()),
            ..user
        };
        assert_eq!(billing.role_label().as_deref(), Some("Billing"));

        let pair: PairResponse = serde_json::from_str(
            r#"{ "token": "t", "user": { "id": "u1", "email": "a@b.c", "name": "Ann", "orgRole": null } }"#,
        )
        .expect("pair");
        assert_eq!(pair.user.display_name(), "Ann");
    }

    #[test]
    fn test_terminal_profiles() {
        let profile: ServerTerminalProfile = serde_json::from_str(
            r#"{ "id": "1", "name": "Claude", "platform": "all", "path": "claude", "args": null, "env": { "A": "1", "B": null }, "icon": null, "color": null, "sortOrder": 0, "updatedAt": "2026-01-01T00:00:00.000Z" }"#,
        )
        .expect("parse");
        assert!(profile.applies_to_current_platform());
        assert_eq!(
            profile.env.as_ref().and_then(|env| env.get("B")).cloned(),
            Some(None)
        );
        let other = ServerTerminalProfile {
            platform: if cfg!(target_os = "windows") {
                "linux".into()
            } else {
                "windows".into()
            },
            ..profile
        };
        assert!(!other.applies_to_current_platform());

        let new_profile = NewTerminalProfile {
            name: "pwsh".into(),
            platform: "windows".into(),
            path: "pwsh.exe".into(),
            ..Default::default()
        };
        assert_eq!(
            serde_json::to_value(&new_profile).expect("serialize"),
            json!({ "name": "pwsh", "platform": "windows", "path": "pwsh.exe" })
        );
    }

    #[test]
    fn test_success_body_parsing() {
        assert!(parse_success_body::<serde::de::IgnoredAny>(204, b"").is_ok());
        assert!(parse_success_body::<serde::de::IgnoredAny>(200, b"").is_ok());
        assert!(parse_success_body::<serde::de::IgnoredAny>(200, br#"{"ok":true}"#).is_ok());
        assert_eq!(
            parse_success_body::<Option<User>>(200, b"  ").ok(),
            Some(None)
        );
    }

    #[test]
    fn test_friendly_messages() {
        let host = "othcloud.xyz";
        assert_eq!(
            ApiError::network().friendly_message(host),
            "Can't reach othcloud.xyz"
        );
        let forbidden = ApiError::new(403, "forbidden");
        assert_eq!(
            forbidden.friendly_message(host),
            "You don't have access to this on OTHCloud."
        );
        let server = ApiError::new(502, "github_unavailable");
        assert_eq!(
            server.friendly_message(host),
            "OTHCloud is having trouble right now."
        );
        let server_with_message = parse_error_body(
            500,
            br#"{ "error": "list_failed", "message": "column \"githubAccessToken\" does not exist" }"#,
        );
        assert_eq!(server_with_message.code, "list_failed");
        assert_eq!(
            server_with_message.friendly_message(host),
            "OTHCloud is having trouble right now (column \"githubAccessToken\" does not exist)."
        );
        let other = ApiError::new(400, "invalid_code");
        assert_eq!(other.friendly_message(host), "invalid_code");
        let other_with_message = parse_error_body(
            400,
            br#"{ "error": "missing_repo_scope", "message": "The token needs the `repo` scope." }"#,
        );
        assert_eq!(
            other_with_message.friendly_message(host),
            "The token needs the `repo` scope."
        );
        let html = parse_error_body(404, b"<html>Not Found</html>");
        assert_eq!(html, ApiError::new(404, "HTTP 404"));
        assert!(ApiError::new(401, "unauthorized").is_unauthorized());
    }
}
