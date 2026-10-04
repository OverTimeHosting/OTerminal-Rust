use gpui::{Action as _, Anchor, Subscription};
use othcloud_client::{OthcloudAccount, OthcloudAccountEvent};
use othcloud_github::{
    AddGithubToken, ConnectGithub, GithubAccountStore, OthcloudGithubSettings, ResolvedAccount,
    SignInToGithub, SwitchGithubAccount,
};
use settings::Settings as _;
use ui::{ButtonLike, ContextMenu, PopoverMenu, Tooltip, prelude::*};
use workspace::{HideStatusItem, ItemHandle, StatusItemView};

use crate::{OpenConsole, PastePairingCode, SignIn, SignOut, ToggleFocus};

/// Status bar entry for the OTHCloud and GitHub accounts: "Sign in" while
/// signed out of OTHCloud, the user's name while signed in. Clicking it opens
/// a menu to sign in to, sign out of and switch either account.
pub struct OthcloudStatusItem {
    _subscriptions: Vec<Subscription>,
}

impl OthcloudStatusItem {
    pub fn new(cx: &mut Context<Self>) -> Self {
        let mut subscriptions = Vec::new();
        if let Some(account) = OthcloudAccount::global(cx) {
            subscriptions.push(cx.observe(&account, |_, _, cx| cx.notify()));
            subscriptions
                .push(cx.subscribe(&account, |_, _, _: &OthcloudAccountEvent, cx| cx.notify()));
        }
        if let Some(store) = GithubAccountStore::global(cx) {
            subscriptions.push(cx.observe(&store, |_, _, cx| cx.notify()));
        }
        Self {
            _subscriptions: subscriptions,
        }
    }
}

fn othcloud_user_name(cx: &App) -> Option<SharedString> {
    let account = OthcloudAccount::global(cx)?;
    let account = account.read(cx);
    account.is_signed_in().then(|| {
        account
            .user()
            .map(crate::panel::user_display_name)
            .unwrap_or_else(|| "OTHCloud".into())
    })
}

#[derive(Default)]
struct GithubMenuState {
    login: Option<String>,
    source: Option<&'static str>,
    problem: Option<SharedString>,
    /// The account stored on this PC that git uses (or would use, when its
    /// token is missing), which is the one "Sign Out of GitHub" removes.
    local_account_id: Option<u64>,
    connect_available: bool,
}

impl GithubMenuState {
    fn read(cx: &App) -> Self {
        let Some(store) = GithubAccountStore::global(cx) else {
            return Self::default();
        };
        let store = store.read(cx);
        Self {
            login: store.active_label(cx),
            source: store.active_account().map(|active| {
                if active.is_local() {
                    "stored on this PC"
                } else {
                    "via OTHCloud"
                }
            }),
            problem: store.problem().cloned(),
            local_account_id: match store.resolved(cx) {
                ResolvedAccount::Local(github_id) => Some(github_id),
                ResolvedAccount::None | ResolvedAccount::Othcloud => None,
            },
            connect_available: store.othcloud_connect_available(),
        }
    }
}

fn github_account_line(login: Option<&str>, source: Option<&str>, has_problem: bool) -> String {
    match (login, source) {
        (Some(login), Some(source)) => format!("Signed in as {login} ({source})"),
        (Some(login), None) if has_problem => login.to_string(),
        (Some(login), None) => format!("Connecting as {login}…"),
        (None, _) => "No account: git uses your credential manager".to_string(),
    }
}

fn build_accounts_menu(menu: ContextMenu, cx: &App) -> ContextMenu {
    let signed_in_to_othcloud = othcloud_user_name(cx).is_some();
    let github = GithubMenuState::read(cx);
    let browser_sign_in_configured = OthcloudGithubSettings::get_global(cx)
        .github_oauth_client_id
        .is_some();

    let menu = menu.header("OTHCloud");
    let menu = if signed_in_to_othcloud {
        menu.action("Open OTHCloud Panel", ToggleFocus.boxed_clone())
            .action("Open Console", OpenConsole.boxed_clone())
            .action("Sign Out", SignOut.boxed_clone())
    } else {
        menu.action("Sign In", SignIn.boxed_clone())
            .action("Paste Pairing Code", PastePairingCode.boxed_clone())
    };

    menu.separator()
        .header("GitHub")
        .label(github_account_line(
            github.login.as_deref(),
            github.source,
            github.problem.is_some(),
        ))
        .when_some(github.problem, |menu, problem| {
            menu.custom_row(move |_, _| {
                div()
                    .max_w(rems(20.))
                    .child(
                        Label::new(problem.clone())
                            .size(LabelSize::Small)
                            .color(Color::Warning),
                    )
                    .into_any_element()
            })
        })
        .action("Switch Account…", SwitchGithubAccount.boxed_clone())
        .action("Add Account…", SignInToGithub.boxed_clone())
        // Without an OAuth app, "Add Account…" already opens the token prompt.
        .when(browser_sign_in_configured, |menu| {
            menu.action("Add Token…", AddGithubToken.boxed_clone())
        })
        .when(signed_in_to_othcloud && github.connect_available, |menu| {
            menu.action("Connect via OTHCloud", ConnectGithub.boxed_clone())
        })
        .when_some(github.local_account_id, |menu, github_id| {
            let label = match &github.login {
                Some(login) => format!("Sign Out of GitHub ({login})"),
                None => "Sign Out of GitHub".to_string(),
            };
            menu.entry(label, None, move |_, cx| {
                let Some(store) = GithubAccountStore::global(cx) else {
                    return;
                };
                // The account git uses can change while the menu is open.
                let still_in_use = matches!(
                    store.read(cx).resolved(cx),
                    ResolvedAccount::Local(resolved_id) if resolved_id == github_id
                );
                if still_in_use {
                    store
                        .update(cx, |store, cx| store.remove_local_account(github_id, cx))
                        .detach();
                }
            })
        })
}

impl Render for OthcloudStatusItem {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let label = othcloud_user_name(cx).unwrap_or_else(|| "Sign in".into());
        let github_problem =
            GithubAccountStore::global(cx).and_then(|store| store.read(cx).problem().cloned());
        let github_login =
            GithubAccountStore::global(cx).and_then(|store| store.read(cx).active_label(cx));
        let tooltip: SharedString = match (&github_problem, github_login) {
            (Some(problem), _) => problem.clone(),
            (None, Some(login)) => {
                format!("OTHCloud and GitHub accounts. Git uses GitHub as {login}.").into()
            }
            (None, None) => "OTHCloud and GitHub accounts".into(),
        };

        PopoverMenu::new("othcloud-status-menu")
            .menu(|window, cx| {
                Some(ContextMenu::build(window, cx, |menu, _, cx| {
                    build_accounts_menu(menu, cx)
                }))
            })
            .anchor(Anchor::BottomRight)
            .trigger_with_tooltip(
                ButtonLike::new("othcloud-status-item").child(
                    h_flex()
                        .gap_1()
                        .child(
                            gpui::img("images/oth_logo_16.png")
                                .flex_none()
                                .size(px(16.)),
                        )
                        .child(Label::new(label).size(LabelSize::Small).color(Color::Muted))
                        .when(github_problem.is_some(), |this| {
                            this.child(
                                Icon::new(IconName::Warning)
                                    .size(IconSize::Small)
                                    .color(Color::Warning),
                            )
                        }),
                ),
                Tooltip::text(tooltip),
            )
    }
}

impl StatusItemView for OthcloudStatusItem {
    fn set_active_pane_item(
        &mut self,
        _: Option<&dyn ItemHandle>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) {
    }

    fn hide_setting(&self, _: &App) -> Option<HideStatusItem> {
        // Always shown: it is the entry point to OTHCloud sign-in.
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn github_account_line_describes_each_state() {
        assert_eq!(
            github_account_line(Some("octocat"), Some("stored on this PC"), false),
            "Signed in as octocat (stored on this PC)"
        );
        assert_eq!(
            github_account_line(Some("octocat"), None, false),
            "Connecting as octocat…"
        );
        assert_eq!(github_account_line(Some("octocat"), None, true), "octocat");
        assert_eq!(
            github_account_line(None, None, false),
            "No account: git uses your credential manager"
        );
    }
}
