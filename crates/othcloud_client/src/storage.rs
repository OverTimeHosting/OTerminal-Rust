//! Persistence for the OTHCloud session.
//!
//! The desktop token lives in the OS credential store (Windows Credential Manager,
//! macOS Keychain, Secret Service on Linux) through Zed's credentials provider.
//! Note that in the Dev release channel Zed uses a plaintext
//! `development_credentials` file instead, to avoid keychain prompts; set
//! `ZED_DEVELOPMENT_USE_KEYCHAIN=1` to exercise the real keychain in dev builds.
//!
//! The last known user is cached (non-secret) in the key-value store so the UI
//! can show who is signed in before the first `/api/desktop/me` round trip.

use anyhow::Result;
use db::kvp::KeyValueStore;
use gpui::AsyncApp;
use util::ResultExt as _;

use crate::{api::User, base_url::base_url};

const TOKEN_USERNAME: &str = "othcloud";
const CACHED_USER_KEY: &str = "othcloud.account.user";

/// Keyed by base URL so a development OTHCloud never clobbers the production token.
pub fn token_key_url() -> String {
    format!("{}/oterminal/desktop-token", base_url())
}

pub async fn read_token(cx: &AsyncApp) -> Option<String> {
    let (_, bytes) = secret_get(&token_key_url(), cx).await?;
    String::from_utf8(bytes)
        .log_err()
        .map(|token| token.trim().to_string())
        .filter(|token| !token.is_empty())
}

pub async fn write_token(token: &str, cx: &AsyncApp) -> Result<()> {
    secret_set(&token_key_url(), TOKEN_USERNAME, token.as_bytes(), cx).await
}

pub async fn delete_token(cx: &AsyncApp) -> Result<()> {
    secret_delete(&token_key_url(), cx).await
}

pub async fn read_cached_user(cx: &AsyncApp) -> Option<User> {
    let json = kv_get(CACHED_USER_KEY, cx).await?;
    parse_cached_user(&json)
}

pub async fn write_cached_user(user: &User, cx: &AsyncApp) -> Result<()> {
    kv_set(CACHED_USER_KEY, &serde_json::to_string(user)?, cx).await
}

pub async fn delete_cached_user(cx: &AsyncApp) -> Result<()> {
    kv_delete(CACHED_USER_KEY, cx).await
}

fn parse_cached_user(json: &str) -> Option<User> {
    let value: serde_json::Value = serde_json::from_str(json).log_err()?;
    let has_identity = value.get("id").is_some_and(serde_json::Value::is_string)
        && value.get("email").is_some_and(serde_json::Value::is_string);
    if !has_identity {
        return None;
    }
    serde_json::from_value(value).log_err()
}

/// Reads a secret stored under `key_url`, returning `(username, bytes)`.
pub async fn secret_get(key_url: &str, cx: &AsyncApp) -> Option<(String, Vec<u8>)> {
    let provider = cx.update(|cx| zed_credentials_provider::global(cx));
    provider
        .read_credentials(key_url, cx)
        .await
        .log_err()
        .flatten()
}

pub async fn secret_set(
    key_url: &str,
    username: &str,
    password: &[u8],
    cx: &AsyncApp,
) -> Result<()> {
    let provider = cx.update(|cx| zed_credentials_provider::global(cx));
    provider
        .write_credentials(key_url, username, password, cx)
        .await
}

pub async fn secret_delete(key_url: &str, cx: &AsyncApp) -> Result<()> {
    let provider = cx.update(|cx| zed_credentials_provider::global(cx));
    provider.delete_credentials(key_url, cx).await
}

/// Reads a secret from the operating system's credential store (Windows
/// Credential Manager, macOS Keychain, Secret Service), returning
/// `(username, bytes)`.
///
/// Unlike [`secret_get`], this never falls back to Zed's plaintext
/// `development_credentials` file in Dev builds: it is meant for long-lived
/// third-party secrets such as locally stored GitHub tokens.
pub async fn os_secret_get(key_url: &str, cx: &AsyncApp) -> Option<(String, Vec<u8>)> {
    cx.update(|cx| cx.read_credentials(key_url))
        .await
        .log_err()
        .flatten()
}

/// Writes a secret to the operating system's credential store. See [`os_secret_get`].
pub async fn os_secret_set(
    key_url: &str,
    username: &str,
    password: &[u8],
    cx: &AsyncApp,
) -> Result<()> {
    cx.update(|cx| cx.write_credentials(key_url, username, password))
        .await
}

/// Deletes a secret from the operating system's credential store. See [`os_secret_get`].
pub async fn os_secret_delete(key_url: &str, cx: &AsyncApp) -> Result<()> {
    cx.update(|cx| cx.delete_credentials(key_url)).await
}

pub async fn kv_get(key: &str, cx: &AsyncApp) -> Option<String> {
    let store = cx.update(|cx| KeyValueStore::global(cx));
    store.read_kvp(key).log_err().flatten()
}

pub async fn kv_set(key: &str, value: &str, cx: &AsyncApp) -> Result<()> {
    let store = cx.update(|cx| KeyValueStore::global(cx));
    store.write_kvp(key.to_string(), value.to_string()).await
}

pub async fn kv_delete(key: &str, cx: &AsyncApp) -> Result<()> {
    let store = cx.update(|cx| KeyValueStore::global(cx));
    store.delete_kvp(key.to_string()).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_cached_user() {
        let user = parse_cached_user(r#"{ "id": "u1", "email": "a@b.c", "name": "Ann" }"#)
            .expect("valid cached user");
        assert_eq!(user.display_name(), "Ann");
        assert!(parse_cached_user(r#"{ "id": "u1" }"#).is_none());
        assert!(parse_cached_user(r#"{ "id": 1, "email": "a@b.c" }"#).is_none());
        assert!(parse_cached_user("not json").is_none());
    }
}
