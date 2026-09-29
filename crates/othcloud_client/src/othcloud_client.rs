//! OTHCloud client for OTerminal: the desktop API, the signed-in account, secure
//! token storage, and the `othcloud-terminal://` pairing deep link.

mod account;
mod api;
mod base_url;
mod deep_link;
mod storage;

use gpui::{App, AppContext as _};

pub use account::{LoadState, OthcloudAccount, OthcloudAccountEvent};
pub use api::{
    ApiError, DevEnvState, DevEnvStatus, GithubAccount, GithubAccountsResponse, GithubTokenKind,
    GithubTokenResponse, NewTerminalProfile, OthcloudApi, PairResponse, Row, ServerTerminalProfile,
    ServicesResponse, User, current_platform,
};
pub use base_url::{absolute_url, base_url, host_of, pages};
pub use deep_link::{URL_SCHEME, parse_pairing_url, register_url_scheme};
pub use storage::{kv_delete, kv_get, kv_set, secret_delete, secret_get, secret_set};

/// Creates the global [`OthcloudAccount`], restores a stored session, and makes
/// sure this executable handles `othcloud-terminal://` links.
pub fn init(cx: &mut App) {
    OthcloudAccount::init_global(cx);

    cx.background_spawn(async move {
        if let Err(error) = register_url_scheme() {
            log::error!("failed to register the {URL_SCHEME}:// URL scheme: {error:#}");
        }
    })
    .detach();
}
