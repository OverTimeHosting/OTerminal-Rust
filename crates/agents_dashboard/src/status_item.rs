//! "⚙ N agents · M background" in the status bar; opens the dashboard.

use std::time::Instant;

use gpui::{Context, Entity, IntoElement, ParentElement, Render, Styled, Subscription, Window};
use ui::{ButtonLike, CommonAnimationExt as _, Icon, IconName, Tooltip, prelude::*};
use workspace::{HideStatusItem, ItemHandle, StatusItemView};

use crate::Open;
use crate::store::AgentsStore;

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
