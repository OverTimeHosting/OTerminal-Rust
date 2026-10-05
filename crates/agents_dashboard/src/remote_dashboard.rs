//! The "Remote Control" workspace item: every Claude Code Remote Control
//! session of the app, with its terminal's output and a way to drive it
//! without switching to it.

use std::time::Instant;

use collections::HashMap;
use editor::Editor;
use gpui::{
    AnyElement, AnyWindowHandle, App, ClipboardItem, Context, Entity, EntityId, EventEmitter,
    FocusHandle, Focusable, IntoElement, ParentElement, Render, ScrollHandle, SharedString, Styled,
    Subscription, Task, Window, div, px,
};
use terminal::Terminal;
use ui::{CommonAnimationExt as _, Disclosure, Icon, IconButton, IconName, Tooltip, prelude::*};
use workspace::{
    Item, Workspace,
    item::{ItemEvent, TabContentParams},
};

use crate::dashboard::{LogPreview, Palette, TICK};
use crate::model::{self, format_duration};
use crate::remote_sessions::{self, RemoteSession};
use crate::store::REFRESH_DEBOUNCE;

/// What the dashboard keeps for a session next to its [`RemoteSession`].
struct SessionView {
    /// Created on first render, as editors need a window.
    input: Option<Entity<Editor>>,
    preview: Option<LogPreview>,
    /// The user's choice; without one the preview is open while the session
    /// runs.
    preview_expanded: Option<bool>,
    output: Option<SharedString>,
    /// Kept once seen, as the link scrolls out of the previewed lines.
    url: Option<SharedString>,
    _subscription: Subscription,
}

pub struct RemoteControlDashboard {
    focus_handle: FocusHandle,
    sessions: Vec<RemoteSession>,
    session_views: HashMap<EntityId, SessionView>,
    scroll_handle: ScrollHandle,
    refresh_task: Option<Task<()>>,
    _ticker: Task<()>,
}

impl RemoteControlDashboard {
    /// Activates the Remote Control tab of this workspace, opening it if
    /// needed.
    pub fn deploy(workspace: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
        if let Some(existing) = workspace.item_of_type::<RemoteControlDashboard>(cx) {
            workspace.activate_item(&existing, true, true, window, cx);
            return;
        }
        let dashboard = cx.new(RemoteControlDashboard::new);
        workspace.add_item_to_active_pane(Box::new(dashboard), None, true, window, cx);
    }

    fn new(cx: &mut Context<Self>) -> Self {
        let ticker = cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(TICK).await;
                let Ok(()) = this.update(cx, |this, cx| this.refresh(cx)) else {
                    return;
                };
            }
        });
        Self::refresh_after_update(cx);
        Self {
            focus_handle: cx.focus_handle(),
            sessions: Vec::new(),
            session_views: HashMap::default(),
            scroll_handle: ScrollHandle::new(),
            refresh_task: None,
            _ticker: ticker,
        }
    }

    /// Refreshes once the current update is over: until then the sessions of
    /// the window being updated can't be listed.
    fn refresh_after_update(cx: &mut Context<Self>) {
        let this = cx.weak_entity();
        cx.defer(move |cx| {
            if let Some(this) = this.upgrade() {
                this.update(cx, |this, cx| this.refresh(cx));
            }
        });
    }

    /// Refreshes shortly, so a burst of terminal output causes one refresh.
    fn schedule_refresh(&mut self, cx: &mut Context<Self>) {
        if self.refresh_task.is_some() {
            return;
        }
        self.refresh_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(REFRESH_DEBOUNCE).await;
            this.update(cx, |this, cx| {
                this.refresh_task = None;
                this.refresh(cx);
            })
            .ok();
        }));
    }

    fn refresh(&mut self, cx: &mut Context<Self>) {
        let sessions = remote_sessions::remote_sessions(cx);
        self.session_views.retain(|entity_id, _| {
            sessions
                .iter()
                .any(|session| session.entity_id == *entity_id)
        });
        for session in &sessions {
            let Some(terminal) = session.terminal.upgrade() else {
                continue;
            };
            let view = self
                .session_views
                .entry(session.entity_id)
                .or_insert_with(|| SessionView {
                    input: None,
                    preview: None,
                    preview_expanded: None,
                    output: None,
                    url: None,
                    _subscription: cx.subscribe(&terminal, Self::handle_terminal_event),
                });
            let output = remote_sessions::terminal_output(terminal.read(cx));
            if let Some(url) = remote_sessions::extract_remote_control_url(&output) {
                view.url = Some(url);
            }
            view.output = model::output_preview(&output);
        }
        self.sessions = sessions;
        cx.notify();
    }

    fn handle_terminal_event(
        &mut self,
        _: Entity<Terminal>,
        event: &terminal::Event,
        cx: &mut Context<Self>,
    ) {
        if matches!(event, terminal::Event::Wakeup) {
            self.schedule_refresh(cx);
        }
    }

    /// Creates the editors of new sessions and shows the newest output in
    /// the open previews.
    fn sync_session_views(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        for session in &self.sessions {
            let Some(view) = self.session_views.get_mut(&session.entity_id) else {
                continue;
            };
            let running = session.state.is_running();
            let input = view.input.get_or_insert_with(|| {
                cx.new(|cx| {
                    let mut editor = Editor::single_line(window, cx);
                    editor.set_placeholder_text("Message or /command", window, cx);
                    editor
                })
            });
            if input.read(cx).read_only(cx) == running {
                input.update(cx, |editor, _| editor.set_read_only(!running));
            }
            if view.preview_expanded.unwrap_or(running) {
                let output = view.output.clone();
                view.preview
                    .get_or_insert_with(|| LogPreview::new(window, cx))
                    .sync(output, window, cx);
            } else {
                view.preview = None;
            }
        }
    }

    fn session(&self, entity_id: EntityId) -> Option<&RemoteSession> {
        self.sessions
            .iter()
            .find(|session| session.entity_id == entity_id)
    }

    fn running_terminal(&self, entity_id: EntityId) -> Option<Entity<Terminal>> {
        self.session(entity_id)
            .filter(|session| session.state.is_running())
            .and_then(|session| session.terminal.upgrade())
    }

    fn send_input(&mut self, entity_id: EntityId, window: &mut Window, cx: &mut Context<Self>) {
        let Some(terminal) = self.running_terminal(entity_id) else {
            return;
        };
        let Some(input) = self
            .session_views
            .get(&entity_id)
            .and_then(|view| view.input.clone())
        else {
            return;
        };
        let Some(text) = remote_sessions::text_to_send(&input.read(cx).text(cx)) else {
            return;
        };
        input.update(cx, |editor, cx| editor.clear(window, cx));
        remote_sessions::send_text(&terminal, text, cx);
        Self::refresh_after_update(cx);
    }

    fn interrupt(&mut self, entity_id: EntityId, cx: &mut Context<Self>) {
        if let Some(terminal) = self.running_terminal(entity_id) {
            remote_sessions::interrupt(&terminal, cx);
            Self::refresh_after_update(cx);
        }
    }

    fn stop(&mut self, entity_id: EntityId, cx: &mut Context<Self>) {
        if let Some(terminal) = self.running_terminal(entity_id) {
            remote_sessions::stop(&terminal, cx);
            Self::refresh_after_update(cx);
        }
    }

    fn show_terminal(&mut self, entity_id: EntityId, cx: &mut Context<Self>) {
        let Some(session) = self.session(entity_id).cloned() else {
            return;
        };
        // Activating the terminal updates the window this is called from.
        cx.defer(move |cx| remote_sessions::show_terminal(&session, cx));
        Self::refresh_after_update(cx);
    }

    fn toggle_preview(&mut self, entity_id: EntityId, cx: &mut Context<Self>) {
        let Some(running) = self
            .session(entity_id)
            .map(|session| session.state.is_running())
        else {
            return;
        };
        if let Some(view) = self.session_views.get_mut(&entity_id) {
            view.preview_expanded = Some(!view.preview_expanded.unwrap_or(running));
            cx.notify();
        }
    }

    fn running_count(&self) -> usize {
        self.sessions
            .iter()
            .filter(|session| session.state.is_running())
            .count()
    }

    fn render_new_session_button(&self, id: &'static str) -> Button {
        let focus_handle = self.focus_handle.clone();
        Button::new(id, "New Remote Control Session")
            .label_size(LabelSize::Small)
            .style(ButtonStyle::Subtle)
            .tooltip(Tooltip::text(
                "Start Claude Code with Remote Control in this project's terminal panel",
            ))
            .on_click(move |_, window, cx| {
                focus_handle.dispatch_action(&crate::NewRemoteControlSession, window, cx);
            })
    }

    fn render_header(&self, cx: &mut Context<Self>) -> AnyElement {
        let colors = Palette::new(cx);
        h_flex()
            .w_full()
            .px_3()
            .py_1p5()
            .gap_2()
            .justify_between()
            .border_b_1()
            .border_color(colors.border)
            .bg(colors.title_bar_background)
            .child(
                h_flex()
                    .gap_2()
                    .child(Icon::new(IconName::SignalHigh).color(Color::Muted))
                    .child(Label::new("Remote Control").weight(gpui::FontWeight::SEMIBOLD))
                    .child(
                        Label::new(remote_sessions::sessions_summary(
                            self.sessions.len(),
                            self.running_count(),
                        ))
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                    ),
            )
            .child(self.render_new_session_button("remote-new-session"))
            .into_any_element()
    }

    fn render_session(
        &self,
        session: &RemoteSession,
        now: Instant,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let colors = Palette::new(cx);
        let entity_id = session.entity_id;
        let key = entity_id.as_u64();
        let running = session.state.is_running();
        let view = self.session_views.get(&entity_id);
        let preview = view.and_then(|view| view.preview.as_ref());
        let preview_expanded = view
            .and_then(|view| view.preview_expanded)
            .unwrap_or(running);
        let url = view.and_then(|view| view.url.clone());
        let in_this_window = AnyWindowHandle::from(session.window) == window.window_handle();
        let duration = session
            .started_at
            .filter(|_| running)
            .map(|started| format_duration(now.saturating_duration_since(started)));

        let actions = h_flex()
            .flex_none()
            .gap_0p5()
            .when_some(url, |this, url| {
                let copied_url = url.clone();
                this.child(
                    Button::new(("remote-open", key), "Open on claude.ai")
                        .label_size(LabelSize::Small)
                        .style(ButtonStyle::Subtle)
                        .color(Color::Accent)
                        .tooltip(Tooltip::text(url.clone()))
                        .on_click(move |_, _, cx| cx.open_url(&url)),
                )
                .child(
                    IconButton::new(("remote-copy-link", key), IconName::Copy)
                        .icon_size(IconSize::XSmall)
                        .tooltip(Tooltip::text("Copy link"))
                        .on_click(move |_, _, cx| {
                            cx.write_to_clipboard(ClipboardItem::new_string(
                                copied_url.to_string(),
                            ));
                        }),
                )
            })
            .child(
                Button::new(("remote-interrupt", key), "Interrupt")
                    .label_size(LabelSize::Small)
                    .style(ButtonStyle::Subtle)
                    .disabled(!running)
                    .tooltip(Tooltip::text("Press Escape in the session"))
                    .on_click(cx.listener(move |this, _, _, cx| this.interrupt(entity_id, cx))),
            )
            .child(
                Button::new(("remote-show", key), "Show Terminal")
                    .label_size(LabelSize::Small)
                    .style(ButtonStyle::Subtle)
                    .on_click(cx.listener(move |this, _, _, cx| this.show_terminal(entity_id, cx))),
            )
            .child(
                Button::new(("remote-stop", key), "Stop")
                    .label_size(LabelSize::Small)
                    .style(ButtonStyle::Subtle)
                    .color(Color::Error)
                    .disabled(!running)
                    .tooltip(Tooltip::text("End the session"))
                    .on_click(cx.listener(move |this, _, _, cx| this.stop(entity_id, cx))),
            );

        let header = h_flex()
            .w_full()
            .px_2()
            .py_1()
            .gap_2()
            .child(
                Disclosure::new(("remote-disclosure", key), preview_expanded).on_click(
                    cx.listener(move |this, _, _, cx| this.toggle_preview(entity_id, cx)),
                ),
            )
            .child(if running {
                Icon::new(IconName::LoadCircle)
                    .size(IconSize::Small)
                    .color(Color::Accent)
                    .with_keyed_rotate_animation(("remote-spin", key), 2)
                    .into_any_element()
            } else {
                Icon::new(IconName::Terminal)
                    .size(IconSize::Small)
                    .color(Color::Muted)
                    .into_any_element()
            })
            .child(
                h_flex()
                    .flex_1()
                    .min_w_0()
                    .gap_2()
                    .child(
                        Label::new(
                            session
                                .project
                                .clone()
                                .unwrap_or_else(|| "No project".into()),
                        )
                        .weight(gpui::FontWeight::MEDIUM)
                        .truncate(),
                    )
                    .when_some(session.working_directory.clone(), |this, directory| {
                        this.child(
                            div().min_w_0().child(
                                Label::new(directory)
                                    .size(LabelSize::XSmall)
                                    .color(Color::Muted)
                                    .truncate(),
                            ),
                        )
                    })
                    .child(
                        Label::new(if in_this_window {
                            "This window"
                        } else {
                            "Another window"
                        })
                        .size(LabelSize::XSmall)
                        .color(Color::Muted),
                    ),
            )
            .child(
                h_flex()
                    .flex_none()
                    .gap_2()
                    .child(
                        Label::new(session.state.label())
                            .size(LabelSize::Small)
                            .color(if running { Color::Accent } else { Color::Muted }),
                    )
                    .when_some(duration, |this, duration| {
                        this.child(
                            Label::new(duration)
                                .size(LabelSize::Small)
                                .color(Color::Muted),
                        )
                    }),
            )
            .child(actions);

        let preview = preview.filter(|_| preview_expanded).map(|preview| {
            v_flex()
                .ml(px(22.))
                .mr_1()
                .rounded_sm()
                .border_1()
                .border_color(colors.border_variant)
                .bg(colors.editor_background)
                .child(
                    h_flex()
                        .px_1p5()
                        .border_b_1()
                        .border_color(colors.border_variant)
                        .child(
                            Label::new(if preview.paused {
                                "Paused while you select \u{b7} click outside to resume"
                            } else {
                                "Terminal output"
                            })
                            .size(LabelSize::XSmall)
                            .color(Color::Muted),
                        ),
                )
                .child(div().p_1p5().child(preview.editor.clone()))
        });

        let input = view.and_then(|view| view.input.clone()).map(|input| {
            h_flex()
                .ml(px(22.))
                .mr_1()
                .gap_1()
                .on_action(cx.listener(move |this, _: &menu::Confirm, window, cx| {
                    this.send_input(entity_id, window, cx);
                }))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .px_1p5()
                        .py_0p5()
                        .rounded_sm()
                        .border_1()
                        .border_color(colors.border_variant)
                        .child(input),
                )
                .child(
                    Button::new(("remote-send", key), "Send")
                        .label_size(LabelSize::Small)
                        .disabled(!running)
                        .tooltip(Tooltip::text("Type this into the session and press Enter"))
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.send_input(entity_id, window, cx);
                        })),
                )
        });

        v_flex()
            .w_full()
            .py_1()
            .gap_1()
            .border_b_1()
            .border_color(colors.border_variant)
            .child(header)
            .children(preview)
            .children(input)
            .into_any_element()
    }
}

impl Render for RemoteControlDashboard {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.sync_session_views(window, cx);
        let now = Instant::now();
        let colors = Palette::new(cx);
        let header = self.render_header(cx);

        let body = if self.sessions.is_empty() {
            v_flex()
                .size_full()
                .items_center()
                .justify_center()
                .gap_2()
                .child(
                    Icon::new(IconName::SignalHigh)
                        .size(IconSize::XLarge)
                        .color(Color::Muted),
                )
                .child(Label::new("No Remote Control sessions").color(Color::Muted))
                .child(
                    Label::new(
                        "A Remote Control session is Claude Code running in a terminal here \
                         that you can also use from claude.ai/code and the mobile app. \
                         Sessions from every project tab and window show up here.",
                    )
                    .size(LabelSize::Small)
                    .color(Color::Muted),
                )
                .child(self.render_new_session_button("remote-new-session-empty"))
                .into_any_element()
        } else {
            let mut list = v_flex()
                .id("remote-control-dashboard-list")
                .size_full()
                .overflow_y_scroll()
                .track_scroll(&self.scroll_handle)
                .p_1();
            for session in &self.sessions {
                list = list.child(self.render_session(session, now, window, cx));
            }
            list.into_any_element()
        };

        v_flex()
            .key_context("RemoteControlDashboard")
            .track_focus(&self.focus_handle)
            .size_full()
            .bg(colors.editor_background)
            .child(header)
            .child(body)
    }
}

impl Focusable for RemoteControlDashboard {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

pub enum RemoteDashboardEvent {}

impl EventEmitter<RemoteDashboardEvent> for RemoteControlDashboard {}

impl Item for RemoteControlDashboard {
    type Event = RemoteDashboardEvent;

    fn tab_content_text(&self, _detail: usize, _cx: &App) -> SharedString {
        "Remote Control".into()
    }

    fn tab_icon(&self, _window: &Window, _cx: &App) -> Option<Icon> {
        Some(Icon::new(IconName::SignalHigh))
    }

    fn tab_tooltip_text(&self, _: &App) -> Option<SharedString> {
        Some("Every Claude Code Remote Control session open in OTerminal".into())
    }

    fn to_item_events(_: &Self::Event, _: &mut dyn FnMut(ItemEvent)) {}

    fn tab_content(&self, params: TabContentParams, _window: &Window, _cx: &App) -> AnyElement {
        let running = self.running_count();
        h_flex()
            .gap_1()
            .child(Label::new("Remote Control").color(params.text_color()))
            .when(running > 0, |this| {
                this.child(
                    Label::new(running.to_string())
                        .size(LabelSize::XSmall)
                        .color(Color::Accent),
                )
            })
            .into_any_element()
    }
}
