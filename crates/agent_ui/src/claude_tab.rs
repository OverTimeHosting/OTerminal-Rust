//! OTerminal: Claude Code threads as workspace items (center pane tabs).
//!
//! Each [`ClaudeTab`] wraps its own [`ConversationView`], i.e. its own
//! Claude Code ACP session, so several can be open at once, split, dragged
//! between panes and moved to another window. Tabs are serialized by
//! [`ThreadId`] and resume the same session on restart.

use std::path::PathBuf;

use acp_thread::ThreadStatus;
use anyhow::{Context as _, Result};
use db::kvp::KeyValueStore;
use gpui::{
    Action, AnyElement, App, Context, Entity, EventEmitter, FocusHandle, Focusable, IntoElement,
    ParentElement, Render, SharedString, Styled, Subscription, Task, TaskExt, WeakEntity, Window,
    div,
};
use project::Project;
use terminal_view::terminal_panel::TerminalPanel;
use ui::{Icon, IconName, prelude::*};
use util::ResultExt as _;
use workspace::{
    Item, ItemId, OpenMode, OpenOptions, SerializableItem, Toast, Workspace, WorkspaceId,
    WorkspaceMatching,
    item::{ItemEvent, TabContentParams},
    notifications::NotificationId,
};

use crate::thread_metadata_store::{ThreadId, ThreadMetadataStore};
use crate::{
    Agent, AgentConnectionStore, AgentInitialContent, AgentPanel, AgentThreadSource,
    ConversationView, NewClaudeTab, OpenClaudeTabInNewWindow, StartClaudeRemoteControl,
};

const CLAUDE_TABS_NAMESPACE: &str = "claude_code_tabs";
const MAX_TAB_TITLE_CHARS: usize = 40;
const FALLBACK_TAB_TITLE: &str = "Claude Code";

pub fn init(cx: &mut App) {
    workspace::register_serializable_item::<ClaudeTab>(cx);

    cx.observe_new(|workspace: &mut Workspace, _window, _cx| {
        workspace
            .register_action(|workspace, _: &NewClaudeTab, window, cx| {
                ClaudeTab::open_new(workspace, window, cx);
            })
            .register_action(|workspace, _: &OpenClaudeTabInNewWindow, window, cx| {
                ClaudeTab::move_active_tab_to_new_window(workspace, window, cx);
            })
            .register_action(|workspace, _: &StartClaudeRemoteControl, window, cx| {
                let resume_session_id = remote_control_session_id(workspace, window, cx);
                if resume_session_id.is_some() {
                    workspace.show_toast(
                        Toast::new(
                            NotificationId::unique::<StartClaudeRemoteControl>(),
                            "This thread continues in the Remote Control terminal. \
                             Carry on there rather than in the chat while it is open.",
                        )
                        .autohide(),
                        cx,
                    );
                }
                TerminalPanel::new_claude_remote_control_terminal(
                    workspace,
                    resume_session_id,
                    window,
                    cx,
                );
            });
    })
    .detach();
}

/// The Claude Code session Remote Control should continue: the thread of the
/// focused Claude Code tab, else the agent panel's thread. `None` for a thread
/// without messages, which has no session to resume yet.
fn remote_control_session_id(workspace: &Workspace, window: &Window, cx: &App) -> Option<String> {
    let focused_tab = workspace
        .active_item_as::<ClaudeTab>(cx)
        .filter(|tab| tab.focus_handle(cx).contains_focused(window, cx));
    let conversation_view = match focused_tab {
        Some(tab) => tab.read(cx).conversation_view.clone(),
        None => workspace
            .panel::<AgentPanel>(cx)?
            .read(cx)
            .active_conversation_view()?
            .clone(),
    };
    let thread = conversation_view.read(cx).root_thread(cx)?;
    let thread = thread.read(cx);
    (!thread.entries().is_empty()).then(|| thread.session_id().0.to_string())
}

#[derive(Default)]
struct OpenClaudeTabs(Vec<WeakEntity<ClaudeTab>>);

impl gpui::Global for OpenClaudeTabs {}

pub enum ClaudeTabEvent {
    UpdateTab,
}

pub struct ClaudeTab {
    conversation_view: Entity<ConversationView>,
    project: Entity<Project>,
    _subscriptions: Vec<Subscription>,
}

impl ClaudeTab {
    pub(crate) fn new(
        conversation_view: Entity<ConversationView>,
        project: Entity<Project>,
        cx: &mut Context<Self>,
    ) -> Self {
        let subscriptions = vec![cx.observe(&conversation_view, |_, _, cx| {
            cx.emit(ClaudeTabEvent::UpdateTab);
            cx.notify();
        })];
        let this = cx.weak_entity();
        let open_tabs = &mut cx.default_global::<OpenClaudeTabs>().0;
        open_tabs.retain(|tab| tab.upgrade().is_some());
        open_tabs.push(this);
        Self {
            conversation_view,
            project,
            _subscriptions: subscriptions,
        }
    }

    /// The open tab for `thread_id` in `project`, if any. Uses a registry
    /// rather than the workspace so it can be called while the workspace is
    /// being updated.
    pub(crate) fn find(
        project: &Entity<Project>,
        thread_id: ThreadId,
        cx: &App,
    ) -> Option<Entity<Self>> {
        cx.try_global::<OpenClaudeTabs>()?
            .0
            .iter()
            .filter_map(|tab| tab.upgrade())
            .find(|tab| {
                let tab = tab.read(cx);
                tab.project == *project && tab.thread_id(cx) == thread_id
            })
    }

    /// Conversations open as Claude Code tabs of `project`.
    pub(crate) fn open_conversation_views(
        project: &Entity<Project>,
        cx: &App,
    ) -> Vec<Entity<ConversationView>> {
        cx.try_global::<OpenClaudeTabs>()
            .map(|tabs| {
                tabs.0
                    .iter()
                    .filter_map(|tab| tab.upgrade())
                    .filter(|tab| tab.read(cx).project == *project)
                    .map(|tab| tab.read(cx).conversation_view.clone())
                    .collect()
            })
            .unwrap_or_default()
    }

    pub fn thread_id(&self, cx: &App) -> ThreadId {
        self.conversation_view.read(cx).thread_id
    }

    pub fn conversation_view(&self) -> &Entity<ConversationView> {
        &self.conversation_view
    }

    fn is_generating(&self, cx: &App) -> bool {
        self.conversation_view
            .read(cx)
            .root_thread(cx)
            .is_some_and(|thread| thread.read(cx).status() == ThreadStatus::Generating)
    }

    fn project_name(&self, cx: &App) -> Option<SharedString> {
        let worktree = self.project.read(cx).visible_worktrees(cx).next()?;
        let name = worktree.read(cx).root_name_str().to_string();
        (!name.is_empty()).then(|| name.into())
    }

    /// The thread's title (Claude Code's, the auto title, or the user's
    /// rename), "Claude Code" until one is known, or the loading / error
    /// status while the session isn't connected.
    fn full_title(&self, cx: &App) -> SharedString {
        let view = self.conversation_view.read(cx);
        if let Some(title) = view.thread_title(cx) {
            return title;
        }
        let status = view.title(cx);
        if status.as_ref() == crate::DEFAULT_THREAD_TITLE {
            FALLBACK_TAB_TITLE.into()
        } else {
            status
        }
    }

    /// Opens a new Claude Code thread in a tab in the active pane.
    pub fn open_new(workspace: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
        let tab = Self::create(workspace, None, window, cx);
        workspace.add_item_to_active_pane(Box::new(tab), None, true, window, cx);
    }

    /// Wraps an existing conversation (e.g. moved out of the agent panel)
    /// in a tab in the active pane.
    pub(crate) fn open_conversation(
        workspace: &mut Workspace,
        conversation_view: Entity<ConversationView>,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) {
        let project = workspace.project().clone();
        let tab = cx.new(|cx| Self::new(conversation_view, project, cx));
        workspace.add_item_to_active_pane(Box::new(tab), None, true, window, cx);
    }

    /// Creates a tab for `thread_id` (resuming its session when it has one),
    /// or for a fresh Claude Code thread when `thread_id` is `None`.
    fn create(
        workspace: &Workspace,
        thread_id: Option<ThreadId>,
        window: &mut Window,
        cx: &mut App,
    ) -> Entity<Self> {
        let project = workspace.project().clone();
        let connection_store = workspace
            .panel::<AgentPanel>(cx)
            .map(|panel| panel.read(cx).connection_store().clone());
        let conversation_view = Self::create_conversation_view(
            workspace.weak_handle(),
            project.clone(),
            connection_store,
            thread_id,
            window,
            cx,
        );
        cx.new(|cx| Self::new(conversation_view, project, cx))
    }

    fn create_conversation_view(
        workspace: WeakEntity<Workspace>,
        project: Entity<Project>,
        connection_store: Option<Entity<AgentConnectionStore>>,
        thread_id: Option<ThreadId>,
        window: &mut Window,
        cx: &mut App,
    ) -> Entity<ConversationView> {
        let metadata = thread_id.and_then(|thread_id| {
            ThreadMetadataStore::try_global(cx)
                .and_then(|store| store.read(cx).entry(thread_id).cloned())
        });

        let agent = crate::resolve_agent(
            metadata
                .as_ref()
                .map(|metadata| Agent::from(metadata.agent_id.clone()))
                .unwrap_or_else(crate::claude_code_agent),
            cx,
        );
        let resume_session_id = metadata
            .as_ref()
            .and_then(|metadata| metadata.session_id.clone());
        let work_dirs = metadata
            .as_ref()
            .map(|metadata| metadata.folder_paths().clone())
            .filter(|paths| !paths.is_empty());
        let title = metadata.as_ref().and_then(|metadata| metadata.title());
        let initial_content = thread_id
            .filter(|_| metadata.as_ref().is_none_or(|metadata| metadata.is_draft()))
            .and_then(|thread_id| crate::draft_prompt_store::read(thread_id, cx))
            .map(|blocks| AgentInitialContent::ContentBlock {
                blocks,
                auto_submit: false,
            });

        let fs = workspace::AppState::global(cx).fs.clone();
        let thread_store = agent::ThreadStore::global(cx);
        let server = agent.server(fs, thread_store.clone());
        let thread_store = matches!(agent, Agent::NativeAgent).then_some(thread_store);
        let connection_store = connection_store
            .unwrap_or_else(|| cx.new(|cx| AgentConnectionStore::new(project.clone(), cx)));

        cx.new(|cx| {
            ConversationView::new(
                server,
                connection_store,
                agent,
                resume_session_id,
                Some(thread_id.unwrap_or_else(ThreadId::new)),
                work_dirs,
                title,
                initial_content,
                workspace,
                project,
                thread_store,
                AgentThreadSource::AgentPanel,
                window,
                cx,
            )
        })
    }

    /// Closes the active Claude Code tab and reopens its thread in a new
    /// window for the same project.
    fn move_active_tab_to_new_window(
        workspace: &mut Workspace,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) {
        let Some(tab) = workspace.active_item_as::<ClaudeTab>(cx) else {
            return;
        };
        let paths: Vec<PathBuf> = workspace
            .project()
            .read(cx)
            .visible_worktrees(cx)
            .map(|worktree| worktree.read(cx).abs_path().to_path_buf())
            .collect();
        if paths.is_empty() {
            return;
        }
        let thread_id = tab.read(cx).thread_id(cx);

        // Drop this window's view of the session before the new window
        // resumes it.
        if let Some(pane) = workspace.pane_for(&tab) {
            pane.update(cx, |pane, cx| {
                pane.remove_item(tab.entity_id(), false, true, window, cx);
            });
        }
        drop(tab);

        let open_task = workspace::open_paths(
            &paths,
            workspace.app_state().clone(),
            OpenOptions {
                workspace_matching: WorkspaceMatching::None,
                open_mode: OpenMode::NewWindow,
                ..OpenOptions::default()
            },
            cx,
        );
        cx.spawn(async move |_, cx| {
            let result = open_task.await?;
            let workspace = result.workspace.clone();
            result.window.update(cx, |_, window, cx| {
                workspace.update(cx, |workspace, cx| {
                    let tab = Self::create(workspace, Some(thread_id), window, cx);
                    workspace.add_item_to_active_pane(Box::new(tab), None, true, window, cx);
                });
            })?;
            anyhow::Ok(())
        })
        .detach_and_log_err(cx);
    }

    fn serialization_key(workspace_id: WorkspaceId, item_id: ItemId) -> String {
        format!("{}:{item_id}", i64::from(workspace_id))
    }
}

impl EventEmitter<ClaudeTabEvent> for ClaudeTab {}

impl Focusable for ClaudeTab {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.conversation_view.focus_handle(cx)
    }
}

impl Render for ClaudeTab {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .size_full()
            .bg(cx.theme().colors().panel_background)
            .child(self.conversation_view.clone())
    }
}

impl Item for ClaudeTab {
    type Event = ClaudeTabEvent;

    fn tab_content(&self, params: TabContentParams, _window: &Window, cx: &App) -> AnyElement {
        let label = Label::new(self.tab_content_text(params.detail.unwrap_or_default(), cx))
            .single_line()
            .color(params.text_color());
        // OTerminal: a dot while Claude finished or needs input unseen.
        if self.conversation_view.read(cx).has_unseen_activity() {
            h_flex()
                .gap_1()
                .child(label)
                .child(ui::Indicator::dot().color(Color::Accent))
                .into_any_element()
        } else {
            label.into_any_element()
        }
    }

    fn tab_content_text(&self, _detail: usize, cx: &App) -> SharedString {
        util::truncate_and_trailoff(&self.full_title(cx), MAX_TAB_TITLE_CHARS).into()
    }

    fn tab_icon(&self, _window: &Window, _cx: &App) -> Option<Icon> {
        Some(Icon::new(IconName::AiClaude))
    }

    fn tab_tooltip_text(&self, cx: &App) -> Option<SharedString> {
        let title = self.full_title(cx);
        let title = match self.project_name(cx) {
            Some(project_name) => format!("{project_name} \u{b7} {title}"),
            None => title.to_string(),
        };
        Some(if self.is_generating(cx) {
            format!("{title} (Claude is working\u{2026})").into()
        } else {
            title.into()
        })
    }

    fn to_item_events(event: &Self::Event, f: &mut dyn FnMut(ItemEvent)) {
        match event {
            ClaudeTabEvent::UpdateTab => f(ItemEvent::UpdateTab),
        }
    }

    /// Shows the tab's modified indicator while Claude is generating.
    fn is_dirty(&self, cx: &App) -> bool {
        self.is_generating(cx)
    }

    fn include_in_nav_history() -> bool {
        false
    }

    fn tab_extra_context_menu_actions(
        &self,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Vec<(SharedString, Box<dyn Action>)> {
        vec![(
            "Move to New Window".into(),
            Box::new(OpenClaudeTabInNewWindow),
        )]
    }
}

impl SerializableItem for ClaudeTab {
    fn serialized_item_kind() -> &'static str {
        "ClaudeCodeTab"
    }

    fn cleanup(
        workspace_id: WorkspaceId,
        alive_items: Vec<ItemId>,
        _window: &mut Window,
        cx: &mut App,
    ) -> Task<Result<()>> {
        let kvp = KeyValueStore::global(cx);
        cx.background_spawn(async move {
            let prefix = format!("{}:", i64::from(workspace_id));
            let keys = kvp.select_bound::<&str, String>(
                "SELECT key FROM scoped_kv_store WHERE namespace = (?)",
            )?(CLAUDE_TABS_NAMESPACE)?;
            let alive: Vec<String> = alive_items
                .into_iter()
                .map(|item_id| ClaudeTab::serialization_key(workspace_id, item_id))
                .collect();
            let scope = kvp.scoped(CLAUDE_TABS_NAMESPACE);
            for key in keys {
                if key.starts_with(&prefix) && !alive.contains(&key) {
                    scope.delete(key).await.log_err();
                }
            }
            Ok(())
        })
    }

    fn deserialize(
        project: Entity<Project>,
        workspace: WeakEntity<Workspace>,
        workspace_id: WorkspaceId,
        item_id: ItemId,
        window: &mut Window,
        cx: &mut App,
    ) -> Task<Result<Entity<Self>>> {
        let kvp = KeyValueStore::global(cx);
        let thread_id = kvp
            .scoped(CLAUDE_TABS_NAMESPACE)
            .read(&Self::serialization_key(workspace_id, item_id))
            .ok()
            .flatten()
            .and_then(|json| serde_json::from_str::<ThreadId>(&json).log_err());
        let Some(thread_id) = thread_id else {
            return Task::ready(Err(anyhow::anyhow!("no Claude Code tab to deserialize")));
        };
        let metadata_loaded =
            ThreadMetadataStore::try_global(cx).map(|store| store.read(cx).reload_task());
        let window_handle = window.window_handle();

        cx.spawn(async move |cx| {
            if let Some(metadata_loaded) = metadata_loaded {
                metadata_loaded.await;
            }
            cx.update_window(window_handle, |_, window, cx| {
                let connection_store = workspace
                    .upgrade()
                    .and_then(|workspace| workspace.read(cx).panel::<AgentPanel>(cx))
                    .map(|panel| panel.read(cx).connection_store().clone());
                let conversation_view = Self::create_conversation_view(
                    workspace,
                    project.clone(),
                    connection_store,
                    Some(thread_id),
                    window,
                    cx,
                );
                cx.new(|cx| Self::new(conversation_view, project, cx))
            })
            .context("restoring Claude Code tab")
        })
    }

    fn serialize(
        &mut self,
        workspace: &mut Workspace,
        item_id: ItemId,
        _closing: bool,
        cx: &mut Context<Self>,
    ) -> Option<Task<Result<()>>> {
        let workspace_id = workspace.database_id()?;
        let key = Self::serialization_key(workspace_id, item_id);
        let value = serde_json::to_string(&self.thread_id(cx)).ok()?;
        let kvp = KeyValueStore::global(cx);
        Some(cx.background_spawn(async move {
            kvp.scoped(CLAUDE_TABS_NAMESPACE).write(key, value).await
        }))
    }

    fn should_serialize(&self, _event: &Self::Event) -> bool {
        false
    }
}
