//! The app-wide registry of Claude Code threads the dashboard shows.
//!
//! Every [`AcpThread`] is picked up when it is created (`observe_new`), so
//! threads in the agent panel, in Claude tabs, in every project tab and every
//! window are covered without asking `agent_ui` for them. The store listens
//! to each thread's events and rebuilds that thread's snapshot at most every
//! [`REFRESH_DEBOUNCE`], so streaming tokens don't cause a rebuild each.

use std::io::{Read as _, Seek as _, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

use acp_thread::{
    AcpThread, AcpThreadEvent, AgentThreadEntry, ThreadStatus, ToolCall, ToolCallContent,
    ToolCallStatus,
};
use agent_ui::{AgentPanel, ConversationView, ThreadId, claude_tab::ClaudeTab};
use collections::{HashMap, HashSet};
use gpui::{
    App, AppContext as _, Context, Entity, EntityId, EventEmitter, Focusable as _, Global,
    ListOffset, SharedString, Subscription, Task, WeakEntity, WindowHandle, px,
};
use util::ResultExt as _;
use workspace::{MultiWorkspace, Workspace};

use crate::model::{
    self, AgentNode, BackgroundItem, BackgroundTaskRef, CallRecord, CallRole, CallTiming,
    NodeStatus, OutputTail, TerminalInfo, ThreadState, TreeContext,
};

pub const REFRESH_DEBOUNCE: Duration = Duration::from_millis(200);
const TAIL_INTERVAL: Duration = Duration::from_secs(3);
const TAIL_BYTES: u64 = 64 * 1024;

struct GlobalAgentsStore(Entity<AgentsStore>);

impl Global for GlobalAgentsStore {}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LocationKind {
    /// The agent panel of a project.
    Panel,
    /// A Claude Code tab in a pane.
    Tab,
}

#[derive(Clone)]
pub struct ThreadSnapshot {
    pub thread: WeakEntity<AcpThread>,
    pub entity_id: EntityId,
    pub title: SharedString,
    pub project: Option<SharedString>,
    pub location: Option<LocationKind>,
    pub state: ThreadState,
    pub turn_started_at: Option<Instant>,
    pub turn_ended_at: Option<Instant>,
    /// `(used, max)` context tokens.
    pub tokens: Option<(u64, u64)>,
    pub cost: Option<(f64, SharedString)>,
    pub tool_calls: usize,
    pub edits: usize,
    pub root: AgentNode,
    pub background: Vec<BackgroundItem>,
    pub has_entries: bool,
}

impl ThreadSnapshot {
    /// Whether the dashboard lists it: shown somewhere, or doing something.
    pub fn is_listed(&self) -> bool {
        self.has_entries
            && (self.location.is_some()
                || self.state.is_active()
                || self.background.iter().any(|item| item.status.is_running()))
    }
}

struct TrackedThread {
    thread: WeakEntity<AcpThread>,
    entity_id: EntityId,
    call_timings: HashMap<SharedString, CallTiming>,
    node_timings: HashMap<SharedString, CallTiming>,
    background_refs: HashMap<SharedString, Option<BackgroundTaskRef>>,
    was_generating: bool,
    turn_started_at: Option<Instant>,
    turn_ended_at: Option<Instant>,
    errored: bool,
    finished_a_turn: bool,
    dirty: bool,
    snapshot: Option<ThreadSnapshot>,
    _subscriptions: [Subscription; 2],
}

pub struct AgentsStore {
    threads: Vec<TrackedThread>,
    output_tails: HashMap<PathBuf, OutputTail>,
    /// Output files with an open log preview, tailed even after their
    /// command stopped.
    previewed_outputs: HashSet<PathBuf>,
    viewers: usize,
    refresh_task: Option<Task<()>>,
    tail_task: Option<Task<()>>,
}

/// Emitted after snapshots changed.
pub struct SnapshotsUpdated;

impl EventEmitter<SnapshotsUpdated> for AgentsStore {}

pub fn init(cx: &mut App) {
    if cx.has_global::<GlobalAgentsStore>() {
        return;
    }
    let store = cx.new(|_| AgentsStore::new());
    cx.set_global(GlobalAgentsStore(store.clone()));
    cx.observe_new(move |_: &mut AcpThread, _, cx: &mut Context<AcpThread>| {
        let thread = cx.entity();
        let store = store.clone();
        cx.defer(move |cx| {
            store.update(cx, |store, cx| store.track(&thread, cx));
        });
    })
    .detach();
}

impl AgentsStore {
    fn new() -> Self {
        Self {
            threads: Vec::new(),
            output_tails: HashMap::default(),
            previewed_outputs: HashSet::default(),
            viewers: 0,
            refresh_task: None,
            tail_task: None,
        }
    }

    pub fn global(cx: &App) -> Option<Entity<Self>> {
        cx.try_global::<GlobalAgentsStore>()
            .map(|global| global.0.clone())
    }

    /// Starts following a thread (also used by tests for threads created
    /// before `init`).
    pub fn track(&mut self, thread: &Entity<AcpThread>, cx: &mut Context<Self>) {
        let entity_id = thread.entity_id();
        if self
            .threads
            .iter()
            .any(|tracked| tracked.entity_id == entity_id)
        {
            return;
        }
        // Sub-agent threads of Zed's native agent live inside their parent's
        // view; Claude Code's sub-agents are tool calls of the root thread.
        if thread.read(cx).parent_session_id().is_some() {
            return;
        }
        let subscriptions = [
            cx.subscribe(thread, Self::handle_thread_event),
            cx.observe_release(thread, |this, _, cx| this.schedule_refresh(cx)),
        ];
        self.threads.push(TrackedThread {
            thread: thread.downgrade(),
            entity_id,
            call_timings: HashMap::default(),
            node_timings: HashMap::default(),
            background_refs: HashMap::default(),
            was_generating: false,
            turn_started_at: None,
            turn_ended_at: None,
            errored: thread.read(cx).had_error(),
            finished_a_turn: false,
            dirty: true,
            snapshot: None,
            _subscriptions: subscriptions,
        });
        self.schedule_refresh(cx);
    }

    fn handle_thread_event(
        &mut self,
        thread: Entity<AcpThread>,
        event: &AcpThreadEvent,
        cx: &mut Context<Self>,
    ) {
        let Some(tracked) = self
            .threads
            .iter_mut()
            .find(|tracked| tracked.entity_id == thread.entity_id())
        else {
            return;
        };
        match event {
            AcpThreadEvent::Error | AcpThreadEvent::LoadError(_) => tracked.errored = true,
            AcpThreadEvent::Stopped(_) => tracked.finished_a_turn = true,
            // Pure UI state; nothing we show changes.
            AcpThreadEvent::PromptUpdated
            | AcpThreadEvent::PromptCapabilitiesUpdated
            | AcpThreadEvent::AvailableCommandsUpdated(_)
            | AcpThreadEvent::ModeUpdated(_)
            | AcpThreadEvent::ConfigOptionsUpdated(_) => return,
            _ => {}
        }
        tracked.dirty = true;
        self.schedule_refresh(cx);
    }

    pub fn schedule_refresh(&mut self, cx: &mut Context<Self>) {
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

    /// Rebuilds the snapshots of changed threads (and every thread's
    /// location, which changes without thread events).
    pub fn refresh(&mut self, cx: &mut Context<Self>) {
        self.threads
            .retain(|tracked| tracked.thread.upgrade().is_some());
        let now = Instant::now();
        let locations = resolve_locations(cx);
        let tails = &self.output_tails;
        for tracked in &mut self.threads {
            let Some(thread) = tracked.thread.upgrade() else {
                continue;
            };
            let location = locations.get(&tracked.entity_id);
            if tracked.dirty || tracked.snapshot.is_none() {
                tracked.dirty = false;
                let snapshot = build_snapshot(tracked, &thread, location, tails, now, cx);
                tracked.snapshot = Some(snapshot);
            } else if let Some(snapshot) = tracked.snapshot.as_mut() {
                snapshot.location = location.map(|location| location.kind);
                if let Some(title) = location.and_then(|location| location.title.clone()) {
                    snapshot.title = title;
                }
            }
        }
        self.update_tail_task(cx);
        cx.emit(SnapshotsUpdated);
        cx.notify();
    }

    /// Marks every thread for a rebuild (e.g. for time-based states).
    pub fn invalidate(&mut self, cx: &mut Context<Self>) {
        for tracked in &mut self.threads {
            tracked.dirty = true;
        }
        self.schedule_refresh(cx);
    }

    pub fn snapshots(&self) -> impl Iterator<Item = &ThreadSnapshot> {
        self.threads
            .iter()
            .filter_map(|tracked| tracked.snapshot.as_ref())
    }

    pub fn listed_snapshots(&self) -> impl Iterator<Item = &ThreadSnapshot> {
        self.snapshots().filter(|snapshot| snapshot.is_listed())
    }

    /// `(running agents, running background items)`.
    pub fn running_counts(&self, now: Instant) -> (usize, usize) {
        model::running_counts(
            self.listed_snapshots().map(|snapshot| {
                (
                    &snapshot.state,
                    &snapshot.root,
                    snapshot.background.as_slice(),
                )
            }),
            now,
        )
    }

    pub fn add_viewer(&mut self, cx: &mut Context<Self>) {
        self.viewers += 1;
        self.refresh(cx);
    }

    pub fn remove_viewer(&mut self, cx: &mut Context<Self>) {
        self.viewers = self.viewers.saturating_sub(1);
        self.update_tail_task(cx);
    }

    /// Keeps `path` tailed while a dashboard previews its log.
    pub fn set_output_previewed(&mut self, path: PathBuf, previewed: bool, cx: &mut Context<Self>) {
        if previewed {
            self.previewed_outputs.insert(path);
        } else {
            self.previewed_outputs.remove(&path);
        }
        self.update_tail_task(cx);
    }

    fn tailed_output_paths(&self) -> Vec<PathBuf> {
        let mut paths = self.previewed_outputs.clone();
        for snapshot in self.snapshots() {
            for item in &snapshot.background {
                if item.status.is_running()
                    && let Some(path) = &item.output_path
                {
                    paths.insert(path.clone());
                }
            }
        }
        paths.into_iter().collect()
    }

    /// Tails the output files of running background commands, and of those
    /// with an open log preview, while a dashboard is open.
    fn update_tail_task(&mut self, cx: &mut Context<Self>) {
        let wanted = self.viewers > 0 && !self.tailed_output_paths().is_empty();
        if !wanted {
            self.tail_task = None;
            return;
        }
        if self.tail_task.is_some() {
            return;
        }
        self.tail_task = Some(cx.spawn(async move |this, cx| {
            loop {
                let Ok(paths) = this.read_with(cx, |this, _| this.tailed_output_paths()) else {
                    return;
                };
                if paths.is_empty() {
                    break;
                }
                let tails = cx
                    .background_spawn(async move {
                        paths
                            .into_iter()
                            .map(|path| {
                                let tail = read_output_tail(&path);
                                (path, tail)
                            })
                            .collect::<Vec<_>>()
                    })
                    .await;
                let Ok(()) = this.update(cx, |this, cx| {
                    let mut changed = false;
                    for (path, tail) in tails {
                        let Some(tail) = tail else { continue };
                        if this.output_tails.get(&path) != Some(&tail) {
                            this.output_tails.insert(path, tail);
                            changed = true;
                        }
                    }
                    if changed {
                        this.invalidate(cx);
                    }
                }) else {
                    return;
                };
                cx.background_executor().timer(TAIL_INTERVAL).await;
            }
            this.update(cx, |this, _| this.tail_task = None).ok();
        }));
    }

    /// Brings the thread's panel or tab to the front (switching window and
    /// project tab) and scrolls it to `entry_ix`.
    pub fn focus_thread(entity_id: EntityId, entry_ix: Option<usize>, cx: &mut App) {
        let Some(location) = resolve_locations(cx).remove(&entity_id) else {
            return;
        };
        focus_location(location, entry_ix, None, cx);
    }

    /// Focuses the thread and inserts `text` into its message editor.
    pub fn insert_into_thread_message(entity_id: EntityId, text: String, cx: &mut App) {
        let Some(location) = resolve_locations(cx).remove(&entity_id) else {
            return;
        };
        focus_location(location, None, Some(text), cx);
    }

    pub fn thread(&self, entity_id: EntityId) -> Option<Entity<AcpThread>> {
        self.threads
            .iter()
            .find(|tracked| tracked.entity_id == entity_id)
            .and_then(|tracked| tracked.thread.upgrade())
    }

    /// Kills the terminals we own (ACP `terminal/create`) behind a tool call.
    pub fn stop_terminal(thread: &Entity<AcpThread>, call_id: &SharedString, cx: &mut App) {
        let terminals = thread
            .read(cx)
            .entries()
            .iter()
            .find_map(|entry| match entry {
                AgentThreadEntry::ToolCall(call) if call.id.0.as_ref() == call_id.as_ref() => {
                    Some(call.terminals().cloned().collect::<Vec<_>>())
                }
                _ => None,
            })
            .unwrap_or_default();
        for terminal in terminals {
            terminal.update(cx, |terminal, cx| terminal.stop_by_user(cx));
        }
    }
}

fn build_snapshot(
    tracked: &mut TrackedThread,
    thread: &Entity<AcpThread>,
    location: Option<&ThreadLocation>,
    tails: &HashMap<PathBuf, OutputTail>,
    now: Instant,
    cx: &App,
) -> ThreadSnapshot {
    let thread_ref = thread.read(cx);
    let generating = thread_ref.status() == ThreadStatus::Generating;
    if generating && !tracked.was_generating {
        tracked.turn_started_at = Some(now);
        tracked.turn_ended_at = None;
        tracked.errored = false;
        tracked.finished_a_turn = false;
    } else if !generating && tracked.was_generating {
        tracked.turn_ended_at = Some(now);
        tracked.finished_a_turn = true;
    }
    tracked.was_generating = generating;

    let mut records = Vec::new();
    let mut last_user_message_ix = None;
    for (ix, entry) in thread_ref.entries().iter().enumerate() {
        match entry {
            AgentThreadEntry::UserMessage(_) => last_user_message_ix = Some(ix),
            AgentThreadEntry::ToolCall(call) => {
                records.push(call_record(ix, call, &mut tracked.background_refs, cx));
            }
            _ => {}
        }
    }

    for record in &records {
        let timing = tracked
            .call_timings
            .entry(record.id.clone())
            .or_insert_with(|| {
                // Calls replayed from history arrive finished, outside a turn.
                if record.status.is_active() || generating {
                    CallTiming {
                        started_at: Some(now),
                        finished_at: None,
                    }
                } else {
                    CallTiming::default()
                }
            });
        let status = model::effective_call_status(record.status, generating);
        if timing.started_at.is_some() && timing.finished_at.is_none() && !status.is_active() {
            timing.finished_at = Some(now);
        }
    }

    let state = model::thread_state(
        generating,
        thread_ref.is_waiting_for_confirmation(),
        tracked.errored,
        tracked.finished_a_turn,
    );
    let title = location
        .and_then(|location| location.title.clone())
        .or_else(|| thread_ref.title())
        .unwrap_or_else(|| "New Thread".into());
    let context = TreeContext {
        generating,
        last_user_message_ix,
    };
    let mut root = model::build_tree(
        &records,
        &tracked.call_timings,
        state.as_node_status(),
        title.clone(),
        context,
    );
    root.timing = CallTiming {
        started_at: tracked.turn_started_at,
        finished_at: tracked.turn_ended_at,
    };
    for child in &mut root.children {
        apply_node_timings(child, &mut tracked.node_timings, generating, now);
    }

    let background = model::build_background(&records, &tracked.call_timings, tails);
    let project = thread_ref
        .project()
        .read(cx)
        .visible_worktrees(cx)
        .next()
        .map(|worktree| SharedString::from(worktree.read(cx).root_name_str().to_string()))
        .filter(|name| !name.is_empty());

    ThreadSnapshot {
        thread: thread.downgrade(),
        entity_id: tracked.entity_id,
        title,
        project,
        location: location.map(|location| location.kind),
        state,
        turn_started_at: tracked.turn_started_at,
        turn_ended_at: tracked.turn_ended_at,
        tokens: thread_ref
            .token_usage()
            .map(|usage| (usage.used_tokens, usage.max_tokens)),
        cost: thread_ref
            .cost()
            .map(|cost| (cost.amount, cost.currency.clone())),
        tool_calls: records.len(),
        edits: records
            .iter()
            .filter(|record| record.role == CallRole::Edit)
            .count(),
        root,
        background,
        has_entries: !thread_ref.entries().is_empty(),
    }
}

/// A sub-agent runs from when we first see it active until it isn't (for a
/// background sub-agent that's later than its spawn call's completion).
fn apply_node_timings(
    node: &mut AgentNode,
    timings: &mut HashMap<SharedString, CallTiming>,
    generating: bool,
    now: Instant,
) {
    if let Some(id) = &node.id {
        let timing = timings.entry(id.clone()).or_insert_with(|| {
            if node.status.is_active() || generating {
                CallTiming {
                    started_at: node.timing.started_at.or(Some(now)),
                    finished_at: None,
                }
            } else {
                CallTiming::default()
            }
        });
        if timing.started_at.is_some() && timing.finished_at.is_none() && !node.status.is_active() {
            timing.finished_at = Some(now);
        }
        node.timing = *timing;
    }
    for child in &mut node.children {
        apply_node_timings(child, timings, generating, now);
    }
}

fn call_record(
    entry_ix: usize,
    call: &ToolCall,
    background_refs: &mut HashMap<SharedString, Option<BackgroundTaskRef>>,
    cx: &App,
) -> CallRecord {
    let id = SharedString::from(call.id.0.to_string());
    let role = model::call_role(
        call.tool_name.as_deref(),
        call.kind,
        call.raw_input.as_ref(),
    );
    let status = match &call.status {
        ToolCallStatus::Pending => NodeStatus::Pending,
        ToolCallStatus::WaitingForConfirmation { .. } => NodeStatus::WaitingForPermission,
        ToolCallStatus::InProgress => NodeStatus::Running,
        ToolCallStatus::Completed => NodeStatus::Completed,
        ToolCallStatus::Failed => NodeStatus::Failed,
        ToolCallStatus::Rejected | ToolCallStatus::Canceled => NodeStatus::Canceled,
    };
    let title = call
        .title()
        .cloned()
        .or_else(|| call.tool_name.clone())
        .unwrap_or_else(|| "Tool call".into());

    let last_output_line = match &role {
        CallRole::SubAgent { .. } | CallRole::Workflow { .. } => {
            call.content.iter().rev().find_map(|content| match content {
                ToolCallContent::ContentBlock(block) => model::last_line(block.to_markdown(cx)),
                _ => None,
            })
        }
        _ => None,
    };

    // The launch text doesn't change once the call completed: parse it once.
    let background_task = match &role {
        CallRole::Shell {
            background: true, ..
        }
        | CallRole::Monitor { .. } => {
            if let Some(cached) = background_refs.get(&id) {
                cached.clone()
            } else {
                let parsed = model::parse_background_launch(&call_output_text(call, cx));
                if status == NodeStatus::Completed {
                    background_refs.insert(id.clone(), parsed.clone());
                }
                parsed
            }
        }
        _ => None,
    };

    let terminal = call.terminals().next().map(|terminal| {
        let terminal = terminal.read(cx);
        let inner = terminal.inner().read(cx);
        let owned = inner.pid_getter().is_some();
        let running = owned && terminal.output().is_none();
        TerminalInfo {
            owned,
            running,
            exit_code: None,
            cwd: terminal
                .working_dir()
                .as_ref()
                .map(|dir| SharedString::from(dir.to_string_lossy().to_string())),
            started_at: terminal.started_at(),
            last_line: if running {
                inner
                    .last_n_non_empty_lines(1)
                    .pop()
                    .map(|line| SharedString::from(model::strip_ansi(&line)))
            } else {
                None
            },
            output: if running {
                model::output_preview(
                    &inner
                        .last_n_non_empty_lines(model::PREVIEW_LINES)
                        .join("\n"),
                )
            } else {
                None
            },
        }
    });

    CallRecord {
        id,
        parent_id: call
            .parent_tool_call_id
            .as_ref()
            .map(|parent| SharedString::from(parent.0.to_string())),
        entry_ix,
        title,
        role,
        status,
        last_output_line,
        background_task,
        terminal,
    }
}

/// The text a tool call returned: its terminal, content or raw output.
fn call_output_text(call: &ToolCall, cx: &App) -> String {
    let mut text = String::new();
    for content in &call.content {
        match content {
            ToolCallContent::Terminal(terminal) => {
                text.push_str(&terminal.read(cx).inner().read(cx).get_content());
            }
            ToolCallContent::ContentBlock(block) => text.push_str(block.to_markdown(cx)),
            ToolCallContent::Diff(_) => {}
        }
        text.push('\n');
    }
    match &call.raw_output {
        Some(serde_json::Value::String(output)) => text.push_str(output),
        Some(value @ (serde_json::Value::Array(_) | serde_json::Value::Object(_))) => {
            text.push_str(&value.to_string())
        }
        _ => {}
    }
    text
}

fn read_output_tail(path: &Path) -> Option<OutputTail> {
    let mut file = std::fs::File::open(path).ok()?;
    let metadata = file.metadata().ok()?;
    let len = metadata.len();
    let start = len.saturating_sub(TAIL_BYTES);
    file.seek(SeekFrom::Start(start)).ok()?;
    let mut bytes = Vec::new();
    file.take(TAIL_BYTES).read_to_end(&mut bytes).ok()?;
    let text = String::from_utf8_lossy(&bytes);
    // A tail that starts inside the file most likely starts inside a line.
    let text = match text.split_once('\n') {
        Some((_, rest)) if start > 0 => rest,
        _ => text.as_ref(),
    };
    let lines: Vec<&str> = text.lines().rev().take(60).collect::<Vec<_>>();
    let lines: Vec<&str> = lines.into_iter().rev().collect();
    Some(OutputTail {
        output: model::output_preview(text),
        last_line: model::last_line(text),
        url: model::detect_url(&lines),
        modified_at: metadata.modified().ok().or(Some(SystemTime::UNIX_EPOCH)),
    })
}

enum LocationTarget {
    Panel(Entity<AgentPanel>),
    Tab(Entity<ClaudeTab>),
}

struct ThreadLocation {
    kind: LocationKind,
    window: WindowHandle<MultiWorkspace>,
    workspace: Entity<Workspace>,
    conversation_view: Entity<ConversationView>,
    target: LocationTarget,
    thread_id: ThreadId,
    title: Option<SharedString>,
}

/// Where each open thread is shown, keyed by its `AcpThread` entity id.
fn resolve_locations(cx: &App) -> HashMap<EntityId, ThreadLocation> {
    let mut locations = HashMap::default();
    for window in cx.windows() {
        let Some(window) = window.downcast::<MultiWorkspace>() else {
            continue;
        };
        let Ok(multi_workspace) = window.read(cx) else {
            continue;
        };
        for workspace in multi_workspace.workspaces() {
            let workspace_ref = workspace.read(cx);
            let mut add = |conversation_view: &Entity<ConversationView>,
                           kind: LocationKind,
                           target: LocationTarget| {
                let view = conversation_view.read(cx);
                let Some(thread_view) = view.root_thread_view() else {
                    return;
                };
                let thread_entity_id = thread_view.read(cx).thread.entity_id();
                locations
                    .entry(thread_entity_id)
                    .or_insert_with(|| ThreadLocation {
                        kind,
                        window,
                        workspace: workspace.clone(),
                        conversation_view: conversation_view.clone(),
                        target,
                        thread_id: view.parent_id(),
                        title: view.thread_title(cx),
                    });
            };
            for tab in workspace_ref.items_of_type::<ClaudeTab>(cx) {
                let conversation_view = tab.read(cx).conversation_view().clone();
                add(
                    &conversation_view,
                    LocationKind::Tab,
                    LocationTarget::Tab(tab),
                );
            }
            if let Some(panel) = workspace_ref.panel::<AgentPanel>(cx) {
                for conversation_view in panel.read(cx).conversation_views() {
                    add(
                        &conversation_view,
                        LocationKind::Panel,
                        LocationTarget::Panel(panel.clone()),
                    );
                }
            }
        }
    }
    locations
}

fn focus_location(
    location: ThreadLocation,
    entry_ix: Option<usize>,
    message_text: Option<String>,
    cx: &mut App,
) {
    let ThreadLocation {
        window,
        workspace,
        conversation_view,
        target,
        thread_id,
        ..
    } = location;
    window
        .update(cx, |multi_workspace, window, cx| {
            window.activate_window();
            multi_workspace.activate(workspace.clone(), None, window, cx);
            workspace.update(cx, |workspace, cx| match &target {
                LocationTarget::Panel(panel) => {
                    workspace.focus_panel::<AgentPanel>(window, cx);
                    let is_active =
                        panel.read(cx).active_conversation_view() == Some(&conversation_view);
                    if !is_active {
                        panel.update(cx, |panel, cx| {
                            panel.activate_retained_thread(thread_id, true, window, cx);
                        });
                    }
                }
                LocationTarget::Tab(tab) => {
                    workspace.activate_item(tab, true, true, window, cx);
                }
            });
            if let Some(entry_ix) = entry_ix
                && let Some(thread_view) = conversation_view.read(cx).root_thread_view()
            {
                thread_view.update(cx, |thread_view, cx| {
                    thread_view.list_state.scroll_to(ListOffset {
                        item_ix: entry_ix,
                        offset_in_item: px(0.),
                    });
                    cx.notify();
                });
            }
            if let Some(text) = message_text
                && let Some(thread_view) = conversation_view.read(cx).root_thread_view()
            {
                let message_editor = thread_view.read(cx).message_editor.clone();
                message_editor.update(cx, |message_editor, cx| {
                    message_editor.insert_text(&text, window, cx);
                });
                message_editor.focus_handle(cx).focus(window, cx);
            }
        })
        .log_err();
}

#[cfg(test)]
#[path = "store_tests.rs"]
mod tests;
