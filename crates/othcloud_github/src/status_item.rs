use gpui::Subscription;
use ui::{ButtonLike, Tooltip, prelude::*};
use workspace::{HideStatusItem, ItemHandle, StatusItemView};

use crate::{GithubAccountStore, SwitchGithubAccount};

/// Status bar entry showing the GitHub account git uses on github.com.
/// Clicking it opens the account switcher.
pub struct GithubStatusItem {
    _subscriptions: Vec<Subscription>,
}

impl GithubStatusItem {
    pub fn new(cx: &mut Context<Self>) -> Self {
        let mut subscriptions = Vec::new();
        if let Some(store) = GithubAccountStore::global(cx) {
            subscriptions.push(cx.observe(&store, |_, _, cx| cx.notify()));
        }
        Self {
            _subscriptions: subscriptions,
        }
    }
}

impl Render for GithubStatusItem {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let (label, problem, source) = GithubAccountStore::global(cx)
            .map(|store| {
                let store = store.read(cx);
                let source = store.active_account().map(|active| {
                    if active.is_local() {
                        "stored on this PC"
                    } else {
                        "via OTHCloud"
                    }
                });
                (store.active_label(cx), store.problem().cloned(), source)
            })
            .unwrap_or_default();

        let tooltip: SharedString = match (&label, &problem, source) {
            (_, Some(problem), _) => problem.clone(),
            (Some(login), None, Some(source)) => {
                format!("Git uses GitHub as {login} ({source}). Click to switch accounts.").into()
            }
            (Some(login), None, None) => {
                format!("Connecting to GitHub as {login}… Click to switch accounts.").into()
            }
            (None, None, _) => "No GitHub account: git uses your own credential manager. \
                                Click to add or switch accounts."
                .into(),
        };
        let text: SharedString = label.unwrap_or_else(|| "GitHub".to_string()).into();
        let color = if problem.is_some() {
            Color::Warning
        } else {
            Color::Muted
        };

        ButtonLike::new("github-account-status-item")
            .child(
                h_flex()
                    .gap_1()
                    .child(
                        Icon::new(IconName::Github)
                            .size(IconSize::Small)
                            .color(color),
                    )
                    .child(Label::new(text).size(LabelSize::Small).color(color)),
            )
            .tooltip(Tooltip::text(tooltip))
            .on_click(|_, window, cx| {
                window.dispatch_action(Box::new(SwitchGithubAccount), cx);
            })
    }
}

impl StatusItemView for GithubStatusItem {
    fn set_active_pane_item(
        &mut self,
        _: Option<&dyn ItemHandle>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) {
    }

    fn hide_setting(&self, _: &App) -> Option<HideStatusItem> {
        None
    }
}
