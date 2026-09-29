//! Project tabs for OTerminal's title bar.
//!
//! Every project opened in a window is a tab. The tabs are a view over Zed's
//! [`MultiWorkspace`]: each project group owns a [`Workspace`] that stays alive
//! while another tab is shown, so its running terminals (e.g. Claude Code
//! sessions), language servers, unsaved buffers, pane layout and scroll
//! positions are exactly as they were when the user switches back. Tab order
//! and the active tab are persisted by the multi-workspace itself (the
//! `multi_workspace_state` key-value entry plus the workspace database), so
//! this crate holds no state of its own beyond subscriptions.

use std::path::PathBuf;

use collections::HashMap;
use gpui::{
    Action, AnyElement, App, ClickEvent, Context, Entity, EntityId, IntoElement, MouseButton,
    ParentElement, Render, SharedString, Styled, Subscription, TaskExt, WeakEntity, Window,
    actions, div, px,
};
use language::BufferEvent;
use project::buffer_store::BufferStoreEvent;
use terminal_view::{TerminalView, terminal_panel::TerminalPanel};
use ui::{ContextMenu, IconButtonShape, Indicator, Tooltip, prelude::*, right_click_menu};
use workspace::{MultiWorkspace, MultiWorkspaceEvent, ProjectGroupKey, Workspace};

actions!(
    project_tabs,
    [
        /// Closes the active project tab, prompting to save unsaved changes.
        CloseProjectTab,
    ]
);

pub fn init(cx: &mut App) {
    cx.observe_new(|workspace: &mut Workspace, _window, _cx| {
        workspace.register_action(|workspace, _: &CloseProjectTab, window, cx| {
            let Some(multi_workspace) = workspace
                .multi_workspace()
                .and_then(|multi_workspace| multi_workspace.upgrade())
            else {
                return;
            };
            let key = workspace.project_group_key(cx);
            if key.path_list().paths().is_empty() {
                return;
            }
            // The workspace is leased while its action handler runs, and
            // closing a project group reads every workspace of the window.
            window.defer(cx, move |window, cx| {
                close_project_group(&multi_workspace, &key, window, cx);
            });
        });
    })
    .detach();
}

fn close_project_group(
    multi_workspace: &Entity<MultiWorkspace>,
    key: &ProjectGroupKey,
    window: &mut Window,
    cx: &mut App,
) {
    multi_workspace
        .update(cx, |multi_workspace, cx| {
            multi_workspace.remove_project_group(key, window, cx)
        })
        .detach_and_log_err(cx);
}

/// Payload for dragging a project tab to reorder it.
#[derive(Clone)]
pub struct DraggedProjectTab {
    pub key: ProjectGroupKey,
    pub label: SharedString,
    pub index: usize,
}

impl Render for DraggedProjectTab {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = cx.theme().colors();
        h_flex()
            .h(px(28.))
            .px_3()
            .bg(colors.tab_active_background)
            .border_1()
            .border_color(colors.border)
            .text_color(colors.text)
            .child(Label::new(self.label.clone()).size(LabelSize::Small))
    }
}

/// The horizontal strip of project tabs rendered by the title bar.
pub struct ProjectTabs {
    multi_workspace: WeakEntity<MultiWorkspace>,
    /// Per held workspace: an observation of the workspace and of its
    /// buffer store, so tabs re-render when layout or dirty state changes.
    watched_workspaces: HashMap<EntityId, Vec<Subscription>>,
    watched_buffers: HashMap<EntityId, [Subscription; 2]>,
    _subscriptions: Vec<Subscription>,
}

impl ProjectTabs {
    pub fn new(
        multi_workspace: &Entity<MultiWorkspace>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let subscription = cx.subscribe_in(
            multi_workspace,
            window,
            |this, multi_workspace, event: &MultiWorkspaceEvent, _window, cx| {
                match event {
                    MultiWorkspaceEvent::ActiveWorkspaceChanged { .. }
                    | MultiWorkspaceEvent::WorkspaceAdded(_)
                    | MultiWorkspaceEvent::WorkspaceRemoved(_)
                    | MultiWorkspaceEvent::ProjectGroupsChanged => {
                        this.sync_watched_workspaces(multi_workspace, cx);
                    }
                }
                cx.notify();
            },
        );
        let observation = cx.observe(multi_workspace, |_, _, cx| cx.notify());

        // Upstream only pins the first workspace of a window once a second
        // project is opened (or the sidebar is shown). Pin it right away so the
        // single open project shows up as a tab and is persisted as one.
        let weak_multi_workspace = multi_workspace.downgrade();
        window.defer(cx, move |_window, cx| {
            weak_multi_workspace
                .update(cx, |multi_workspace, cx| {
                    if !multi_workspace.active_workspace_is_retained() {
                        multi_workspace.retain_active_workspace(cx);
                    }
                })
                .ok();
        });

        let mut this = Self {
            multi_workspace: multi_workspace.downgrade(),
            watched_workspaces: HashMap::default(),
            watched_buffers: HashMap::default(),
            _subscriptions: vec![subscription, observation],
        };
        this.sync_watched_workspaces(multi_workspace, cx);
        this
    }

    fn sync_watched_workspaces(
        &mut self,
        multi_workspace: &Entity<MultiWorkspace>,
        cx: &mut Context<Self>,
    ) {
        let workspaces: Vec<Entity<Workspace>> =
            multi_workspace.read(cx).workspaces().cloned().collect();
        self.watched_workspaces.retain(|id, _| {
            workspaces
                .iter()
                .any(|workspace| workspace.entity_id() == *id)
        });

        for workspace in workspaces {
            if self.watched_workspaces.contains_key(&workspace.entity_id()) {
                continue;
            }
            let buffer_store = workspace.read(cx).project().read(cx).buffer_store().clone();
            let subscriptions = vec![
                cx.observe(&workspace, |_, _, cx| cx.notify()),
                cx.subscribe(&buffer_store, |this, _, event: &BufferStoreEvent, cx| {
                    if let BufferStoreEvent::BufferAdded(buffer) = event {
                        this.watch_buffer(buffer, cx);
                    }
                }),
            ];
            let buffers: Vec<_> = buffer_store.read(cx).buffers().collect();
            for buffer in &buffers {
                self.watch_buffer(buffer, cx);
            }
            self.watched_workspaces
                .insert(workspace.entity_id(), subscriptions);
        }
    }

    fn watch_buffer(&mut self, buffer: &Entity<language::Buffer>, cx: &mut Context<Self>) {
        let buffer_id = buffer.entity_id();
        if self.watched_buffers.contains_key(&buffer_id) {
            return;
        }
        let events = cx.subscribe(buffer, |_, _, event: &BufferEvent, cx| {
            if matches!(
                event,
                BufferEvent::DirtyChanged | BufferEvent::Saved | BufferEvent::FileHandleChanged
            ) {
                cx.notify();
            }
        });
        let release = cx.observe_release(buffer, move |this, _, cx| {
            this.watched_buffers.remove(&buffer_id);
            cx.notify();
        });
        self.watched_buffers.insert(buffer_id, [events, release]);
    }

    fn activate(&self, key: &ProjectGroupKey, window: &mut Window, cx: &mut App) {
        let Some(multi_workspace) = self.multi_workspace.upgrade() else {
            return;
        };
        let activated = multi_workspace.update(cx, |multi_workspace, cx| {
            multi_workspace.activate_project_group(key, window, cx)
        });
        if activated {
            return;
        }
        // A remote project whose workspace is not loaded: reconnect through
        // the standard remote connection modal, then open it here.
        let Some(host) = key.host() else {
            return;
        };
        let modal_workspace = multi_workspace.read(cx).workspace().clone();
        let key = key.clone();
        let task = multi_workspace.update(cx, |multi_workspace, cx| {
            let connect_workspace = modal_workspace.clone();
            multi_workspace.find_or_create_workspace(
                key.path_list().clone(),
                Some(host),
                Some(key.clone()),
                move |options, window, cx| {
                    remote_connection::connect_with_modal(&connect_workspace, options, window, cx)
                },
                None,
                workspace::OpenMode::Activate,
                None,
                window,
                cx,
            )
        });
        window
            .spawn(cx, async move |cx| {
                let result = task.await;
                remote_connection::dismiss_connection_modal(&modal_workspace, cx);
                result.map(|_| ())
            })
            .detach_and_log_err(cx);
    }

    fn render_tab(
        &self,
        index: usize,
        tab: TabInfo,
        tab_count: usize,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let colors = cx.theme().colors();
        let group_name: SharedString = format!("project-tab-{index}").into();
        let TabInfo {
            key,
            label,
            tooltip,
            is_active,
            is_dirty,
            running_terminals,
            closable,
        } = tab;

        let close_button = closable.then(|| {
            let key = key.clone();
            IconButton::new(("close-project-tab", index), IconName::Close)
                .shape(IconButtonShape::Square)
                .size(ButtonSize::None)
                .icon_size(IconSize::XSmall)
                .icon_color(Color::Muted)
                .when(!is_active, |button| {
                    button.visible_on_hover(group_name.clone())
                })
                .tooltip(Tooltip::text("Close Project"))
                .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                    cx.stop_propagation();
                    if let Some(multi_workspace) = this.multi_workspace.upgrade() {
                        close_project_group(&multi_workspace, &key, window, cx);
                    }
                }))
        });

        let tab_element = h_flex()
            .id(("project-tab", index))
            .group(group_name.clone())
            .relative()
            .flex_none()
            .h_full()
            .px_3()
            .gap_1p5()
            .border_r_1()
            .border_color(colors.border)
            .cursor_pointer()
            .map(|tab| {
                if is_active {
                    tab.bg(colors.tab_active_background).text_color(colors.text)
                } else {
                    tab.text_color(colors.text_muted)
                        .hover(|style| style.bg(colors.ghost_element_hover))
                }
            })
            .when(is_dirty, |tab| {
                tab.child(Indicator::dot().color(Color::Modified))
            })
            .child(
                Label::new(label.clone())
                    .size(LabelSize::Small)
                    .color(if is_active {
                        Color::Default
                    } else {
                        Color::Muted
                    })
                    .single_line(),
            )
            .when(running_terminals > 0 && !is_active, |tab| {
                tab.child(
                    Icon::new(IconName::Terminal)
                        .size(IconSize::XSmall)
                        .color(Color::Muted),
                )
            })
            .children(close_button)
            .when(is_active, |tab| {
                tab.child(
                    div()
                        .absolute()
                        .left_0()
                        .right_0()
                        .bottom_0()
                        .h(px(2.))
                        .bg(colors.text),
                )
            })
            .tooltip(Tooltip::text(tooltip))
            .on_click(cx.listener({
                let key = key.clone();
                move |this, _: &ClickEvent, window, cx| {
                    this.activate(&key, window, cx);
                }
            }))
            .on_aux_click(cx.listener({
                let key = key.clone();
                move |this, event: &ClickEvent, window, cx| {
                    if !event.is_middle_click() || !closable {
                        return;
                    }
                    cx.stop_propagation();
                    if let Some(multi_workspace) = this.multi_workspace.upgrade() {
                        close_project_group(&multi_workspace, &key, window, cx);
                    }
                }
            }))
            // The title bar is a caption (drag) area on Windows: consume the
            // presses so Windows neither starts a window move on a tab click nor
            // opens the system menu on top of the tab's context menu.
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .on_mouse_up(MouseButton::Right, |_, _, cx| cx.stop_propagation())
            .when(closable, |tab| {
                tab.on_drag(
                    DraggedProjectTab {
                        key: key.clone(),
                        label: label.clone(),
                        index,
                    },
                    |dragged, _, _, cx| cx.new(|_| dragged.clone()),
                )
                .drag_over::<DraggedProjectTab>(move |style, dragged, _, cx| {
                    let colors = cx.theme().colors();
                    let style = style
                        .bg(colors.drop_target_background)
                        .border_color(colors.drop_target_border);
                    if index < dragged.index {
                        style.border_l_2()
                    } else if index > dragged.index {
                        style.border_r_2()
                    } else {
                        style
                    }
                })
                .on_drop(cx.listener(
                    move |this, dragged: &DraggedProjectTab, _window, cx| {
                        if let Some(multi_workspace) = this.multi_workspace.upgrade() {
                            multi_workspace.update(cx, |multi_workspace, cx| {
                                multi_workspace.move_project_group_to(&dragged.key, index, cx);
                            });
                        }
                    },
                ))
            });

        if !closable {
            return tab_element.into_any_element();
        }

        let multi_workspace = self.multi_workspace.clone();
        let paths: Vec<PathBuf> = key.path_list().ordered_paths().cloned().collect();
        let is_local = key.host().is_none();
        right_click_menu(("project-tab-menu", index))
            .trigger(move |_, _, _| tab_element)
            .menu(move |window, cx| {
                let multi_workspace = multi_workspace.clone();
                let key = key.clone();
                let paths = paths.clone();
                ContextMenu::build(window, cx, move |menu, _, _| {
                    let close = {
                        let multi_workspace = multi_workspace.clone();
                        let key = key.clone();
                        move |window: &mut Window, cx: &mut App| {
                            if let Some(multi_workspace) = multi_workspace.upgrade() {
                                close_project_group(&multi_workspace, &key, window, cx);
                            }
                        }
                    };
                    let close_others = {
                        let multi_workspace = multi_workspace.clone();
                        let key = key.clone();
                        move |window: &mut Window, cx: &mut App| {
                            close_other_project_groups(multi_workspace.clone(), &key, window, cx);
                        }
                    };
                    let move_to_new_window = {
                        let multi_workspace = multi_workspace.clone();
                        let key = key.clone();
                        move |window: &mut Window, cx: &mut App| {
                            if let Some(multi_workspace) = multi_workspace.upgrade() {
                                multi_workspace
                                    .update(cx, |multi_workspace, cx| {
                                        multi_workspace
                                            .open_project_group_in_new_window(&key, window, cx)
                                    })
                                    .detach_and_log_err(cx);
                            }
                        }
                    };
                    let reveal_path = paths.first().cloned();

                    menu.entry("Close", Some(CloseProjectTab.boxed_clone()), close)
                        .when(tab_count > 1, |menu| {
                            menu.entry("Close Others", None, close_others).entry(
                                "Move to New Window",
                                None,
                                move_to_new_window,
                            )
                        })
                        .when_some(reveal_path.filter(|_| is_local), |menu, path| {
                            menu.separator()
                                .entry("Reveal in File Manager", None, move |_, cx| {
                                    cx.reveal_path(&path);
                                })
                        })
                })
            })
            .into_any_element()
    }
}

fn close_other_project_groups(
    multi_workspace: WeakEntity<MultiWorkspace>,
    keep: &ProjectGroupKey,
    window: &mut Window,
    cx: &mut App,
) {
    let Some(entity) = multi_workspace.upgrade() else {
        return;
    };
    let others: Vec<ProjectGroupKey> = entity
        .read(cx)
        .project_group_keys()
        .into_iter()
        .filter(|key| key != keep)
        .collect();
    let keep = keep.clone();
    window
        .spawn(cx, async move |cx| {
            // Show the kept project first so no other tab flashes into view
            // while the rest are closed one by one (each may prompt to save).
            multi_workspace.update_in(cx, |multi_workspace, window, cx| {
                multi_workspace.activate_project_group(&keep, window, cx);
            })?;
            for key in others {
                let task = multi_workspace.update_in(cx, |multi_workspace, window, cx| {
                    multi_workspace.remove_project_group(&key, window, cx)
                })?;
                if !task.await? {
                    break;
                }
            }
            anyhow::Ok(())
        })
        .detach_and_log_err(cx);
}

struct TabInfo {
    key: ProjectGroupKey,
    label: SharedString,
    tooltip: SharedString,
    is_active: bool,
    is_dirty: bool,
    running_terminals: usize,
    closable: bool,
}

fn tab_label(key: &ProjectGroupKey) -> SharedString {
    let name = key
        .path_list()
        .ordered_paths()
        .next()
        .map(|path| {
            path.file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_else(|| path.to_string_lossy().into_owned())
        })
        .unwrap_or_else(|| "Empty".to_string());
    let extra_roots = key.path_list().paths().len().saturating_sub(1);
    let name = if extra_roots > 0 {
        format!("{name} +{extra_roots}")
    } else {
        name
    };
    match key.host() {
        Some(host) => format!("{}: {name}", host.display_name()).into(),
        None => name.into(),
    }
}

fn tab_tooltip(key: &ProjectGroupKey, running_terminals: usize) -> SharedString {
    let mut tooltip = key
        .path_list()
        .ordered_paths()
        .map(|path| path.to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join("\n");
    if tooltip.is_empty() {
        tooltip = "Empty project".to_string();
    }
    if let Some(host) = key.host() {
        tooltip = format!("{}\n{tooltip}", host.display_name());
    }
    match running_terminals {
        0 => {}
        1 => tooltip.push_str("\n1 terminal running"),
        count => tooltip.push_str(&format!("\n{count} terminals running")),
    }
    tooltip.into()
}

fn running_terminal_count(workspace: &Workspace, cx: &App) -> usize {
    let in_panel = workspace
        .panel::<TerminalPanel>(cx)
        .map(|panel| {
            panel
                .read(cx)
                .panes()
                .into_iter()
                .map(|pane| pane.read(cx).items_len())
                .sum::<usize>()
        })
        .unwrap_or(0);
    in_panel + workspace.items_of_type::<TerminalView>(cx).count()
}

impl Render for ProjectTabs {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let Some(multi_workspace) = self.multi_workspace.upgrade() else {
            return h_flex().id("project-tabs").h_full();
        };

        let tabs: Vec<TabInfo> = {
            let multi_workspace = multi_workspace.read(cx);
            let active_key =
                multi_workspace.project_group_key_for_workspace(multi_workspace.workspace(), cx);
            let groups = multi_workspace.project_groups(cx);
            let active_is_listed = groups.iter().any(|group| group.key == active_key);

            let mut tabs: Vec<TabInfo> = groups
                .into_iter()
                .map(|group| {
                    let is_active = group.key == active_key;
                    let is_dirty = group.workspaces.iter().any(|workspace| {
                        workspace.read(cx).items(cx).any(|item| item.is_dirty(cx))
                    });
                    let running_terminals = group
                        .workspaces
                        .iter()
                        .map(|workspace| running_terminal_count(workspace.read(cx), cx))
                        .sum();
                    TabInfo {
                        label: tab_label(&group.key),
                        tooltip: tab_tooltip(&group.key, running_terminals),
                        key: group.key,
                        is_active,
                        is_dirty,
                        running_terminals,
                        closable: true,
                    }
                })
                .collect();

            // The displayed workspace has no project group while it has no
            // folder open (a fresh window) or until it is pinned; still show
            // it as the active tab.
            if !active_is_listed {
                let workspace = multi_workspace.workspace().read(cx);
                let is_dirty = workspace.items(cx).any(|item| item.is_dirty(cx));
                let running_terminals = running_terminal_count(workspace, cx);
                tabs.push(TabInfo {
                    label: tab_label(&active_key),
                    tooltip: tab_tooltip(&active_key, running_terminals),
                    key: active_key,
                    is_active: true,
                    is_dirty,
                    running_terminals,
                    closable: false,
                });
            }
            tabs
        };

        let tab_count = tabs.len();
        let tab_elements: Vec<AnyElement> = tabs
            .into_iter()
            .enumerate()
            .map(|(index, tab)| self.render_tab(index, tab, tab_count, cx))
            .collect();

        h_flex()
            .id("project-tabs")
            .h_full()
            .min_w_0()
            .border_l_1()
            .border_color(cx.theme().colors().border)
            .child(
                h_flex()
                    .id("project-tabs-scroll")
                    .h_full()
                    .min_w_0()
                    .overflow_x_scroll()
                    .children(tab_elements),
            )
            .child(
                div().flex_none().px_1().child(
                    IconButton::new("project-tabs-open", IconName::Plus)
                        .icon_size(IconSize::Small)
                        .icon_color(Color::Muted)
                        .tooltip(Tooltip::text("Open Project…"))
                        .on_click(|_, window, cx| {
                            window.dispatch_action(
                                workspace::Open {
                                    create_new_window: Some(false),
                                }
                                .boxed_clone(),
                                cx,
                            );
                        }),
                ),
            )
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use util::path_list::PathList;

    #[test]
    fn test_tab_label() {
        assert_eq!(tab_label(&ProjectGroupKey::default()).as_ref(), "Empty");

        let root = if cfg!(windows) {
            PathBuf::from(r"C:\code\othcloud")
        } else {
            PathBuf::from("/code/othcloud")
        };
        let key = ProjectGroupKey::new(None, PathList::new(&[root.clone()]));
        assert_eq!(tab_label(&key).as_ref(), "othcloud");
        assert!(tab_tooltip(&key, 2).contains("2 terminals running"));

        let other = root.with_file_name("oterminal");
        let key = ProjectGroupKey::new(None, PathList::new(&[root, other]));
        assert!(tab_label(&key).ends_with("+1"));
    }
}
