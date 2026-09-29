//! A small GitHub client: token validation, repository listing and the OAuth
//! device flow. Talks to github.com / api.github.com only.

use std::{fmt, sync::Arc};

use futures::AsyncReadExt as _;
use http_client::{AsyncBody, HttpClient, Method, Request};
use serde::{Deserialize, Serialize, de::DeserializeOwned};

pub const GITHUB_API: &str = "https://api.github.com";
pub const GITHUB_WEB: &str = "https://github.com";
/// The scopes OTerminal asks for: clone/push private repositories, push
/// workflow files, and see organization repositories.
pub const REQUESTED_SCOPES: &str = "repo workflow read:org";
/// Where a user creates a classic personal access token with those scopes.
pub const CREATE_TOKEN_URL: &str =
    "https://github.com/settings/tokens/new?scopes=repo,workflow,read:org&description=OTerminal";

const PER_PAGE: usize = 100;
const MAX_PAGES: usize = 10;

/// Something that went wrong talking to GitHub, with a user-facing message.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GithubError {
    /// 401: the token is wrong, expired or revoked.
    Unauthorized,
    /// A classic/OAuth token without the `repo` scope.
    MissingRepoScope {
        scopes: Vec<String>,
    },
    RateLimited,
    /// 403 for another reason, with GitHub's message.
    Forbidden(String),
    NotFound,
    /// The request never got an answer.
    Network(String),
    Http(u16),
    InvalidResponse(String),
}

impl GithubError {
    pub fn message(&self) -> String {
        match self {
            GithubError::Unauthorized => {
                "GitHub rejected the token: it expired or was revoked. Sign in again.".to_string()
            }
            GithubError::MissingRepoScope { scopes } => {
                let granted = if scopes.is_empty() {
                    "none".to_string()
                } else {
                    scopes.join(", ")
                };
                format!(
                    "The token is missing the \"repo\" scope (it has: {granted}). \
                     Create a token with repo, workflow and read:org."
                )
            }
            GithubError::RateLimited => {
                "GitHub's rate limit was hit. Try again in a few minutes.".to_string()
            }
            GithubError::Forbidden(message) if !message.is_empty() => {
                format!("GitHub refused the request: {message}")
            }
            GithubError::Forbidden(_) => "GitHub refused the request.".to_string(),
            GithubError::NotFound => "GitHub couldn't find that.".to_string(),
            GithubError::Network(_) => {
                "Can't reach GitHub. Check your internet connection.".to_string()
            }
            GithubError::Http(status) => format!("GitHub returned an error (HTTP {status})."),
            GithubError::InvalidResponse(_) => "GitHub sent an unexpected response.".to_string(),
        }
    }
}

impl fmt::Display for GithubError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            GithubError::Network(detail) | GithubError::InvalidResponse(detail) => {
                write!(formatter, "{} ({detail})", self.message())
            }
            _ => formatter.write_str(&self.message()),
        }
    }
}

impl std::error::Error for GithubError {}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GithubUser {
    pub login: String,
    pub id: u64,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub avatar_url: Option<String>,
}

/// A token GitHub accepted, and who it belongs to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ValidatedToken {
    pub user: GithubUser,
    /// The OAuth scopes of a classic PAT / OAuth token; `None` for tokens that
    /// don't report scopes (fine-grained PATs).
    pub scopes: Option<Vec<String>>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct GithubRepoOwner {
    #[serde(default)]
    pub login: String,
}

#[derive(Clone, Debug, Deserialize)]
pub struct GithubRepo {
    pub full_name: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub private: bool,
    #[serde(default)]
    pub fork: bool,
    #[serde(default)]
    pub archived: bool,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub clone_url: Option<String>,
    #[serde(default)]
    pub owner: Option<GithubRepoOwner>,
}

impl GithubRepo {
    pub fn clone_url(&self) -> String {
        self.clone_url
            .clone()
            .filter(|url| !url.is_empty())
            .unwrap_or_else(|| format!("{GITHUB_WEB}/{}.git", self.full_name))
    }

    pub fn directory_name(&self) -> String {
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

/// Parses GitHub's `X-OAuth-Scopes` header (`"repo, workflow, read:org"`).
pub fn parse_scopes(header: Option<&str>) -> Option<Vec<String>> {
    let header = header?;
    Some(
        header
            .split(',')
            .map(|scope| scope.trim().to_string())
            .filter(|scope| !scope.is_empty())
            .collect(),
    )
}

/// Checks that a token can clone and push private repositories.
///
/// Fine-grained tokens (`github_pat_…`) don't report scopes — their access is
/// per repository — so they are accepted as is, as are tokens that report no
/// scope header at all.
pub fn check_token_scopes(token: &str, scopes: Option<&[String]>) -> Result<(), GithubError> {
    if token.starts_with("github_pat_") {
        return Ok(());
    }
    let Some(scopes) = scopes else {
        return Ok(());
    };
    if scopes.iter().any(|scope| scope == "repo") {
        Ok(())
    } else {
        Err(GithubError::MissingRepoScope {
            scopes: scopes.to_vec(),
        })
    }
}

/// Maps an unsuccessful GitHub response to an error.
pub fn classify_failure(
    status: u16,
    rate_limit_remaining: Option<&str>,
    body: &[u8],
) -> GithubError {
    #[derive(Deserialize)]
    struct ErrorBody {
        #[serde(default)]
        message: String,
    }
    let message = serde_json::from_slice::<ErrorBody>(body)
        .map(|body| body.message)
        .unwrap_or_default();
    match status {
        401 => GithubError::Unauthorized,
        403 | 429
            if rate_limit_remaining.is_some_and(|remaining| remaining.trim() == "0")
                || message.to_lowercase().contains("rate limit") =>
        {
            GithubError::RateLimited
        }
        429 => GithubError::RateLimited,
        403 => GithubError::Forbidden(message),
        404 => GithubError::NotFound,
        status => GithubError::Http(status),
    }
}

/// Cleans up a pasted token: whitespace, quotes, and a `token `/`Bearer ` prefix.
pub fn normalize_token(input: &str) -> String {
    let token = input.trim().trim_matches(|c| c == '"' || c == '\'').trim();
    let token = token
        .strip_prefix("Bearer ")
        .or_else(|| token.strip_prefix("bearer "))
        .or_else(|| token.strip_prefix("token "))
        .unwrap_or(token);
    token.trim().to_string()
}

struct RawResponse {
    status: u16,
    scopes: Option<String>,
    rate_limit_remaining: Option<String>,
    body: Vec<u8>,
}

async fn send(
    http: &Arc<dyn HttpClient>,
    method: Method,
    url: &str,
    token: Option<&str>,
    form: Option<String>,
) -> Result<RawResponse, GithubError> {
    let mut builder = Request::builder()
        .method(method)
        .uri(url)
        .header("Accept", "application/vnd.github+json")
        .header("X-GitHub-Api-Version", "2022-11-28")
        .header("User-Agent", "OTerminal");
    if let Some(token) = token {
        builder = builder.header("Authorization", format!("Bearer {token}"));
    }
    let body = match form {
        Some(form) => {
            builder = builder
                .header("Content-Type", "application/x-www-form-urlencoded")
                // The OAuth endpoints answer form-encoded unless asked for JSON.
                .header("Accept", "application/json");
            AsyncBody::from(form)
        }
        None => AsyncBody::default(),
    };
    let request = builder
        .body(body)
        .map_err(|error| GithubError::InvalidResponse(error.to_string()))?;
    let mut response = http
        .send(request)
        .await
        .map_err(|error| GithubError::Network(format!("{error:#}")))?;
    let header = |name: &str| {
        response
            .headers()
            .get(name)
            .and_then(|value| value.to_str().ok())
            .map(ToString::to_string)
    };
    let scopes = header("x-oauth-scopes");
    let rate_limit_remaining = header("x-ratelimit-remaining");
    let mut body = Vec::new();
    response
        .body_mut()
        .read_to_end(&mut body)
        .await
        .map_err(|error| GithubError::Network(error.to_string()))?;
    Ok(RawResponse {
        status: response.status().as_u16(),
        scopes,
        rate_limit_remaining,
        body,
    })
}

async fn get_json<T: DeserializeOwned>(
    http: &Arc<dyn HttpClient>,
    url: &str,
    token: &str,
) -> Result<(T, Option<String>), GithubError> {
    let response = send(http, Method::GET, url, Some(token), None).await?;
    if !(200..300).contains(&response.status) {
        return Err(classify_failure(
            response.status,
            response.rate_limit_remaining.as_deref(),
            &response.body,
        ));
    }
    let value = serde_json::from_slice(&response.body)
        .map_err(|error| GithubError::InvalidResponse(error.to_string()))?;
    Ok((value, response.scopes))
}

/// Asks GitHub who `token` belongs to and checks it can clone and push.
pub async fn validate_token(
    http: Arc<dyn HttpClient>,
    token: &str,
) -> Result<ValidatedToken, GithubError> {
    let (user, scopes) =
        get_json::<GithubUser>(&http, &format!("{GITHUB_API}/user"), token).await?;
    let scopes = parse_scopes(scopes.as_deref());
    check_token_scopes(token, scopes.as_deref())?;
    Ok(ValidatedToken { user, scopes })
}

/// Lists the repositories `token` can reach: for user tokens everything the
/// user owns, collaborates on or can see through an organization; for GitHub
/// App installation tokens the installation's repositories.
pub async fn list_repositories(
    http: Arc<dyn HttpClient>,
    token: &str,
) -> Result<Vec<GithubRepo>, GithubError> {
    let mut repos: Vec<GithubRepo> = Vec::new();
    let mut user_repos_failed = None;
    for page in 1..=MAX_PAGES {
        let url = format!(
            "{GITHUB_API}/user/repos?sort=pushed&per_page={PER_PAGE}&page={page}&affiliation=owner,collaborator,organization_member"
        );
        match get_json::<Vec<GithubRepo>>(&http, &url, token).await {
            Ok((batch, _)) => {
                let done = batch.len() < PER_PAGE;
                repos.extend(batch);
                if done {
                    break;
                }
            }
            Err(error @ (GithubError::Unauthorized | GithubError::Network(_))) => {
                return Err(error);
            }
            // Installation tokens can't call /user/repos (403).
            Err(error) if page == 1 => {
                user_repos_failed = Some(error);
                break;
            }
            Err(error) => return Err(error),
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
            match get_json::<InstallationRepos>(&http, &url, token).await {
                Ok((batch, _)) => {
                    let done = batch.repositories.len() < PER_PAGE;
                    repos.extend(batch.repositories);
                    if done {
                        break;
                    }
                }
                // Not an installation token: report the /user/repos failure.
                Err(error) if page == 1 => {
                    if let Some(user_error) = user_repos_failed {
                        return Err(user_error);
                    }
                    log::debug!("GitHub /installation/repositories failed: {error}");
                    break;
                }
                Err(error) => return Err(error),
            }
        }
    }

    let mut seen = std::collections::HashSet::new();
    repos.retain(|repo| seen.insert(repo.full_name.to_lowercase()));
    Ok(repos)
}

// ---------------------------------------------------------------------------
// OAuth device flow
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
pub struct DeviceCode {
    pub device_code: String,
    pub user_code: String,
    pub verification_uri: String,
    #[serde(default = "default_expires_in")]
    pub expires_in: u64,
    #[serde(default = "default_interval")]
    pub interval: u64,
}

fn default_expires_in() -> u64 {
    900
}

fn default_interval() -> u64 {
    5
}

/// One answer while polling for the device flow's token.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DevicePoll {
    /// The user hasn't finished in the browser yet.
    Pending,
    /// Polling too fast: wait this many seconds between polls from now on.
    SlowDown(u64),
    Token(String),
    /// The flow ended without a token, with a user-facing reason.
    Failed(String),
}

fn form_encode(pairs: &[(&str, &str)]) -> String {
    pairs
        .iter()
        .map(|(key, value)| format!("{key}={}", urlencoding::encode(value)))
        .collect::<Vec<_>>()
        .join("&")
}

/// Parses an answer of `POST /login/oauth/access_token` during the device flow.
pub fn parse_device_poll(body: &[u8], current_interval: u64) -> DevicePoll {
    #[derive(Deserialize)]
    struct PollBody {
        #[serde(default)]
        access_token: Option<String>,
        #[serde(default)]
        error: Option<String>,
        #[serde(default)]
        error_description: Option<String>,
        #[serde(default)]
        interval: Option<u64>,
    }
    let Ok(body) = serde_json::from_slice::<PollBody>(body) else {
        return DevicePoll::Failed("GitHub sent an unexpected response.".to_string());
    };
    if let Some(token) = body.access_token.filter(|token| !token.is_empty()) {
        return DevicePoll::Token(token);
    }
    match body.error.as_deref() {
        Some("authorization_pending") => DevicePoll::Pending,
        Some("slow_down") => DevicePoll::SlowDown(body.interval.unwrap_or(current_interval + 5)),
        Some("expired_token") => {
            DevicePoll::Failed("The code expired. Start the sign-in again.".to_string())
        }
        Some("access_denied") => DevicePoll::Failed("Sign-in was cancelled on GitHub.".to_string()),
        Some("device_flow_disabled") => DevicePoll::Failed(
            "Device flow is disabled for the configured GitHub OAuth App.".to_string(),
        ),
        Some("incorrect_client_credentials") => DevicePoll::Failed(
            "The configured GitHub OAuth client id (othcloud.github_oauth_client_id) is invalid."
                .to_string(),
        ),
        Some(error) => DevicePoll::Failed(
            body.error_description
                .filter(|description| !description.is_empty())
                .unwrap_or_else(|| format!("GitHub sign-in failed: {error}")),
        ),
        None => DevicePoll::Failed("GitHub sent an unexpected response.".to_string()),
    }
}

/// Starts the device flow for the OAuth App `client_id`.
pub async fn request_device_code(
    http: Arc<dyn HttpClient>,
    client_id: &str,
) -> Result<DeviceCode, GithubError> {
    let form = form_encode(&[("client_id", client_id), ("scope", REQUESTED_SCOPES)]);
    let response = send(
        &http,
        Method::POST,
        &format!("{GITHUB_WEB}/login/device/code"),
        None,
        Some(form),
    )
    .await?;
    if !(200..300).contains(&response.status) {
        return Err(classify_failure(
            response.status,
            response.rate_limit_remaining.as_deref(),
            &response.body,
        ));
    }
    // A bad client id answers 200 with `{"error": ...}`.
    if let DevicePoll::Failed(message) = parse_device_poll(&response.body, default_interval())
        && serde_json::from_slice::<DeviceCode>(&response.body).is_err()
    {
        return Err(GithubError::InvalidResponse(message));
    }
    serde_json::from_slice(&response.body)
        .map_err(|error| GithubError::InvalidResponse(error.to_string()))
}

/// Polls once for the device flow's token.
pub async fn poll_device_token(
    http: Arc<dyn HttpClient>,
    client_id: &str,
    device_code: &str,
    current_interval: u64,
) -> Result<DevicePoll, GithubError> {
    let form = form_encode(&[
        ("client_id", client_id),
        ("device_code", device_code),
        ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
    ]);
    let response = send(
        &http,
        Method::POST,
        &format!("{GITHUB_WEB}/login/oauth/access_token"),
        None,
        Some(form),
    )
    .await?;
    if !(200..300).contains(&response.status) {
        return Err(classify_failure(
            response.status,
            response.rate_limit_remaining.as_deref(),
            &response.body,
        ));
    }
    Ok(parse_device_poll(&response.body, current_interval))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scopes_header_parsing() {
        assert_eq!(
            parse_scopes(Some("repo, workflow,read:org ")),
            Some(vec![
                "repo".to_string(),
                "workflow".to_string(),
                "read:org".to_string()
            ])
        );
        assert_eq!(parse_scopes(Some("")), Some(vec![]));
        assert_eq!(parse_scopes(None), None);
    }

    #[test]
    fn token_scope_validation() {
        let scopes = |list: &[&str]| list.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(
            check_token_scopes("ghp_x", Some(&scopes(&["repo", "workflow"]))),
            Ok(())
        );
        assert_eq!(
            check_token_scopes("ghp_x", Some(&scopes(&["public_repo"]))),
            Err(GithubError::MissingRepoScope {
                scopes: scopes(&["public_repo"])
            })
        );
        assert!(matches!(
            check_token_scopes("gho_x", Some(&[])),
            Err(GithubError::MissingRepoScope { .. })
        ));
        // Fine-grained tokens have per-repository permissions instead.
        assert_eq!(check_token_scopes("github_pat_x", Some(&[])), Ok(()));
        assert_eq!(check_token_scopes("github_pat_x", None), Ok(()));
        // No header at all: can't tell, accept.
        assert_eq!(check_token_scopes("ghp_x", None), Ok(()));
    }

    #[test]
    fn failure_classification() {
        assert_eq!(
            classify_failure(401, None, b"{}"),
            GithubError::Unauthorized
        );
        assert_eq!(
            classify_failure(403, Some("0"), b"{}"),
            GithubError::RateLimited
        );
        assert_eq!(
            classify_failure(
                403,
                Some("12"),
                br#"{"message":"API rate limit exceeded for user"}"#
            ),
            GithubError::RateLimited
        );
        assert_eq!(
            classify_failure(
                403,
                Some("4999"),
                br#"{"message":"Resource not accessible by integration"}"#
            ),
            GithubError::Forbidden("Resource not accessible by integration".to_string())
        );
        assert_eq!(classify_failure(404, None, b""), GithubError::NotFound);
        assert_eq!(classify_failure(502, None, b"oops"), GithubError::Http(502));
        assert!(
            GithubError::Unauthorized
                .message()
                .contains("expired or was revoked")
        );
        assert!(
            GithubError::MissingRepoScope {
                scopes: vec!["gist".into()]
            }
            .message()
            .contains("\"repo\" scope (it has: gist)")
        );
    }

    #[test]
    fn token_normalization() {
        assert_eq!(normalize_token("  ghp_abc \n"), "ghp_abc");
        assert_eq!(normalize_token("\"ghp_abc\""), "ghp_abc");
        assert_eq!(normalize_token("Bearer ghp_abc"), "ghp_abc");
        assert_eq!(normalize_token("token github_pat_1"), "github_pat_1");
    }

    #[test]
    fn user_parsing() {
        let user: GithubUser = serde_json::from_str(
            r#"{ "login": "octocat", "id": 583231, "name": null, "avatar_url": "https://avatars.githubusercontent.com/u/583231?v=4", "type": "User" }"#,
        )
        .expect("parse");
        assert_eq!(user.login, "octocat");
        assert_eq!(user.id, 583231);
        assert_eq!(user.name, None);
    }

    #[test]
    fn device_poll_parsing() {
        assert_eq!(
            parse_device_poll(br#"{"error":"authorization_pending"}"#, 5),
            DevicePoll::Pending
        );
        assert_eq!(
            parse_device_poll(br#"{"error":"slow_down","interval":10}"#, 5),
            DevicePoll::SlowDown(10)
        );
        assert_eq!(
            parse_device_poll(br#"{"error":"slow_down"}"#, 5),
            DevicePoll::SlowDown(10)
        );
        assert_eq!(
            parse_device_poll(
                br#"{"access_token":"gho_abc","token_type":"bearer","scope":"repo"}"#,
                5
            ),
            DevicePoll::Token("gho_abc".to_string())
        );
        assert!(matches!(
            parse_device_poll(br#"{"error":"access_denied"}"#, 5),
            DevicePoll::Failed(message) if message.contains("cancelled")
        ));
        assert!(matches!(
            parse_device_poll(br#"{"error":"expired_token"}"#, 5),
            DevicePoll::Failed(message) if message.contains("expired")
        ));
        assert!(matches!(
            parse_device_poll(b"nope", 5),
            DevicePoll::Failed(_)
        ));

        let code: DeviceCode = serde_json::from_str(
            r#"{"device_code":"d","user_code":"WDJB-MJHT","verification_uri":"https://github.com/login/device","expires_in":899,"interval":5}"#,
        )
        .expect("parse");
        assert_eq!(code.user_code, "WDJB-MJHT");
        assert_eq!(code.interval, 5);
    }

    #[test]
    fn repo_names_and_urls() {
        let repos: Vec<GithubRepo> = serde_json::from_str(
            r#"[{ "full_name": "octo/hello", "name": "hello", "private": true, "clone_url": "https://github.com/octo/hello.git", "owner": { "login": "octo" } },
                { "full_name": "octo/world" }]"#,
        )
        .expect("parse");
        assert_eq!(repos[0].directory_name(), "hello");
        assert_eq!(repos[0].clone_url(), "https://github.com/octo/hello.git");
        assert_eq!(repos[1].directory_name(), "world");
        assert_eq!(repos[1].clone_url(), "https://github.com/octo/world.git");
    }
}
