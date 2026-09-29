//! The OTHCloud dock panel, status bar item and `othcloud:` commands.
//!
//! Everything in here is built on top of the `othcloud_client` crate: the
//! panel only renders what [`OthcloudAccount`] knows and forwards user intent
//! (sign in, start a dev environment, ...) to it or to its [`OthcloudApi`].

mod pairing_modal;
mod panel;
mod status_item;

use gpui::{App, Entity, WeakEntity, actions};
use othcloud_client::{ApiError, OthcloudAccount};
use workspace::{Toast, Workspace, notifications::NotificationId};

pub use pairing_modal::PairingCodeModal;
pub use panel::OthcloudPanel;
pub use status_item::OthcloudStatusItem;

actions!(
    othcloud_panel,
    [
        /// Toggles focus on the OTHCloud panel.
        ToggleFocus,
    ]
);

actions!(
    othcloud,
    [
        /// Signs in to OTHCloud by opening the pairing page at othcloud.xyz.
        SignIn,
        /// Signs out of OTHCloud on this device.
        SignOut,
        /// Completes sign-in with a pasted pairing link or code.
        PastePairingCode,
        /// Opens the OTHCloud console in the browser.
        OpenConsole,
        /// Opens the OTHCloud dashboard in the browser.
        OpenDashboard,
        /// Opens the OTHCloud game servers page in the browser.
        OpenGameServers,
        /// Reloads projects, dev environments and game servers from OTHCloud.
        RefreshServices,
    ]
);

pub fn init(cx: &mut App) {
    cx.observe_new(|workspace: &mut Workspace, _, _| {
        workspace.register_action(|workspace, _: &ToggleFocus, window, cx| {
            workspace.toggle_panel_focus::<OthcloudPanel>(window, cx);
        });
        workspace.register_action(|_, _: &SignIn, _, cx| {
            sign_in(cx);
        });
        workspace.register_action(|_, _: &SignOut, _, cx| {
            sign_out(cx);
        });
        workspace.register_action(|workspace, _: &PastePairingCode, window, cx| {
            workspace.toggle_modal(window, cx, PairingCodeModal::new);
        });
        workspace.register_action(|_, _: &OpenConsole, _, cx| {
            cx.open_url(&othcloud_client::absolute_url("/dashboard"));
        });
        workspace.register_action(|_, _: &OpenDashboard, _, cx| {
            cx.open_url(&othcloud_client::absolute_url("/dashboard"));
        });
        workspace.register_action(|_, _: &OpenGameServers, _, cx| {
            cx.open_url(&othcloud_client::absolute_url("/dashboard/games"));
        });
        workspace.register_action(|_, _: &RefreshServices, _, cx| {
            refresh_services(cx);
        });
    })
    .detach();
}

/// Starts the browser-based sign-in flow.
pub(crate) fn sign_in(cx: &mut App) {
    if let Some(account) = OthcloudAccount::global(cx) {
        account.update(cx, |account, cx| account.begin_sign_in(cx));
    }
}

fn sign_out(cx: &mut App) {
    let Some(account) = OthcloudAccount::global(cx) else {
        return;
    };
    let task = account.update(cx, |account, cx| account.sign_out(cx));
    cx.spawn(async move |cx| {
        if let Err(error) = task.await {
            cx.update(|cx| handle_account_error(&error, cx));
        }
    })
    .detach();
}

/// Reloads the account and its services. Failures are surfaced by the panel
/// (the stale-data banner), so they are only logged here.
pub(crate) fn refresh_services(cx: &mut App) {
    let Some(account) = OthcloudAccount::global(cx) else {
        return;
    };
    let task = account.update(cx, |account, cx| account.refresh(cx));
    cx.spawn(async move |cx| {
        if let Err(error) = task.await {
            cx.update(|cx| handle_account_error(&error, cx));
        }
    })
    .detach();
}

/// For errors from [`OthcloudAccount`] tasks, which already show their own
/// notifications / load state: drop the session on 401, log everything else.
pub(crate) fn handle_account_error(error: &anyhow::Error, cx: &mut App) {
    if error
        .downcast_ref::<ApiError>()
        .is_some_and(|error| error.is_unauthorized())
    {
        handle_unauthorized(cx);
    } else {
        log::warn!("othcloud: {error:#}");
    }
}

fn handle_unauthorized(cx: &mut App) {
    if let Some(account) = OthcloudAccount::global(cx) {
        account.update(cx, |account, cx| account.handle_unauthorized(cx));
    }
}

struct OthcloudErrorToast;

/// Reports an error from an API call made by the panel: a 401 signs the user
/// out (the token is no longer valid), anything else is shown as a toast.
pub(crate) fn report_api_error(workspace: &WeakEntity<Workspace>, error: ApiError, cx: &mut App) {
    if error.is_unauthorized() {
        handle_unauthorized(cx);
        return;
    }
    let message = error.friendly_message(&othcloud_host());
    log::warn!("othcloud: {message} ({error})");
    workspace
        .update(cx, |workspace, cx| {
            workspace.show_toast(
                Toast::new(NotificationId::unique::<OthcloudErrorToast>(), message).autohide(),
                cx,
            );
        })
        .ok();
}

/// The host name of the OTHCloud instance, e.g. `othcloud.xyz`.
pub(crate) fn othcloud_host() -> String {
    othcloud_client::host_of(&othcloud_client::base_url()).to_string()
}

pub(crate) fn account(cx: &App) -> Option<Entity<OthcloudAccount>> {
    OthcloudAccount::global(cx)
}
