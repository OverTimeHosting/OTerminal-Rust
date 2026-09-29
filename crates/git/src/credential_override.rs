//! A process-global hook that lets the application inject credentials into
//! every git subprocess Zed spawns (fetch, pull, push, clone, ...).
//!
//! OTerminal uses this to hand git the GitHub token that OTHCloud gives out,
//! without touching the user's global git configuration. The credentials are
//! provided through `GIT_CONFIG_COUNT` / `GIT_CONFIG_KEY_n` / `GIT_CONFIG_VALUE_n`
//! environment entries, which git reads as if they were in a config file, plus
//! a few plain environment variables.
//!
//! Note that `git -c key=value` on the command line takes precedence over
//! `GIT_CONFIG_*` environment entries. Zed passes `-c credential.helper=` for
//! untrusted repositories, which resets the helper list and therefore also
//! drops the helper configured here: tokens are deliberately never handed to
//! untrusted repositories.

use parking_lot::RwLock;
use std::sync::Arc;

/// Something that can provide credentials to git subprocesses.
pub trait GitCredentialOverride: Send + Sync {
    /// Plain environment variables to set on every git subprocess.
    fn env_for_command(&self) -> Vec<(String, String)>;

    /// Git configuration entries (`key`, `value`) to inject via
    /// `GIT_CONFIG_KEY_n` / `GIT_CONFIG_VALUE_n`. Entries are applied in order,
    /// so repeated keys behave like multi-valued config.
    fn config_entries(&self) -> Vec<(String, String)> {
        Vec::new()
    }
}

static OVERRIDE: RwLock<Option<Arc<dyn GitCredentialOverride>>> = RwLock::new(None);
static GITHUB_API_TOKEN: RwLock<Option<String>> = RwLock::new(None);

/// Installs (or, with `None`, removes) the process-wide credential override.
pub fn set_git_credential_override(credential_override: Option<Arc<dyn GitCredentialOverride>>) {
    *OVERRIDE.write() = credential_override;
}

/// Returns whether a credential override is currently installed.
pub fn git_credential_override_active() -> bool {
    OVERRIDE.read().is_some()
}

/// Sets the token used for GitHub REST API calls (avatars, PR lookups, ...).
pub fn set_github_api_token(token: Option<String>) {
    *GITHUB_API_TOKEN.write() = token.filter(|token| !token.is_empty());
}

/// The token used for GitHub REST API calls, if the app installed one.
pub fn github_api_token() -> Option<String> {
    GITHUB_API_TOKEN.read().clone()
}

fn current_override() -> Option<Arc<dyn GitCredentialOverride>> {
    OVERRIDE.read().clone()
}

/// The plain environment variables of the current override (no config entries).
pub fn git_credential_plain_env() -> Vec<(String, String)> {
    current_override()
        .map(|o| o.env_for_command())
        .unwrap_or_default()
}

/// The git config entries of the current override.
pub fn git_credential_config_entries() -> Vec<(String, String)> {
    current_override()
        .map(|o| o.config_entries())
        .unwrap_or_default()
}

/// The full environment for a git subprocess that inherits this process's
/// environment: plain variables plus the config entries encoded as
/// `GIT_CONFIG_*`, numbered after any `GIT_CONFIG_COUNT` already present in
/// this process's environment. Empty when no override is installed.
pub fn git_credential_env() -> Vec<(String, String)> {
    let Some(credential_override) = current_override() else {
        return Vec::new();
    };
    let mut env = Vec::new();
    if let Ok(count) = std::env::var("GIT_CONFIG_COUNT") {
        env.push(("GIT_CONFIG_COUNT".to_string(), count));
    }
    env.extend(credential_override.env_for_command());
    merge_git_config_env(&mut env, &credential_override.config_entries());
    env
}

/// Appends `entries` as `GIT_CONFIG_KEY_n` / `GIT_CONFIG_VALUE_n` pairs to
/// `env`, starting at the `GIT_CONFIG_COUNT` already present in `env`
/// (0 when absent or unparsable), and updates `GIT_CONFIG_COUNT`.
pub fn merge_git_config_env(env: &mut Vec<(String, String)>, entries: &[(String, String)]) {
    if entries.is_empty() {
        return;
    }
    let count_position = env.iter().rposition(|(key, _)| key == "GIT_CONFIG_COUNT");
    let mut count = count_position
        .and_then(|ix| env[ix].1.trim().parse::<usize>().ok())
        .unwrap_or(0);
    for (key, value) in entries {
        env.push((format!("GIT_CONFIG_KEY_{count}"), key.clone()));
        env.push((format!("GIT_CONFIG_VALUE_{count}"), value.clone()));
        count += 1;
    }
    let count = count.to_string();
    match count_position {
        Some(ix) => env[ix].1 = count,
        None => env.push(("GIT_CONFIG_COUNT".to_string(), count)),
    }
}

/// Name of the environment variable the credential helper reads the token from.
pub const GITHUB_TOKEN_ENV_VAR: &str = "OTERMINAL_GH_TOKEN";

/// The shell credential helper used for GitHub hosts. Git runs `!` helpers
/// through `sh` (Git for Windows ships its own), so this also works on Windows.
/// The token is read from the environment so that it never appears in the
/// command line of any process.
pub const GITHUB_TOKEN_HELPER: &str = "!f() { test \"$1\" = get || exit 0; echo username=x-access-token; echo \"password=$OTERMINAL_GH_TOKEN\"; }; f";

/// Credentials for GitHub (or GitHub Enterprise) hosts using a token.
#[derive(Clone)]
pub struct GithubTokenCredentials {
    /// Hosts as URL prefixes, e.g. `https://github.com`.
    pub hosts: Vec<String>,
    pub token: String,
}

impl GithubTokenCredentials {
    pub fn github(token: String) -> Self {
        Self {
            hosts: vec!["https://github.com".to_string()],
            token,
        }
    }
}

impl GitCredentialOverride for GithubTokenCredentials {
    fn env_for_command(&self) -> Vec<(String, String)> {
        vec![
            (GITHUB_TOKEN_ENV_VAR.to_string(), self.token.clone()),
            ("GIT_TERMINAL_PROMPT".to_string(), "0".to_string()),
            ("GCM_INTERACTIVE".to_string(), "never".to_string()),
        ]
    }

    fn config_entries(&self) -> Vec<(String, String)> {
        let mut entries = Vec::with_capacity(self.hosts.len() * 3);
        for host in &self.hosts {
            let host = host.trim_end_matches('/');
            // An empty value resets the helper list for this host only, so
            // Git Credential Manager (or any other helper) is not consulted.
            entries.push((format!("credential.{host}.helper"), String::new()));
            entries.push((
                format!("credential.{host}.helper"),
                GITHUB_TOKEN_HELPER.to_string(),
            ));
            entries.push((
                format!("credential.{host}.username"),
                "x-access-token".to_string(),
            ));
        }
        entries
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pairs(items: &[(&str, &str)]) -> Vec<(String, String)> {
        items
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn merge_appends_after_existing_entries() {
        let mut env = pairs(&[
            ("GIT_CONFIG_COUNT", "1"),
            ("GIT_CONFIG_KEY_0", "gpg.program"),
            ("GIT_CONFIG_VALUE_0", "/tmp/gpg"),
        ]);
        let credentials = GithubTokenCredentials::github("tok".into());
        merge_git_config_env(&mut env, &credentials.config_entries());

        let get = |key: &str| {
            env.iter()
                .rev()
                .find(|(k, _)| k == key)
                .map(|(_, v)| v.clone())
        };
        assert_eq!(get("GIT_CONFIG_COUNT").as_deref(), Some("4"));
        assert_eq!(get("GIT_CONFIG_KEY_0").as_deref(), Some("gpg.program"));
        assert_eq!(
            get("GIT_CONFIG_KEY_1").as_deref(),
            Some("credential.https://github.com.helper")
        );
        assert_eq!(get("GIT_CONFIG_VALUE_1").as_deref(), Some(""));
        assert_eq!(
            get("GIT_CONFIG_VALUE_2").as_deref(),
            Some(GITHUB_TOKEN_HELPER)
        );
        assert_eq!(
            get("GIT_CONFIG_KEY_3").as_deref(),
            Some("credential.https://github.com.username")
        );
        assert_eq!(get("GIT_CONFIG_VALUE_3").as_deref(), Some("x-access-token"));
        assert_eq!(
            env.iter().filter(|(k, _)| k == "GIT_CONFIG_COUNT").count(),
            1
        );
    }

    #[test]
    fn merge_starts_at_zero_and_then_appends_gpg() {
        let mut env = Vec::new();
        merge_git_config_env(&mut env, &pairs(&[("a.b", "1"), ("c.d", "2")]));
        merge_git_config_env(&mut env, &pairs(&[("gpg.program", "x")]));
        assert_eq!(
            env,
            pairs(&[
                ("GIT_CONFIG_KEY_0", "a.b"),
                ("GIT_CONFIG_VALUE_0", "1"),
                ("GIT_CONFIG_KEY_1", "c.d"),
                ("GIT_CONFIG_VALUE_1", "2"),
                ("GIT_CONFIG_COUNT", "3"),
                ("GIT_CONFIG_KEY_2", "gpg.program"),
                ("GIT_CONFIG_VALUE_2", "x"),
            ])
        );
    }

    #[test]
    fn merge_with_no_entries_is_noop() {
        let mut env = Vec::new();
        merge_git_config_env(&mut env, &[]);
        assert!(env.is_empty());
    }

    #[test]
    fn helper_script() {
        assert_eq!(
            GITHUB_TOKEN_HELPER,
            r#"!f() { test "$1" = get || exit 0; echo username=x-access-token; echo "password=$OTERMINAL_GH_TOKEN"; }; f"#
        );
        let env = GithubTokenCredentials::github("secret".into()).env_for_command();
        assert!(env.contains(&("OTERMINAL_GH_TOKEN".to_string(), "secret".to_string())));
        assert!(env.contains(&("GIT_TERMINAL_PROMPT".to_string(), "0".to_string())));
        assert!(env.contains(&("GCM_INTERACTIVE".to_string(), "never".to_string())));
    }
}
