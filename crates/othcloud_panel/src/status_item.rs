use gpui::Subscription;
use othcloud_client::{OthcloudAccount, OthcloudAccountEvent};
use ui::{ButtonLike, Tooltip, prelude::*};
use workspace::{HideStatusItem, ItemHandle, StatusItemView};

use crate::{SignIn, ToggleFocus};

/// Status bar entry: "Sign in" while signed out, the user's name while signed
/// in. Clicking it signs in or toggles the OTHCloud panel.
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
        Self {
            _subscriptions: subscriptions,
        }
    }
}

impl Render for OthcloudStatusItem {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let user_name = OthcloudAccount::global(cx).and_then(|account| {
            let account = account.read(cx);
            if account.is_signed_in() {
                Some(
                    account
                        .user()
                        .map(crate::panel::user_display_name)
                        .unwrap_or_else(|| "OTHCloud".into()),
                )
            } else {
                None
            }
        });

        let signed_in = user_name.is_some();
        let label: SharedString = user_name.unwrap_or_else(|| "Sign in".into());

        ButtonLike::new("othcloud-status-item")
            .child(
                h_flex()
                    .gap_1()
                    .child(
                        Icon::new(IconName::Othcloud)
                            .size(IconSize::Small)
                            .color(Color::Muted),
                    )
                    .child(Label::new(label).size(LabelSize::Small).color(Color::Muted)),
            )
            .tooltip(Tooltip::text(if signed_in {
                "Toggle OTHCloud Panel"
            } else {
                "Sign in to OTHCloud"
            }))
            .on_click(move |_, window, cx| {
                if signed_in {
                    window.dispatch_action(Box::new(ToggleFocus), cx);
                } else {
                    window.dispatch_action(Box::new(SignIn), cx);
                }
            })
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
