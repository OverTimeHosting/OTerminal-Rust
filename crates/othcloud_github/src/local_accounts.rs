//! GitHub accounts stored on this PC, usable without OTHCloud.
//!
//! The token of each account lives in the operating system's credential
//! store (Windows Credential Manager) under [`token_key_url`]. Only a
//! non-secret index — login, id, avatar — is kept in the key-value store.

use anyhow::Result;
use gpui::AsyncApp;
use serde::{Deserialize, Serialize};

use crate::github_api::{GithubUser, ValidatedToken};

/// Key-value store key of the JSON index of local accounts.
pub(crate) const LOCAL_ACCOUNTS_KEY: &str = "othcloud.github.localAccounts";
/// Key-value store key of the active account choice (see [`ActiveChoice`]).
pub(crate) const ACTIVE_CHOICE_KEY: &str = "othcloud.github.activeAccount";

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LocalGithubAccount {
    /// GitHub's numeric user id.
    pub id: u64,
    pub login: String,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub avatar_url: Option<String>,
    /// OAuth scopes of the token, when GitHub reported them.
    #[serde(default)]
    pub scopes: Option<Vec<String>>,
    /// Whether the token was also saved on OTHCloud when it was added.
    #[serde(default)]
    pub saved_to_othcloud: bool,
    /// How the token was obtained: `"device"` (browser sign-in) or `"token"`.
    #[serde(default)]
    pub source: Option<String>,
}

impl LocalGithubAccount {
    pub fn from_validated(validated: &ValidatedToken, source: &str) -> Self {
        let GithubUser {
            login,
            id,
            name,
            avatar_url,
        } = validated.user.clone();
        Self {
            id,
            login,
            name: name.filter(|name| !name.trim().is_empty()),
            avatar_url,
            scopes: validated.scopes.clone(),
            saved_to_othcloud: false,
            source: Some(source.to_string()),
        }
    }
}

/// The credential store entry of a local account's token. Independent of the
/// OTHCloud server, so the same accounts work in every build.
pub fn token_key_url(github_id: u64) -> String {
    format!("https://github.com/oterminal/local-account/{github_id}")
}

/// Parses the stored index; anything unreadable counts as "no accounts".
pub fn parse_index(json: &str) -> Vec<LocalGithubAccount> {
    match serde_json::from_str::<Vec<LocalGithubAccount>>(json) {
        Ok(mut accounts) => {
            let mut seen = std::collections::HashSet::new();
            accounts.retain(|account| account.id != 0 && seen.insert(account.id));
            accounts
        }
        Err(error) => {
            log::error!("ignoring unreadable local GitHub account index: {error}");
            Vec::new()
        }
    }
}

pub fn serialize_index(accounts: &[LocalGithubAccount]) -> String {
    serde_json::to_string(accounts).unwrap_or_else(|_| "[]".to_string())
}

/// Adds `account`, or replaces the entry with the same GitHub id in place.
pub fn upsert(accounts: &mut Vec<LocalGithubAccount>, account: LocalGithubAccount) {
    match accounts
        .iter_mut()
        .find(|existing| existing.id == account.id)
    {
        Some(existing) => *existing = account,
        None => accounts.push(account),
    }
}

/// Removes the account with `github_id`; returns whether it was there.
pub fn remove(accounts: &mut Vec<LocalGithubAccount>, github_id: u64) -> bool {
    let before = accounts.len();
    accounts.retain(|account| account.id != github_id);
    accounts.len() != before
}

/// Which GitHub identity the user chose for git, persisted across restarts.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ActiveChoice {
    /// Nothing chosen yet: OTHCloud's account when signed in, else the first
    /// local account, else none.
    #[default]
    Auto,
    /// No account: git uses the user's own credential manager.
    None,
    /// A GitHub account stored on this PC.
    Local(u64),
    /// The OTHCloud account (which one is remembered per OTHCloud user).
    Othcloud,
}

impl ActiveChoice {
    pub fn parse(value: Option<&str>) -> Self {
        match value.map(str::trim) {
            None | Some("") | Some("auto") => ActiveChoice::Auto,
            Some("none") => ActiveChoice::None,
            Some("othcloud") => ActiveChoice::Othcloud,
            Some(value) => value
                .strip_prefix("local:")
                .and_then(|id| id.parse::<u64>().ok())
                .map(ActiveChoice::Local)
                .unwrap_or(ActiveChoice::Auto),
        }
    }

    pub fn serialize(self) -> String {
        match self {
            ActiveChoice::Auto => "auto".to_string(),
            ActiveChoice::None => "none".to_string(),
            ActiveChoice::Othcloud => "othcloud".to_string(),
            ActiveChoice::Local(id) => format!("local:{id}"),
        }
    }
}

/// What actually provides git's GitHub credentials right now.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ResolvedAccount {
    None,
    Local(u64),
    Othcloud,
}

/// Resolves the user's choice against what is available.
pub fn resolve(
    choice: ActiveChoice,
    local_accounts: &[LocalGithubAccount],
    othcloud_signed_in: bool,
) -> ResolvedAccount {
    let fallback = || {
        if othcloud_signed_in {
            ResolvedAccount::Othcloud
        } else {
            local_accounts
                .first()
                .map_or(ResolvedAccount::None, |account| {
                    ResolvedAccount::Local(account.id)
                })
        }
    };
    match choice {
        ActiveChoice::None => ResolvedAccount::None,
        ActiveChoice::Local(id) if local_accounts.iter().any(|account| account.id == id) => {
            ResolvedAccount::Local(id)
        }
        ActiveChoice::Local(_) | ActiveChoice::Auto | ActiveChoice::Othcloud => fallback(),
    }
}

pub async fn read_token(github_id: u64, cx: &AsyncApp) -> Option<String> {
    let (_, bytes) = othcloud_client::os_secret_get(&token_key_url(github_id), cx).await?;
    String::from_utf8(bytes)
        .ok()
        .map(|token| token.trim().to_string())
        .filter(|token| !token.is_empty())
}

pub async fn write_token(account: &LocalGithubAccount, token: &str, cx: &AsyncApp) -> Result<()> {
    othcloud_client::os_secret_set(
        &token_key_url(account.id),
        &account.login,
        token.as_bytes(),
        cx,
    )
    .await
}

pub async fn delete_token(github_id: u64, cx: &AsyncApp) -> Result<()> {
    othcloud_client::os_secret_delete(&token_key_url(github_id), cx).await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn account(id: u64, login: &str) -> LocalGithubAccount {
        LocalGithubAccount {
            id,
            login: login.to_string(),
            name: None,
            avatar_url: None,
            scopes: Some(vec!["repo".to_string()]),
            saved_to_othcloud: false,
            source: Some("token".to_string()),
        }
    }

    #[test]
    fn index_round_trips() {
        let accounts = vec![account(1, "octocat"), account(2, "hubot")];
        let json = serialize_index(&accounts);
        // The index is non-secret: it never contains a token field.
        assert!(!json.to_lowercase().contains("token\":"));
        assert_eq!(parse_index(&json), accounts);
    }

    #[test]
    fn index_parsing_is_lenient() {
        assert_eq!(parse_index("not json"), Vec::new());
        assert_eq!(parse_index("[]"), Vec::new());
        // Old/partial entries still load; duplicates and id 0 are dropped.
        let parsed = parse_index(
            r#"[{"id":1,"login":"a"},{"id":1,"login":"dup"},{"id":0,"login":"zero"},{"id":2,"login":"b","extra":true}]"#,
        );
        assert_eq!(
            parsed.iter().map(|a| a.login.as_str()).collect::<Vec<_>>(),
            vec!["a", "b"]
        );
        assert_eq!(parsed[0].scopes, None);
        assert!(!parsed[0].saved_to_othcloud);
    }

    #[test]
    fn upsert_and_remove() {
        let mut accounts = vec![account(1, "a"), account(2, "b")];
        upsert(&mut accounts, account(1, "renamed"));
        assert_eq!(accounts.len(), 2);
        assert_eq!(accounts[0].login, "renamed");
        upsert(&mut accounts, account(3, "c"));
        assert_eq!(accounts.len(), 3);
        assert!(remove(&mut accounts, 2));
        assert!(!remove(&mut accounts, 2));
        assert_eq!(
            accounts.iter().map(|a| a.id).collect::<Vec<_>>(),
            vec![1, 3]
        );
    }

    #[test]
    fn active_choice_round_trips() {
        for choice in [
            ActiveChoice::Auto,
            ActiveChoice::None,
            ActiveChoice::Othcloud,
            ActiveChoice::Local(583231),
        ] {
            assert_eq!(ActiveChoice::parse(Some(&choice.serialize())), choice);
        }
        assert_eq!(ActiveChoice::parse(None), ActiveChoice::Auto);
        assert_eq!(ActiveChoice::parse(Some("local:abc")), ActiveChoice::Auto);
        assert_eq!(ActiveChoice::parse(Some("weird")), ActiveChoice::Auto);
    }

    #[test]
    fn resolution() {
        let locals = vec![account(7, "a"), account(8, "b")];
        use ResolvedAccount as R;
        // Explicit local wins, even when signed in to OTHCloud.
        assert_eq!(resolve(ActiveChoice::Local(8), &locals, true), R::Local(8));
        // A removed local account falls back.
        assert_eq!(resolve(ActiveChoice::Local(9), &locals, false), R::Local(7));
        assert_eq!(resolve(ActiveChoice::Local(9), &locals, true), R::Othcloud);
        // Explicit "none" means the user's own credential manager.
        assert_eq!(resolve(ActiveChoice::None, &locals, true), R::None);
        // Auto / OTHCloud prefer OTHCloud when signed in.
        assert_eq!(resolve(ActiveChoice::Auto, &locals, true), R::Othcloud);
        assert_eq!(resolve(ActiveChoice::Othcloud, &locals, false), R::Local(7));
        assert_eq!(resolve(ActiveChoice::Auto, &[], false), R::None);
        assert_eq!(resolve(ActiveChoice::Othcloud, &[], false), R::None);
    }

    #[test]
    fn token_keys_are_per_account() {
        assert_eq!(
            token_key_url(42),
            "https://github.com/oterminal/local-account/42"
        );
        assert_ne!(token_key_url(1), token_key_url(2));
    }
}
