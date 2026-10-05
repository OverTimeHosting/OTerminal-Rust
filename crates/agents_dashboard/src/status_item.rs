//! "⚙ N agents · M background" in the status bar; opens the dashboard.

use std::time::Instant;

use gpui::{Context, Entity, IntoElement, ParentElement, Render, Styled, Subscription, Window};
use ui::{ButtonLike, CommonAnimationExt as _, Icon, IconName, Tooltip, prelude::*};
use workspace::{HideStatusItem, ItemHandle, StatusItemView};

use crate::remote_sessions::{self, SessionStartTimes};
use crate::store::AgentsStore;
use crate::{Open, OpenRemoteControl};

pub struct AgentsStatusItem {
    store: Option<Entity<AgentsStore>>,
    _subscription: Option<Subscription>,
}

impl AgentsStatusItem {
    pub fn new(cx: &mut Context<Self>) -> Self {
        let store = AgentsStore::global(cx);
        let subscription = store
            .as_ref()
            .map(|store| cx.observe(store, |_, _, cx| cx.notify()));
        Self {
            store,
            _subscription: subscription,
        }
    }
}

impl Render for AgentsStatusItem {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let Some(store) = &self.store else {
            return div().into_any_element();
        };
        let (agents, background) = store.read(cx).running_counts(Instant::now());
        if agents == 0 && background == 0 {
            return div().into_any_element();
        }
        let mut text = format!("{agents} agent{}", if agents == 1 { "" } else { "s" });
        if background > 0 {
            text.push_str(&format!(" · {background} background"));
        }
        ButtonLike::new("agents-status-item")
            .child(
                h_flex()
                    .gap_1()
                    .child(if agents > 0 {
                        Icon::new(IconName::Settings)
                            .size(IconSize::Small)
                            .color(Color::Accent)
                            .with_rotate_animation(4)
                            .into_any_element()
                    } else {
                        Icon::new(IconName::Terminal)
                            .size(IconSize::Small)
                            .color(Color::Muted)
                            .into_any_element()
                    })
                    .child(Label::new(text).size(LabelSize::Small)),
            )
            .tooltip(Tooltip::for_action_title("Open Agents Dashboard", &Open))
            .on_click(|_, window, cx| window.dispatch_action(Box::new(Open), cx))
            .into_any_element()
    }
}

impl StatusItemView for AgentsStatusItem {
    fn set_active_pane_item(
        &mut self,
        _: Option<&dyn ItemHandle>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) {
    }

    fn hide_setting(&self, _: &gpui::App) -> Option<HideStatusItem> {
        // Only shown while agents or background commands are running.
        None
    }
}

/// The status bar's entry to the Remote Control tab, with the number of
/// Remote Control sessions open in the app.
pub struct RemoteControlStatusItem {
    _subscription: Subscription,
}

impl RemoteControlStatusItem {
    pub fn new(cx: &mut Context<Self>) -> Self {
        Self {
            _subscription: cx.observe_global::<SessionStartTimes>(|_, cx| cx.notify()),
        }
    }
}

impl Render for RemoteControlStatusItem {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let sessions = remote_sessions::open_session_count(cx);
        let color = if sessions > 0 {
            Color::Accent
        } else {
            Color::Muted
        };
        ButtonLike::new("remote-control-status-item")
            .child(
                h_flex()
                    .gap_1()
                    .child(
                        Icon::new(IconName::SignalHigh)
                            .size(IconSize::Small)
                            .color(color),
                    )
                    .when(sessions > 0, |this| {
                        this.child(Label::new(sessions.to_string()).size(LabelSize::Small))
                    }),
            )
            .tooltip(Tooltip::for_action_title(
                "Remote Control Sessions",
                &OpenRemoteControl,
            ))
            .on_click(|_, window, cx| window.dispatch_action(Box::new(OpenRemoteControl), cx))
    }
}

impl StatusItemView for RemoteControlStatusItem {
    fn set_active_pane_item(
        &mut self,
        _: Option<&dyn ItemHandle>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) {
    }

    fn hide_setting(&self, _: &gpui::App) -> Option<HideStatusItem> {
        // Always shown: it is how the Remote Control tab is found.
        None
    }
}
