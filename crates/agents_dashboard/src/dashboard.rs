//! The "Agents" workspace item: every Claude Code thread of the app, live,
//! with its sub-agent tree and background work.

use std::path::PathBuf;
use std::time::{Duration, Instant, SystemTime};

use collections::{HashMap, HashSet};
use editor::{Editor, MultiBufferOffset, actions::MoveToEnd};
use gpui::{
    AnyElement, App, ClickEvent, ClipboardItem, Context, ElementId, Entity, EntityId, EventEmitter,
    FocusHandle, Focusable, IntoElement, ParentElement, Render, ScrollHandle, SharedString, Styled,
    Subscription, Task, TextStyleRefinement, WeakEntity, Window, div, px,
};
use ui::{
    Chip, CommonAnimationExt as _, ContextMenu, Disclosure, Icon, IconButton, IconName, Tooltip,
    prelude::*,
};
use workspace::{
    Item, OpenOptions, Workspace,
    item::{ItemEvent, TabContentParams},
};

use crate::model::{
    AgentNode, AgentNodeKind, BackgroundItem, BackgroundKind, BackgroundStatus, NodeStatus,
    ThreadState, format_duration, format_tokens,
};
use crate::store::{AgentsStore, LocationKind, ThreadSnapshot};

pub(crate) const TICK: Duration = Duration::from_secs(1);
const LOG_PREVIEW_MAX_LINES: usize = 16;

/// An open preview of the output of a background item or a terminal.
pub(crate) struct LogPreview {
    /// Read-only, so the output can be selected and copied.
    pub(crate) editor: Entity<Editor>,
    /// The output `editor` shows.
    pub(crate) text: Option<SharedString>,
    /// New output is held back while the user works in `editor`, so their
    /// selection and scroll position survive.
    pub(crate) paused: bool,
}

impl LogPreview {
    pub(crate) fn new(window: &mut Window, cx: &mut App) -> Self {
        let editor = cx.new(|cx| {
            let mut editor = Editor::auto_height(1, LOG_PREVIEW_MAX_LINES, window, cx);
            editor.set_read_only(true);
            editor.set_show_gutter(false, cx);
            editor.set_placeholder_text("No output yet", window, cx);
            editor.set_custom_context_menu(|editor, _point, window, cx| {
                let has_selection = editor.has_non_empty_selection(&editor.display_snapshot(cx));
                Some(ContextMenu::build(window, cx, |menu, _, _| {
                    menu.action_disabled_when(
                        !has_selection,
                        "Copy",
                        Box::new(editor::actions::Copy),
                    )
                    .action("Select All", Box::new(editor::actions::SelectAll))
                }))
            });
            editor.set_text_style_refinement(TextStyleRefinement {
                font_family: Some(theme::theme_settings(cx).buffer_font(cx).family.clone()),
                font_size: Some(
                    TextSize::XSmall
                        .rems(cx)
                        .to_pixels(window.rem_size())
                        .into(),
                ),
                ..Default::default()
            });
            editor
        });
        Self {
            editor,
            text: None,
            paused: false,
        }
    }

    /// Shows `output` and scrolls to its end, unless the user is working in
    /// the preview.
    pub(crate) fn sync(&mut self, output: Option<SharedString>, window: &mut Window, cx: &mut App) {
        self.paused =
            self.text != output && self.editor.focus_handle(cx).contains_focused(window, cx);
        if self.text == output || self.paused {
            return;
        }
        self.editor.update(cx, |editor, cx| {
            editor.set_text(output.as_deref().unwrap_or_default(), window, cx);
            editor.move_to_end(&MoveToEnd, window, cx);
        });
        self.text = output;
    }
}

pub struct AgentsDashboard {
    store: Entity<AgentsStore>,
    workspace: WeakEntity<Workspace>,
    focus_handle: FocusHandle,
    /// Collapsed nodes, keyed by thread and node id (`""` = the thread).
    collapsed: HashSet<(EntityId, SharedString)>,
    show_inactive: bool,
    /// Open log previews of background items, keyed by thread and call id.
    log_previews: HashMap<(EntityId, SharedString), LogPreview>,
    scroll_handle: ScrollHandle,
    ticker: Option<Task<()>>,
    _subscriptions: Vec<Subscription>,
}

impl AgentsDashboard {
    /// Activates the dashboard of this workspace, opening it if needed.
    pub fn deploy(workspace: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
        if let Some(existing) = workspace.item_of_type::<AgentsDashboard>(cx) {
            workspace.activate_item(&existing, true, true, window, cx);
            return;
        }
        let Some(store) = AgentsStore::global(cx) else {
            return;
        };
        let workspace_handle = cx.weak_entity();
        let dashboard = cx.new(|cx| AgentsDashboard::new(store, workspace_handle, cx));
        workspace.add_item_to_active_pane(Box::new(dashboard), None, true, window, cx);
    }

    fn new(
        store: Entity<AgentsStore>,
        workspace: WeakEntity<Workspace>,
        cx: &mut Context<Self>,
    ) -> Self {
        let subscriptions = vec![cx.observe(&store, |this, _, cx| {
            this.update_ticker(cx);
            cx.notify();
        })];
        store.update(cx, |store, cx| store.add_viewer(cx));
        cx.on_release({
            let store = store.downgrade();
            move |_, cx| {
                store.update(cx, |store, cx| store.remove_viewer(cx)).ok();
            }
        })
        .detach();
        let mut this = Self {
            store,
            workspace,
            focus_handle: cx.focus_handle(),
            collapsed: HashSet::default(),
            show_inactive: true,
            log_previews: HashMap::default(),
            scroll_handle: ScrollHandle::new(),
            ticker: None,
            _subscriptions: subscriptions,
        };
        this.update_ticker(cx);
        this
    }

    /// Re-renders every second (elapsed times, monitor timeouts) while
    /// something is running; otherwise renders only on store updates.
    fn update_ticker(&mut self, cx: &mut Context<Self>) {
        let (agents, background) = self.store.read(cx).running_counts(Instant::now());
        if agents + background == 0 {
            self.ticker = None;
            return;
        }
        if self.ticker.is_some() {
            return;
        }
        self.ticker = Some(cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(TICK).await;
                let Ok(()) = this.update(cx, |_, cx| cx.notify()) else {
                    return;
                };
            }
        }));
    }

    fn toggle_collapsed(&mut self, key: (EntityId, SharedString), cx: &mut Context<Self>) {
        if !self.collapsed.remove(&key) {
            self.collapsed.insert(key);
        }
        cx.notify();
    }

    fn toggle_log_preview(
        &mut self,
        key: (EntityId, SharedString),
        output_path: Option<PathBuf>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let previewed = self.log_previews.remove(&key).is_none();
        if previewed {
            self.log_previews.insert(key, LogPreview::new(window, cx));
        }
        if let Some(path) = output_path {
            self.store.update(cx, |store, cx| {
                store.set_output_previewed(path, previewed, cx)
            });
        }
        cx.notify();
    }

    /// Shows the newest output in the open log previews and scrolls them to
    /// its end, except in one the user is working in.
    fn sync_log_previews(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.log_previews.is_empty() {
            return;
        }
        let mut outputs = HashMap::default();
        for snapshot in self.store.read(cx).snapshots() {
            for item in &snapshot.background {
                let key = (snapshot.entity_id, item.call_id.clone());
                if self.log_previews.contains_key(&key) {
                    outputs.insert(key, item.output.clone());
                }
            }
        }
        for (key, preview) in &mut self.log_previews {
            let Some(output) = outputs.remove(key) else {
                continue;
            };
            preview.sync(output, window, cx);
        }
    }

    /// The text selected in a log preview, or all of it without a selection.
    fn log_preview_text(preview: &LogPreview, cx: &mut App) -> Option<String> {
        let selected = preview.editor.update(cx, |editor, cx| {
            let snapshot = editor.display_snapshot(cx);
            let selection = editor.selections.newest::<MultiBufferOffset>(&snapshot);
            snapshot
                .buffer_snapshot()
                .text_for_range(selection.start..selection.end)
                .collect::<String>()
        });
        if selected.trim().is_empty() {
            preview.text.as_ref().map(|text| text.to_string())
        } else {
            Some(selected)
        }
    }

    /// Puts the selected part of a log preview (or all of it) into the
    /// message editor of the thread that started the command.
    fn send_log_to_thread(
        &mut self,
        key: &(EntityId, SharedString),
        command: &str,
        output_path: Option<&PathBuf>,
        cx: &mut Context<Self>,
    ) {
        let Some(text) = self
            .log_previews
            .get(key)
            .and_then(|preview| Self::log_preview_text(preview, cx))
        else {
            return;
        };
        let full_log = output_path
            .map(|path| format!(" (full log: {})", path.display()))
            .unwrap_or_default();
        let message = format!(
            "Output of the background command `{}`{full_log}:\n```\n{text}\n```\n",
            first_line(command)
        );
        let thread = key.0;
        // Activating the thread updates the window and workspace this is
        // called from.
        cx.defer(move |cx| AgentsStore::insert_into_thread_message(thread, message, cx));
    }

    fn render_header(
        &self,
        snapshots: &[&ThreadSnapshot],
        now: Instant,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let (agents, background) = self.store.read(cx).running_counts(now);
        let waiting = snapshots
            .iter()
            .filter(|snapshot| snapshot.state == ThreadState::WaitingForPermission)
            .count();
        let sub_agents: usize = snapshots
            .iter()
            .map(|snapshot| snapshot.root.descendant_count())
            .sum();
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
                    .child(Icon::new(IconName::UserGroup).color(Color::Muted))
                    .child(Label::new("Agents").weight(gpui::FontWeight::SEMIBOLD))
                    .child(
                        Label::new(format!(
                            "{} threads · {} sub-agents",
                            snapshots.len(),
                            sub_agents
                        ))
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                    ),
            )
            .child(
                h_flex()
                    .gap_1()
                    .when(agents > 0, |this| {
                        this.child(
                            Chip::new(format!("{agents} running"))
                                .icon(IconName::LoadCircle)
                                .icon_color(Color::Accent)
                                .label_color(Color::Accent),
                        )
                    })
                    .when(waiting > 0, |this| {
                        this.child(
                            Chip::new(format!("{waiting} need permission"))
                                .icon(IconName::Warning)
                                .icon_color(Color::Warning)
                                .label_color(Color::Warning),
                        )
                    })
                    .when(background > 0, |this| {
                        this.child(
                            Chip::new(format!("{background} background")).icon(IconName::Terminal),
                        )
                    })
                    .child(
                        Button::new(
                            "toggle-inactive",
                            if self.show_inactive {
                                "Hide idle"
                            } else {
                                "Show idle"
                            },
                        )
                        .label_size(LabelSize::Small)
                        .style(ButtonStyle::Subtle)
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.show_inactive = !this.show_inactive;
                            cx.notify();
                        })),
                    ),
            )
            .into_any_element()
    }

    fn render_thread(
        &self,
        snapshot: &ThreadSnapshot,
        now: Instant,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let entity_id = snapshot.entity_id;
        let colors = Palette::new(cx);
        let thread_key = (entity_id, SharedString::default());
        let collapsed = self.collapsed.contains(&thread_key);
        let has_details = !snapshot.root.children.is_empty() || !snapshot.background.is_empty();

        let elapsed = snapshot.turn_started_at.map(|started| {
            snapshot
                .turn_ended_at
                .unwrap_or(now)
                .saturating_duration_since(started)
        });
        let mut stats: Vec<String> = Vec::new();
        if snapshot.tool_calls > 0 {
            stats.push(format!("{} tools", snapshot.tool_calls));
        }
        if snapshot.edits > 0 {
            stats.push(format!("{} edits", snapshot.edits));
        }
        if let Some((used, max)) = snapshot.tokens
            && used > 0
        {
            if max > 0 {
                stats.push(format!("{}/{}", format_tokens(used), format_tokens(max)));
            } else {
                stats.push(format!("{} tokens", format_tokens(used)));
            }
        }
        if let Some((amount, currency)) = &snapshot.cost {
            if currency.eq_ignore_ascii_case("USD") {
                stats.push(format!("${amount:.2}"));
            } else {
                stats.push(format!("{amount:.2} {currency}"));
            }
        }

        let location = match snapshot.location {
            Some(LocationKind::Panel) => Some("Panel"),
            Some(LocationKind::Tab) => Some("Tab"),
            None => None,
        };
        let header = h_flex()
            .id(("thread-header", entity_id.as_u64()))
            .w_full()
            .px_2()
            .py_1()
            .gap_2()
            .rounded_sm()
            .cursor_pointer()
            .hover(|style| style.bg(colors.ghost_element_hover))
            .on_click(move |_: &ClickEvent, _, cx| {
                AgentsStore::focus_thread(entity_id, None, cx);
            })
            .tooltip(Tooltip::text("Go to thread"))
            .child(
                Disclosure::new(("thread-disclosure", entity_id.as_u64()), !collapsed)
                    .disabled(!has_details)
                    .on_click(cx.listener(move |this, _, _, cx| {
                        cx.stop_propagation();
                        this.toggle_collapsed((entity_id, SharedString::default()), cx);
                    })),
            )
            .child(thread_state_icon(snapshot.state, entity_id))
            .child(
                h_flex()
                    .flex_1()
                    .min_w_0()
                    .gap_2()
                    .child(
                        Label::new(snapshot.title.clone())
                            .weight(gpui::FontWeight::MEDIUM)
                            .truncate(),
                    )
                    .when_some(snapshot.project.clone(), |this, project| {
                        this.child(
                            Chip::new(project)
                                .icon(IconName::Folder)
                                .label_size(LabelSize::XSmall),
                        )
                    })
                    .when_some(location, |this, location| {
                        this.child(
                            Label::new(location)
                                .size(LabelSize::XSmall)
                                .color(Color::Muted),
                        )
                    }),
            )
            .child(
                h_flex()
                    .flex_none()
                    .gap_2()
                    .child(
                        Label::new(snapshot.state.label())
                            .size(LabelSize::Small)
                            .color(thread_state_color(snapshot.state)),
                    )
                    .when_some(elapsed, |this, elapsed| {
                        this.child(
                            Label::new(format_duration(elapsed))
                                .size(LabelSize::Small)
                                .color(Color::Muted),
                        )
                    })
                    .when(!stats.is_empty(), |this| {
                        this.child(
                            Label::new(stats.join(" · "))
                                .size(LabelSize::Small)
                                .color(Color::Muted),
                        )
                    }),
            );

        let activity = snapshot
            .root
            .activity
            .as_ref()
            .filter(|_| snapshot.state.is_active())
            .map(|activity| {
                let entry_ix = activity.entry_ix;
                h_flex()
                    .id(("thread-activity", entity_id.as_u64()))
                    .pl(px(44.))
                    .pr_2()
                    .gap_1()
                    .cursor_pointer()
                    .on_click(move |_: &ClickEvent, _, cx| {
                        AgentsStore::focus_thread(entity_id, Some(entry_ix), cx);
                    })
                    .child(
                        Icon::new(IconName::ArrowUpRight)
                            .size(IconSize::XSmall)
                            .color(Color::Muted),
                    )
                    .child(
                        Label::new(activity.text.clone())
                            .size(LabelSize::Small)
                            .color(Color::Muted)
                            .truncate(),
                    )
            });

        let mut body = v_flex().w_full().pl(px(22.)).pr_2();
        if !collapsed {
            for (ix, child) in snapshot.root.children.iter().enumerate() {
                body = body.child(self.render_node(entity_id, child, ix, now, cx));
            }
            if !snapshot.background.is_empty() {
                body = body.child(self.render_background(entity_id, &snapshot.background, now, cx));
            }
        }

        v_flex()
            .w_full()
            .py_1()
            .border_b_1()
            .border_color(colors.border_variant)
            .child(header)
            .children(activity)
            .child(body)
            .into_any_element()
    }

    fn render_node(
        &self,
        thread: EntityId,
        node: &AgentNode,
        ix: usize,
        now: Instant,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let colors = Palette::new(cx);
        let node_id = node.id.clone().unwrap_or_default();
        let key = (thread, node_id.clone());
        let collapsed = self.collapsed.contains(&key);
        let element_key: SharedString = format!("{}-{node_id}-{ix}", thread.as_u64()).into();
        let entry_ix = node.entry_ix;

        let duration = node.timing.started_at.map(|started| {
            node.timing
                .finished_at
                .unwrap_or(now)
                .saturating_duration_since(started)
        });
        let mut stats: Vec<String> = Vec::new();
        if node.tool_calls > 0 {
            stats.push(format!("{} tools", node.tool_calls));
        }
        if node.edits > 0 {
            stats.push(format!("{} edits", node.edits));
        }
        let detail = if node.status.is_active() {
            node.activity.as_ref().map(|activity| activity.text.clone())
        } else {
            node.last_output
                .clone()
                .or_else(|| node.activity.as_ref().map(|activity| activity.text.clone()))
        };
        let icon = match node.kind {
            AgentNodeKind::Workflow => IconName::ListTree,
            _ => IconName::ToolThink,
        };

        let row = h_flex()
            .id(ElementId::Name(format!("node-{element_key}").into()))
            .w_full()
            .px_1()
            .py_0p5()
            .gap_1p5()
            .rounded_sm()
            .cursor_pointer()
            .hover(|style| style.bg(colors.ghost_element_hover))
            .on_click(move |_: &ClickEvent, _, cx| {
                AgentsStore::focus_thread(thread, entry_ix, cx);
            })
            .child(
                Disclosure::new(
                    ElementId::Name(format!("node-disclosure-{element_key}").into()),
                    !collapsed,
                )
                .disabled(node.children.is_empty())
                .on_click(cx.listener(move |this, _, _, cx| {
                    cx.stop_propagation();
                    this.toggle_collapsed(key.clone(), cx);
                })),
            )
            .child(node_status_icon(
                node.status,
                format!("node-status-{element_key}"),
            ))
            .child(Icon::new(icon).size(IconSize::XSmall).color(Color::Muted))
            .child(
                h_flex()
                    .flex_1()
                    .min_w_0()
                    .gap_1p5()
                    .child(
                        Label::new(node.label.clone())
                            .size(LabelSize::Small)
                            .truncate(),
                    )
                    .when_some(node.subagent_type.clone(), |this, subagent_type| {
                        this.child(Chip::new(subagent_type).label_size(LabelSize::XSmall))
                    })
                    .when(node.background, |this| {
                        this.child(
                            Label::new("background")
                                .size(LabelSize::XSmall)
                                .color(Color::Muted),
                        )
                    })
                    .when_some(detail, |this, detail| {
                        this.child(
                            Label::new(detail)
                                .size(LabelSize::XSmall)
                                .color(Color::Muted)
                                .truncate(),
                        )
                    }),
            )
            .child(
                h_flex()
                    .flex_none()
                    .gap_2()
                    .when(!node.status.is_active(), |this| {
                        this.child(
                            Label::new(node.status.label())
                                .size(LabelSize::XSmall)
                                .color(node_status_color(node.status)),
                        )
                    })
                    .when_some(duration, |this, duration| {
                        this.child(
                            Label::new(format_duration(duration))
                                .size(LabelSize::XSmall)
                                .color(Color::Muted),
                        )
                    })
                    .when(!stats.is_empty(), |this| {
                        this.child(
                            Label::new(stats.join(" · "))
                                .size(LabelSize::XSmall)
                                .color(Color::Muted),
                        )
                    }),
            );

        let phases = (!node.phases.is_empty()).then(|| {
            h_flex()
                .pl(px(40.))
                .gap_1()
                .flex_wrap()
                .child(
                    Label::new("Phases")
                        .size(LabelSize::XSmall)
                        .color(Color::Muted),
                )
                .children(node.phases.iter().enumerate().map(|(ix, phase)| {
                    h_flex()
                        .gap_1()
                        .when(ix > 0, |this| {
                            this.child(Label::new("→").size(LabelSize::XSmall).color(Color::Muted))
                        })
                        .child(Chip::new(phase.clone()).label_size(LabelSize::XSmall))
                }))
        });

        let children = (!collapsed && !node.children.is_empty()).then(|| {
            let mut container = v_flex()
                .ml(px(10.))
                .pl(px(6.))
                .border_l_1()
                .border_color(colors.border_variant);
            for (ix, child) in node.children.iter().enumerate() {
                container = container.child(self.render_node(thread, child, ix, now, cx));
            }
            container
        });

        v_flex()
            .w_full()
            .child(row)
            .children(phases)
            .children(children)
            .into_any_element()
    }

    fn render_background(
        &self,
        thread: EntityId,
        items: &[BackgroundItem],
        now: Instant,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let key = (thread, SharedString::from("\u{0}background"));
        let collapsed = self.collapsed.contains(&key);
        let running = items
            .iter()
            .filter(|item| item.status_at(now).is_running())
            .count();
        let colors = Palette::new(cx);

        let header = h_flex()
            .px_1()
            .py_0p5()
            .gap_1p5()
            .child(
                Disclosure::new(("background-disclosure", thread.as_u64()), !collapsed).on_click(
                    cx.listener(move |this, _, _, cx| this.toggle_collapsed(key.clone(), cx)),
                ),
            )
            .child(
                Icon::new(IconName::Terminal)
                    .size(IconSize::XSmall)
                    .color(Color::Muted),
            )
            .child(
                Label::new(format!("Background · {running} running of {}", items.len()))
                    .size(LabelSize::Small)
                    .color(Color::Muted),
            );

        let rows = (!collapsed).then(|| {
            let mut container = v_flex()
                .ml(px(10.))
                .pl(px(6.))
                .border_l_1()
                .border_color(colors.border_variant);
            for (ix, item) in items.iter().enumerate() {
                container = container.child(self.render_background_item(thread, item, ix, now, cx));
            }
            container
        });

        v_flex()
            .w_full()
            .child(header)
            .children(rows)
            .into_any_element()
    }

    fn render_background_item(
        &self,
        thread: EntityId,
        item: &BackgroundItem,
        ix: usize,
        now: Instant,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let colors = Palette::new(cx);
        let status = item.status_at(now);
        let element_key: SharedString = format!("{}-{}-{ix}", thread.as_u64(), item.call_id).into();
        let entry_ix = item.entry_ix;
        let (status_text, status_color) = match status {
            BackgroundStatus::Running => ("Running", Color::Accent),
            BackgroundStatus::Stopped => ("Stopped", Color::Muted),
            BackgroundStatus::Exited(Some(code)) if code != 0 => ("Exited", Color::Error),
            BackgroundStatus::Exited(_) => ("Exited", Color::Muted),
            BackgroundStatus::Failed => ("Failed", Color::Error),
            BackgroundStatus::Expired => ("Timed out", Color::Muted),
        };
        let exit_code = match status {
            BackgroundStatus::Exited(Some(code)) => Some(format!("code {code}")),
            _ => None,
        };
        let duration = item
            .started_at
            .filter(|_| status.is_running())
            .map(|started| format_duration(now.saturating_duration_since(started)));
        let quiet_for = item
            .last_output_at
            .filter(|_| status.is_running())
            .and_then(|at| SystemTime::now().duration_since(at).ok())
            .filter(|quiet| *quiet >= Duration::from_secs(60))
            .map(|quiet| format!("quiet {}", format_duration(quiet)));
        let kind_icon = match item.kind {
            BackgroundKind::Shell | BackgroundKind::Terminal => IconName::Terminal,
            BackgroundKind::Monitor => IconName::Eye,
        };
        let tooltip: SharedString = {
            let mut lines = vec![item.command.to_string()];
            if let Some(description) = &item.description {
                lines.push(description.to_string());
            }
            if let Some(cwd) = &item.cwd {
                lines.push(format!("in {cwd}"));
            }
            if let Some(task_id) = &item.task_id {
                lines.push(format!("task {task_id}"));
            }
            if item.kind == BackgroundKind::Shell && status.is_running() {
                lines.push(
                    "Claude Code doesn't report when background commands exit; \
                     \"Running\" means not seen stopped."
                        .into(),
                );
            }
            lines.join("\n").into()
        };

        let url = item.url.clone().filter(|_| status.is_running());
        let output_path = item.output_path.clone();
        let workspace = self.workspace.clone();
        let can_stop = item.can_stop && status.is_running();
        let call_id = item.call_id.clone();
        let store = self.store.clone();

        let preview_key = (thread, item.call_id.clone());
        let open_preview = self.log_previews.get(&preview_key);
        let has_preview = item.output_path.is_some() || item.kind == BackgroundKind::Terminal;
        let preview_toggle = has_preview.then(|| {
            let preview_key = preview_key.clone();
            let output_path = item.output_path.clone();
            IconButton::new(
                ElementId::Name(format!("bg-preview-{element_key}").into()),
                IconName::ToolTerminal,
            )
            .icon_size(IconSize::XSmall)
            .toggle_state(open_preview.is_some())
            .tooltip(Tooltip::text(if open_preview.is_some() {
                "Hide output"
            } else {
                "Preview output"
            }))
            .on_click(cx.listener(move |this, _, window, cx| {
                this.toggle_log_preview(preview_key.clone(), output_path.clone(), window, cx);
            }))
        });
        let preview =
            open_preview.map(|preview| {
                let copy_text = preview.text.clone();
                let command = item.command.clone();
                let output_path = item.output_path.clone();
                let header = h_flex()
                .px_1p5()
                .gap_1()
                .justify_between()
                .border_b_1()
                .border_color(colors.border_variant)
                .child(
                    Label::new(if preview.paused {
                        "Paused while you select \u{b7} click outside to resume"
                    } else {
                        "Last lines of output"
                    })
                    .size(LabelSize::XSmall)
                    .color(Color::Muted),
                )
                .child(
                    h_flex()
                        .gap_0p5()
                        .child(
                            Button::new(
                                ElementId::Name(format!("bg-log-send-{element_key}").into()),
                                "Send to Claude",
                            )
                            .label_size(LabelSize::XSmall)
                            .style(ButtonStyle::Subtle)
                            .tooltip(Tooltip::text(
                                "Add the selected lines (or all of them) to this thread's message",
                            ))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.send_log_to_thread(
                                    &preview_key,
                                    &command,
                                    output_path.as_ref(),
                                    cx,
                                );
                            })),
                        )
                        .child(
                            IconButton::new(
                                ElementId::Name(format!("bg-log-copy-{element_key}").into()),
                                IconName::Copy,
                            )
                            .icon_size(IconSize::XSmall)
                            .disabled(copy_text.is_none())
                            .tooltip(Tooltip::text("Copy all"))
                            .on_click(move |_, _, cx| {
                                if let Some(text) = &copy_text {
                                    cx.write_to_clipboard(ClipboardItem::new_string(
                                        text.to_string(),
                                    ));
                                }
                            }),
                        ),
                );
                v_flex()
                    .ml(px(22.))
                    .mr_1()
                    .mb_1()
                    .rounded_sm()
                    .border_1()
                    .border_color(colors.border_variant)
                    .bg(colors.editor_background)
                    .child(header)
                    .child(div().p_1p5().child(preview.editor.clone()))
            });

        let actions = h_flex()
            .flex_none()
            .gap_0p5()
            .children(preview_toggle)
            .when_some(url.clone(), |this, url| {
                this.child(
                    Button::new(
                        ElementId::Name(format!("bg-url-{element_key}").into()),
                        url.clone(),
                    )
                    .label_size(LabelSize::XSmall)
                    .style(ButtonStyle::Subtle)
                    .color(Color::Accent)
                    .tooltip(Tooltip::text("Open in browser"))
                    .on_click(move |_, _, cx| cx.open_url(&url)),
                )
            })
            .child(
                IconButton::new(
                    ElementId::Name(format!("bg-output-{element_key}").into()),
                    IconName::File,
                )
                .icon_size(IconSize::XSmall)
                .tooltip(Tooltip::text(if output_path.is_some() {
                    "Open output file"
                } else {
                    "Show in thread"
                }))
                .on_click(move |_, window, cx| {
                    if let Some(path) = output_path.clone()
                        && let Some(workspace) = workspace.upgrade()
                    {
                        workspace.update(cx, |workspace, cx| {
                            workspace
                                .open_abs_path(path, OpenOptions::default(), window, cx)
                                .detach_and_log_err(cx);
                        });
                    } else {
                        AgentsStore::focus_thread(thread, Some(entry_ix), cx);
                    }
                }),
            )
            .when(can_stop, |this| {
                this.child(
                    IconButton::new(
                        ElementId::Name(format!("bg-stop-{element_key}").into()),
                        IconName::Stop,
                    )
                    .icon_size(IconSize::XSmall)
                    .icon_color(Color::Error)
                    .tooltip(Tooltip::text("Stop"))
                    .on_click(move |_, _, cx| {
                        if let Some(thread) = store.read(cx).thread(thread) {
                            AgentsStore::stop_terminal(&thread, &call_id, cx);
                        }
                    }),
                )
            });

        let row = h_flex()
            .id(ElementId::Name(format!("bg-{element_key}").into()))
            .w_full()
            .px_1()
            .py_0p5()
            .gap_1p5()
            .rounded_sm()
            .hover(|style| style.bg(colors.ghost_element_hover))
            .tooltip(Tooltip::text(tooltip))
            .child(if status.is_running() {
                Icon::new(IconName::LoadCircle)
                    .size(IconSize::XSmall)
                    .color(Color::Accent)
                    .with_keyed_rotate_animation(
                        ElementId::Name(format!("bg-spin-{element_key}").into()),
                        3,
                    )
                    .into_any_element()
            } else {
                Icon::new(kind_icon)
                    .size(IconSize::XSmall)
                    .color(Color::Muted)
                    .into_any_element()
            })
            .child(
                h_flex()
                    .flex_1()
                    .min_w_0()
                    .gap_1p5()
                    .child(
                        div().min_w_0().child(
                            Label::new(first_line(&item.command))
                                .size(LabelSize::Small)
                                .buffer_font(cx)
                                .truncate(),
                        ),
                    )
                    .when_some(
                        item.last_line.clone().filter(|_| status.is_running()),
                        |this, line| {
                            this.child(
                                Label::new(line)
                                    .size(LabelSize::XSmall)
                                    .color(Color::Muted)
                                    .truncate(),
                            )
                        },
                    ),
            )
            .child(
                h_flex()
                    .flex_none()
                    .gap_2()
                    .child(
                        Label::new(status_text)
                            .size(LabelSize::XSmall)
                            .color(status_color),
                    )
                    .when_some(exit_code, |this, code| {
                        this.child(Label::new(code).size(LabelSize::XSmall).color(Color::Muted))
                    })
                    .when_some(duration, |this, duration| {
                        this.child(
                            Label::new(duration)
                                .size(LabelSize::XSmall)
                                .color(Color::Muted),
                        )
                    })
                    .when_some(quiet_for, |this, quiet| {
                        this.child(
                            Label::new(quiet)
                                .size(LabelSize::XSmall)
                                .color(Color::Muted),
                        )
                    }),
            )
            .child(actions);

        v_flex()
            .w_full()
            .child(row)
            .children(preview)
            .into_any_element()
    }
}

fn first_line(text: &str) -> SharedString {
    let mut lines = text.lines();
    let first = lines.next().unwrap_or_default();
    if lines.next().is_some() {
        format!("{first} …").into()
    } else {
        first.to_string().into()
    }
}

fn thread_state_color(state: ThreadState) -> Color {
    match state {
        ThreadState::Generating => Color::Accent,
        ThreadState::WaitingForPermission => Color::Warning,
        ThreadState::Error => Color::Error,
        ThreadState::Finished => Color::Success,
        ThreadState::Idle => Color::Muted,
    }
}

fn thread_state_icon(state: ThreadState, entity_id: EntityId) -> AnyElement {
    match state {
        ThreadState::Generating => Icon::new(IconName::LoadCircle)
            .size(IconSize::Small)
            .color(Color::Accent)
            .with_keyed_rotate_animation(("thread-spin", entity_id.as_u64()), 2)
            .into_any_element(),
        ThreadState::WaitingForPermission => Icon::new(IconName::Warning)
            .size(IconSize::Small)
            .color(Color::Warning)
            .into_any_element(),
        ThreadState::Error => Icon::new(IconName::XCircle)
            .size(IconSize::Small)
            .color(Color::Error)
            .into_any_element(),
        ThreadState::Finished => Icon::new(IconName::Check)
            .size(IconSize::Small)
            .color(Color::Success)
            .into_any_element(),
        ThreadState::Idle => Icon::new(IconName::AiClaude)
            .size(IconSize::Small)
            .color(Color::Muted)
            .into_any_element(),
    }
}

fn node_status_color(status: NodeStatus) -> Color {
    match status {
        NodeStatus::Pending | NodeStatus::Running => Color::Accent,
        NodeStatus::WaitingForPermission => Color::Warning,
        NodeStatus::Completed => Color::Success,
        NodeStatus::Failed => Color::Error,
        NodeStatus::Canceled => Color::Muted,
    }
}

fn node_status_icon(status: NodeStatus, key: String) -> AnyElement {
    match status {
        NodeStatus::Pending | NodeStatus::Running => Icon::new(IconName::LoadCircle)
            .size(IconSize::XSmall)
            .color(Color::Accent)
            .with_keyed_rotate_animation(ElementId::Name(key.into()), 2)
            .into_any_element(),
        NodeStatus::WaitingForPermission => Icon::new(IconName::Warning)
            .size(IconSize::XSmall)
            .color(Color::Warning)
            .into_any_element(),
        NodeStatus::Completed => Icon::new(IconName::Check)
            .size(IconSize::XSmall)
            .color(Color::Success)
            .into_any_element(),
        NodeStatus::Failed => Icon::new(IconName::XCircle)
            .size(IconSize::XSmall)
            .color(Color::Error)
            .into_any_element(),
        NodeStatus::Canceled => Icon::new(IconName::Stop)
            .size(IconSize::XSmall)
            .color(Color::Muted)
            .into_any_element(),
    }
}

impl Render for AgentsDashboard {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.sync_log_previews(window, cx);
        let now = Instant::now();
        let show_inactive = self.show_inactive;
        let mut snapshots: Vec<ThreadSnapshot> = self
            .store
            .read(cx)
            .listed_snapshots()
            .filter(|snapshot| {
                show_inactive
                    || snapshot.state.is_active()
                    || snapshot
                        .background
                        .iter()
                        .any(|item| item.status_at(now).is_running())
            })
            .cloned()
            .collect();
        // Busy threads first, then most recently active.
        snapshots.sort_by_key(|snapshot| {
            (
                !snapshot.state.is_active(),
                std::cmp::Reverse(snapshot.turn_started_at),
            )
        });
        let refs: Vec<&ThreadSnapshot> = snapshots.iter().collect();
        let header = self.render_header(&refs, now, cx);
        let colors = Palette::new(cx);

        let body =
            if snapshots.is_empty() {
                v_flex()
                .size_full()
                .items_center()
                .justify_center()
                .gap_2()
                .child(Icon::new(IconName::UserGroup).size(IconSize::XLarge).color(Color::Muted))
                .child(Label::new("No Claude Code threads").color(Color::Muted))
                .child(
                    Label::new(
                        "Threads from every project tab and window show up here while they work.",
                    )
                    .size(LabelSize::Small)
                    .color(Color::Muted),
                )
                .into_any_element()
            } else {
                let mut list = v_flex()
                    .id("agents-dashboard-list")
                    .size_full()
                    .overflow_y_scroll()
                    .track_scroll(&self.scroll_handle)
                    .p_1();
                for snapshot in &snapshots {
                    list = list.child(self.render_thread(snapshot, now, cx));
                }
                list.into_any_element()
            };

        v_flex()
            .key_context("AgentsDashboard")
            .track_focus(&self.focus_handle)
            .size_full()
            .bg(colors.editor_background)
            .child(header)
            .child(body)
    }
}

impl Focusable for AgentsDashboard {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

pub enum DashboardEvent {}

impl EventEmitter<DashboardEvent> for AgentsDashboard {}

impl Item for AgentsDashboard {
    type Event = DashboardEvent;

    fn tab_content_text(&self, _detail: usize, _cx: &App) -> SharedString {
        "Agents".into()
    }

    fn tab_icon(&self, _window: &Window, _cx: &App) -> Option<Icon> {
        Some(Icon::new(IconName::UserGroup))
    }

    fn tab_tooltip_text(&self, _: &App) -> Option<SharedString> {
        Some("Every Claude Code thread, its sub-agents and background work".into())
    }

    fn to_item_events(_: &Self::Event, _: &mut dyn FnMut(ItemEvent)) {}

    fn tab_content(&self, params: TabContentParams, _window: &Window, cx: &App) -> AnyElement {
        let (agents, _) = self.store.read(cx).running_counts(Instant::now());
        h_flex()
            .gap_1()
            .child(Label::new("Agents").color(params.text_color()))
            .when(agents > 0, |this| {
                this.child(
                    Label::new(agents.to_string())
                        .size(LabelSize::XSmall)
                        .color(Color::Accent),
                )
            })
            .into_any_element()
    }
}

/// The theme colors the dashboard uses, copied out so rendering can keep
/// using `cx` mutably.
#[derive(Clone, Copy)]
pub(crate) struct Palette {
    pub(crate) border: gpui::Hsla,
    pub(crate) border_variant: gpui::Hsla,
    pub(crate) ghost_element_hover: gpui::Hsla,
    pub(crate) title_bar_background: gpui::Hsla,
    pub(crate) editor_background: gpui::Hsla,
}

impl Palette {
    pub(crate) fn new(cx: &App) -> Self {
        let colors = cx.theme().colors();
        Self {
            border: colors.border,
            border_variant: colors.border_variant,
            ghost_element_hover: colors.ghost_element_hover,
            title_bar_background: colors.title_bar_background,
            editor_background: colors.editor_background,
        }
    }
}
