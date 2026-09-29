//! Pure data model of the dashboard: tool call records extracted from a
//! thread, the agent tree built from them (main agent → sub-agents →
//! nested sub-agents / workflows) and the background work they started.
//!
//! Nothing here touches gpui entities, so it is cheap to test.
//!
//! What the Claude Code ACP adapter (`@agentclientprotocol/claude-agent-acp`)
//! tells a client like ours, which advertises neither native sub-agent
//! sessions nor async tasks:
//! * A sub-agent is an `Agent` (or legacy `Task`) tool call: `name: "Agent"`,
//!   `kind: think`, `title` = the input's `description`, `rawInput` =
//!   `{description, prompt, subagent_type, name?, run_in_background?, …}`.
//! * Every tool call the sub-agent makes arrives in the *root* session with
//!   `_meta.claudeCode.parentToolUseId` = the spawning `Agent` call's id
//!   (nested sub-agents the same way), see
//!   [`acp_thread::parent_tool_call_id_from_meta`]. The sub-agent's own text is
//!   not forwarded.
//! * A `Workflow` tool call carries the workflow script in `rawInput.script`,
//!   whose `export const meta = { name, description, phases }` names the
//!   phases. The workflow runs as a background task whose progress is only
//!   published to AIR clients, so per-agent phase membership is unknown.
//! * A backgrounded `Bash` call (`rawInput.run_in_background: true`)
//!   completes immediately; its output says
//!   `Command running in background with ID: <id>. … Output is being written
//!   to: <path>. You will be notified …`. Its later lifecycle is again
//!   AIR-only, so we only learn it stopped from a `TaskStop` call.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use collections::{HashMap, HashSet};
use gpui::SharedString;
use regex::Regex;
use std::sync::LazyLock;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum NodeStatus {
    Pending,
    Running,
    WaitingForPermission,
    Completed,
    Failed,
    Canceled,
}

impl NodeStatus {
    pub fn is_active(self) -> bool {
        matches!(
            self,
            NodeStatus::Pending | NodeStatus::Running | NodeStatus::WaitingForPermission
        )
    }

    pub fn label(self) -> &'static str {
        match self {
            NodeStatus::Pending => "Pending",
            NodeStatus::Running => "Running",
            NodeStatus::WaitingForPermission => "Needs permission",
            NodeStatus::Completed => "Done",
            NodeStatus::Failed => "Failed",
            NodeStatus::Canceled => "Stopped",
        }
    }
}

/// What a tool call is, as far as the dashboard cares.
#[derive(Clone, Debug, PartialEq)]
pub enum CallRole {
    SubAgent {
        subagent_type: Option<SharedString>,
        name: Option<SharedString>,
        background: bool,
    },
    Workflow {
        name: Option<SharedString>,
        phases: Vec<SharedString>,
    },
    Shell {
        command: SharedString,
        description: Option<SharedString>,
        background: bool,
    },
    Monitor {
        description: Option<SharedString>,
        command: Option<SharedString>,
        timeout: Option<Duration>,
    },
    StopTask {
        task_id: Option<SharedString>,
    },
    Edit,
    Other,
}

/// A task Claude Code started in the background (from a tool result).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BackgroundTaskRef {
    pub task_id: SharedString,
    pub output_path: Option<PathBuf>,
}

/// A terminal attached to a tool call's content.
#[derive(Clone, Debug, PartialEq)]
pub struct TerminalInfo {
    /// We spawned the process (ACP `terminal/create`), so we can kill it.
    /// Claude Code's own `Bash` terminals are display-only mirrors.
    pub owned: bool,
    pub running: bool,
    pub exit_code: Option<i32>,
    pub cwd: Option<SharedString>,
    pub started_at: Instant,
    pub last_line: Option<SharedString>,
}

#[derive(Clone, Debug)]
pub struct CallRecord {
    pub id: SharedString,
    pub parent_id: Option<SharedString>,
    pub entry_ix: usize,
    pub title: SharedString,
    pub role: CallRole,
    pub status: NodeStatus,
    /// Last non-empty line of the call's output (sub-agent report, command
    /// output). Only filled for sub-agents and shells.
    pub last_output_line: Option<SharedString>,
    /// For a backgrounded shell or monitor: the task it started.
    pub background_task: Option<BackgroundTaskRef>,
    pub terminal: Option<TerminalInfo>,
}

impl CallRecord {
    pub fn is_agent(&self) -> bool {
        matches!(
            self.role,
            CallRole::SubAgent { .. } | CallRole::Workflow { .. }
        )
    }
}

/// When the dashboard saw a tool call start and finish. Calls first seen
/// already finished (history replay) have no timing.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CallTiming {
    pub started_at: Option<Instant>,
    pub finished_at: Option<Instant>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AgentNodeKind {
    Main,
    SubAgent,
    Workflow,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Activity {
    pub text: SharedString,
    pub entry_ix: usize,
    pub status: NodeStatus,
}

#[derive(Clone, Debug)]
pub struct AgentNode {
    /// The spawning tool call; `None` for the main agent.
    pub id: Option<SharedString>,
    pub entry_ix: Option<usize>,
    pub kind: AgentNodeKind,
    pub label: SharedString,
    pub subagent_type: Option<SharedString>,
    pub background: bool,
    pub status: NodeStatus,
    /// Tool calls made directly by this agent (not by its sub-agents).
    pub tool_calls: usize,
    pub edits: usize,
    pub activity: Option<Activity>,
    pub last_output: Option<SharedString>,
    pub phases: Vec<SharedString>,
    pub children: Vec<AgentNode>,
    pub timing: CallTiming,
}

impl AgentNode {
    /// Running sub-agents in this subtree, excluding `self`.
    pub fn active_descendants(&self) -> usize {
        self.children
            .iter()
            .map(|child| usize::from(child.status.is_active()) + child.active_descendants())
            .sum()
    }

    pub fn descendant_count(&self) -> usize {
        self.children
            .iter()
            .map(|child| 1 + child.descendant_count())
            .sum()
    }

    fn with_waiting(mut self, waiting: bool) -> Self {
        if waiting && self.status.is_active() {
            self.status = NodeStatus::WaitingForPermission;
        }
        self
    }

    fn any_waiting(&self) -> bool {
        self.status == NodeStatus::WaitingForPermission
            || self.children.iter().any(AgentNode::any_waiting)
    }
}

/// The facts about the thread a tree build needs.
#[derive(Clone, Copy, Debug, Default)]
pub struct TreeContext {
    /// The thread's turn is running (the adapter holds a turn open while
    /// background sub-agents it spawned are still working).
    pub generating: bool,
    /// Index of the last user message: background sub-agents spawned before
    /// it belong to an earlier, finished turn.
    pub last_user_message_ix: Option<usize>,
}

/// Builds the agent tree from a thread's tool calls (in entry order).
pub fn build_tree(
    records: &[CallRecord],
    timings: &HashMap<SharedString, CallTiming>,
    main_status: NodeStatus,
    main_label: SharedString,
    context: TreeContext,
) -> AgentNode {
    let agent_ids: HashSet<&SharedString> = records
        .iter()
        .filter(|record| record.is_agent())
        .map(|record| &record.id)
        .collect();

    // Children by parent agent; calls whose parent is unknown go to the root.
    let mut children_of: HashMap<Option<&SharedString>, Vec<&CallRecord>> = HashMap::default();
    for record in records {
        let parent = record
            .parent_id
            .as_ref()
            .filter(|parent| agent_ids.contains(parent) && *parent != &record.id);
        children_of.entry(parent).or_default().push(record);
    }

    let mut visited = HashSet::default();
    let mut root = build_node(
        None,
        &children_of,
        timings,
        context,
        &mut visited,
        main_status,
        main_label,
    );
    root.kind = AgentNodeKind::Main;
    if root.status.is_active() && root.any_waiting() {
        root.status = NodeStatus::WaitingForPermission;
    }
    root
}

fn build_node(
    record: Option<&CallRecord>,
    children_of: &HashMap<Option<&SharedString>, Vec<&CallRecord>>,
    timings: &HashMap<SharedString, CallTiming>,
    context: TreeContext,
    visited: &mut HashSet<SharedString>,
    main_status: NodeStatus,
    main_label: SharedString,
) -> AgentNode {
    let key = record.map(|record| &record.id);
    let calls = children_of.get(&key).map(Vec::as_slice).unwrap_or_default();

    let mut children = Vec::new();
    let mut tool_calls = 0;
    let mut edits = 0;
    let mut latest_active: Option<&CallRecord> = None;
    let mut latest: Option<&CallRecord> = None;
    let mut waiting = false;
    for call in calls {
        if call.is_agent() {
            if visited.insert(call.id.clone()) {
                children.push(build_node(
                    Some(call),
                    children_of,
                    timings,
                    context,
                    visited,
                    main_status,
                    main_label.clone(),
                ));
            }
            continue;
        }
        tool_calls += 1;
        if call.role == CallRole::Edit {
            edits += 1;
        }
        latest = Some(call);
        waiting |= call.status == NodeStatus::WaitingForPermission;
        if call.status.is_active() {
            latest_active = Some(call);
        }
    }

    let activity = latest_active.or(latest).map(|call| Activity {
        text: activity_text(call),
        entry_ix: call.entry_ix,
        status: effective_call_status(call.status, context.generating),
    });

    let Some(record) = record else {
        return AgentNode {
            id: None,
            entry_ix: None,
            kind: AgentNodeKind::Main,
            label: main_label,
            subagent_type: None,
            background: false,
            status: main_status,
            tool_calls,
            edits,
            activity,
            last_output: None,
            phases: Vec::new(),
            children,
            timing: CallTiming::default(),
        }
        .with_waiting(waiting);
    };

    let (kind, subagent_type, background, phases, label) = match &record.role {
        CallRole::SubAgent {
            subagent_type,
            name,
            background,
        } => (
            AgentNodeKind::SubAgent,
            subagent_type.clone(),
            *background,
            Vec::new(),
            name.clone()
                .filter(|_| record.title.trim().is_empty() || record.title == "Task")
                .unwrap_or_else(|| record.title.clone()),
        ),
        CallRole::Workflow { name, phases } => (
            AgentNodeKind::Workflow,
            None,
            true,
            phases.clone(),
            name.clone()
                .map(|name| SharedString::from(format!("Workflow: {name}")))
                .unwrap_or_else(|| record.title.clone()),
        ),
        _ => (
            AgentNodeKind::SubAgent,
            None,
            false,
            Vec::new(),
            record.title.clone(),
        ),
    };

    let mut node = AgentNode {
        id: Some(record.id.clone()),
        entry_ix: Some(record.entry_ix),
        kind,
        label,
        subagent_type,
        background,
        status: agent_status(record, background, &children, context),
        tool_calls,
        edits,
        activity,
        last_output: record.last_output_line.clone(),
        phases,
        children,
        timing: timings.get(&record.id).copied().unwrap_or_default(),
    };
    if node.status.is_active() && (waiting || node.any_waiting()) {
        node.status = NodeStatus::WaitingForPermission;
    }
    node
}

/// A tool call's status as the dashboard shows it: work that is still
/// "running" when the turn has ended was cut off.
pub fn effective_call_status(status: NodeStatus, generating: bool) -> NodeStatus {
    if !generating && matches!(status, NodeStatus::Pending | NodeStatus::Running) {
        NodeStatus::Canceled
    } else {
        status
    }
}

fn agent_status(
    record: &CallRecord,
    background: bool,
    children: &[AgentNode],
    context: TreeContext,
) -> NodeStatus {
    let own = effective_call_status(record.status, context.generating);
    if own == NodeStatus::Completed && background && context.generating {
        // The spawn call returned right away; the sub-agent runs on while
        // the turn is held open. Only this turn's spawns can still run.
        let in_current_turn = context
            .last_user_message_ix
            .is_none_or(|user_ix| record.entry_ix > user_ix);
        if in_current_turn {
            return NodeStatus::Running;
        }
    }
    if own == NodeStatus::Completed && children.iter().any(|child| child.status.is_active()) {
        return NodeStatus::Running;
    }
    own
}

fn activity_text(call: &CallRecord) -> SharedString {
    let title = call.title.replace('`', "");
    match &call.role {
        CallRole::Shell { command, .. } => format!("Running {}", first_line(command)).into(),
        CallRole::Monitor {
            description: Some(description),
            ..
        } => format!("Monitoring {description}").into(),
        _ => first_line(&title).into(),
    }
}

fn first_line(text: &str) -> String {
    let mut lines = text.lines();
    let first = lines.next().unwrap_or_default().trim();
    if lines.next().is_some() {
        format!("{first}…")
    } else {
        first.to_string()
    }
}

/// Classifies a tool call from its ACP name, kind and raw input.
pub fn call_role(
    tool_name: Option<&str>,
    kind: agent_client_protocol::schema::v1::ToolKind,
    raw_input: Option<&serde_json::Value>,
) -> CallRole {
    use agent_client_protocol::schema::v1::ToolKind;

    let string = |key: &str| -> Option<SharedString> {
        raw_input
            .and_then(|input| input.get(key))
            .and_then(|value| value.as_str())
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(|value| SharedString::from(value.to_string()))
    };
    let flag = |key: &str| -> bool {
        raw_input
            .and_then(|input| input.get(key))
            .and_then(|value| value.as_bool())
            .unwrap_or(false)
    };

    match tool_name {
        Some("Agent" | "Task") => CallRole::SubAgent {
            subagent_type: string("subagent_type"),
            name: string("name"),
            background: flag("run_in_background"),
        },
        Some("Workflow") => {
            let script = raw_input
                .and_then(|input| input.get("script"))
                .and_then(|value| value.as_str())
                .unwrap_or_default();
            let (script_name, phases) = parse_workflow_meta(script);
            CallRole::Workflow {
                name: script_name.or_else(|| string("name")),
                phases,
            }
        }
        Some("Bash" | "PowerShell") => CallRole::Shell {
            command: string("command").unwrap_or_else(|| "Terminal".into()),
            description: string("description"),
            background: flag("run_in_background"),
        },
        Some("Monitor") => CallRole::Monitor {
            description: string("description"),
            command: string("command").or_else(|| {
                raw_input
                    .and_then(|input| input.get("ws"))
                    .and_then(|ws| ws.get("url"))
                    .and_then(|url| url.as_str())
                    .map(|url| SharedString::from(url.to_string()))
            }),
            timeout: raw_input
                .and_then(|input| input.get("timeout_ms"))
                .and_then(|value| value.as_u64())
                .map(|ms| Duration::from_millis(ms.min(1_800_000))),
        },
        Some("TaskStop" | "KillShell" | "KillBash") => CallRole::StopTask {
            task_id: string("task_id").or_else(|| string("shell_id")),
        },
        Some("Edit" | "Write" | "MultiEdit" | "NotebookEdit") => CallRole::Edit,
        _ if kind == ToolKind::Edit || kind == ToolKind::Delete || kind == ToolKind::Move => {
            CallRole::Edit
        }
        // Agents that don't send tool names: a `think` call with an agent-like input.
        None if kind == ToolKind::Think && string("subagent_type").is_some() => {
            CallRole::SubAgent {
                subagent_type: string("subagent_type"),
                name: string("name"),
                background: flag("run_in_background"),
            }
        }
        None if kind == ToolKind::Execute => CallRole::Shell {
            command: string("command").unwrap_or_else(|| "Terminal".into()),
            description: string("description"),
            background: flag("run_in_background"),
        },
        _ => CallRole::Other,
    }
}

/// `(name, phases)` from a workflow script's
/// `export const meta = { name, description, phases }` literal. Phases may be
/// strings or objects with a `title` / `name`.
pub fn parse_workflow_meta(script: &str) -> (Option<SharedString>, Vec<SharedString>) {
    static META: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"(?s)\bmeta\s*=\s*\{").expect("valid regex"));
    static NAME: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r#"(?s)\b(?:name|title)\s*:\s*(?:'([^']*)'|"([^"]*)"|`([^`]*)`)"#)
            .expect("valid regex")
    });

    let Some(meta_start) = META.find(script).map(|m| m.end() - 1) else {
        return (None, Vec::new());
    };
    let Some(meta_body) = balanced(&script[meta_start..], '{', '}') else {
        return (None, Vec::new());
    };

    let phases_at = meta_body.find("phases");
    let phases = phases_at
        .and_then(|at| {
            let rest = &meta_body[at..];
            let open = rest.find('[')?;
            balanced(&rest[open..], '[', ']')
        })
        .map(|array| {
            split_top_level(&array[1..array.len() - 1])
                .into_iter()
                .filter_map(|element| {
                    let element = element.trim();
                    if let Some(literal) = string_literal(element) {
                        Some(literal)
                    } else if element.starts_with('{') {
                        NAME.captures(element).and_then(|captures| {
                            captures
                                .iter()
                                .skip(1)
                                .flatten()
                                .next()
                                .map(|m| m.as_str().to_string())
                        })
                    } else {
                        None
                    }
                })
                .filter(|phase| !phase.trim().is_empty())
                .map(SharedString::from)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();

    // The workflow's own name: a `name:` of the meta object that isn't inside
    // the phases array.
    let head = phases_at.map_or(meta_body, |at| &meta_body[..at]);
    let name = NAME
        .captures(head)
        .and_then(|captures| captures.iter().skip(1).flatten().next())
        .map(|m| SharedString::from(m.as_str().to_string()))
        .filter(|name| !name.trim().is_empty());
    (name, phases)
}

/// The text from `text`'s first char (which must be `open`) to its matching
/// `close`, skipping string literals.
fn balanced(text: &str, open: char, close: char) -> Option<&str> {
    let mut depth = 0usize;
    let mut quote: Option<char> = None;
    let mut escaped = false;
    for (ix, ch) in text.char_indices() {
        if let Some(q) = quote {
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == q {
                quote = None;
            }
            continue;
        }
        match ch {
            '\'' | '"' | '`' => quote = Some(ch),
            c if c == open => depth += 1,
            c if c == close => {
                depth = depth.checked_sub(1)?;
                if depth == 0 {
                    return Some(&text[..ix + ch.len_utf8()]);
                }
            }
            _ => {}
        }
    }
    None
}

fn split_top_level(text: &str) -> Vec<&str> {
    let mut parts = Vec::new();
    let mut depth = 0i32;
    let mut quote: Option<char> = None;
    let mut escaped = false;
    let mut start = 0;
    for (ix, ch) in text.char_indices() {
        if let Some(q) = quote {
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == q {
                quote = None;
            }
            continue;
        }
        match ch {
            '\'' | '"' | '`' => quote = Some(ch),
            '{' | '[' | '(' => depth += 1,
            '}' | ']' | ')' => depth -= 1,
            ',' if depth == 0 => {
                parts.push(&text[start..ix]);
                start = ix + 1;
            }
            _ => {}
        }
    }
    if !text[start..].trim().is_empty() {
        parts.push(&text[start..]);
    }
    parts
}

fn string_literal(text: &str) -> Option<String> {
    let first = text.chars().next()?;
    if matches!(first, '\'' | '"' | '`') && text.len() >= 2 && text.ends_with(first) {
        Some(text[1..text.len() - 1].to_string())
    } else {
        None
    }
}

/// The task id and output file of a command Claude Code moved to the
/// background, from its tool result text.
pub fn parse_background_launch(text: &str) -> Option<BackgroundTaskRef> {
    static ID: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"(?i)(?:running in (?:the )?background|started|launched)[^\n]*?\bID:?\s*([A-Za-z0-9_-]+)")
            .expect("valid regex")
    });
    const OUTPUT_MARKER: &str = "Output is being written to: ";

    let task_id = ID.captures(text)?.get(1)?.as_str().to_string();
    let output_path = text.find(OUTPUT_MARKER).and_then(|start| {
        let rest = &text[start + OUTPUT_MARKER.len()..];
        let end = rest
            .find(". You will be notified")
            .or_else(|| rest.find('\n'))
            .unwrap_or(rest.len());
        let path = rest[..end].trim().trim_end_matches('.');
        (!path.is_empty()).then(|| PathBuf::from(path))
    });
    Some(BackgroundTaskRef {
        task_id: task_id.into(),
        output_path,
    })
}

/// A local URL a server announced in its output (newest line first).
pub fn detect_url(lines: &[impl AsRef<str>]) -> Option<SharedString> {
    static URL: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r#"https?://[^\s'"<>`)\]]+"#).expect("valid regex"));
    static PORT: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"(?i)\b(?:listening|running|serving|started|ready|available|server)\b[^\n]*?(?:\bport\b\s*:?\s*|localhost:|127\.0\.0\.1:|0\.0\.0\.0:)(\d{2,5})\b")
            .expect("valid regex")
    });

    let clean = |url: &str| -> SharedString {
        let url = url.trim_end_matches(['.', ',', ';', ':']);
        url.replacen("://0.0.0.0", "://localhost", 1)
            .replacen("://[::]", "://localhost", 1)
            .into()
    };
    let mut fallback: Option<SharedString> = None;
    for line in lines.iter().rev() {
        let line = strip_ansi(line.as_ref());
        for m in URL.find_iter(&line) {
            let url = m.as_str();
            let local = ["localhost", "127.0.0.1", "0.0.0.0", "[::1]", "[::]"]
                .iter()
                .any(|host| url.contains(&format!("://{host}")));
            if local {
                return Some(clean(url));
            }
            fallback.get_or_insert_with(|| clean(url));
        }
        if let Some(port) = PORT.captures(&line).and_then(|c| c.get(1)) {
            return Some(format!("http://localhost:{}", port.as_str()).into());
        }
    }
    fallback
}

pub fn strip_ansi(text: &str) -> String {
    static ANSI: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"\x1b\[[0-9;?]*[A-Za-z]|\x1b\][^\x07]*\x07").expect("valid regex")
    });
    ANSI.replace_all(text, "").into_owned()
}

/// The last non-empty line of `text`, without ANSI escapes.
pub fn last_line(text: &str) -> Option<SharedString> {
    text.lines()
        .rev()
        .map(|line| strip_ansi(line).trim().to_string())
        .find(|line| !line.is_empty() && !line.starts_with("```"))
        .map(SharedString::from)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BackgroundKind {
    Shell,
    Monitor,
    Terminal,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BackgroundStatus {
    /// Launched and not known to have stopped. For Claude Code's shells this
    /// is unverified: the adapter doesn't tell us when they exit.
    Running,
    Stopped,
    Exited(Option<i32>),
    Failed,
    /// A monitor past its timeout.
    Expired,
}

impl BackgroundStatus {
    pub fn is_running(self) -> bool {
        self == BackgroundStatus::Running
    }
}

/// The tail of a background task's output file.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct OutputTail {
    pub last_line: Option<SharedString>,
    pub url: Option<SharedString>,
    pub modified_at: Option<std::time::SystemTime>,
}

#[derive(Clone, Debug)]
pub struct BackgroundItem {
    pub call_id: SharedString,
    pub entry_ix: usize,
    /// The sub-agent that started it (`None` = the main agent).
    pub owner: Option<SharedString>,
    pub kind: BackgroundKind,
    pub command: SharedString,
    pub description: Option<SharedString>,
    pub cwd: Option<SharedString>,
    pub task_id: Option<SharedString>,
    pub output_path: Option<PathBuf>,
    pub status: BackgroundStatus,
    pub started_at: Option<Instant>,
    pub expires_at: Option<Instant>,
    pub last_line: Option<SharedString>,
    pub url: Option<SharedString>,
    pub last_output_at: Option<std::time::SystemTime>,
    /// We own the process and can kill it.
    pub can_stop: bool,
}

impl BackgroundItem {
    /// The status at `now` (a monitor expires on its own).
    pub fn status_at(&self, now: Instant) -> BackgroundStatus {
        match (self.status, self.expires_at) {
            (BackgroundStatus::Running, Some(expires_at)) if now >= expires_at => {
                BackgroundStatus::Expired
            }
            (status, _) => status,
        }
    }
}

/// The background work of a thread: backgrounded shells and monitors, and
/// terminals we own that outlived their tool call.
pub fn build_background(
    records: &[CallRecord],
    timings: &HashMap<SharedString, CallTiming>,
    tails: &HashMap<PathBuf, OutputTail>,
) -> Vec<BackgroundItem> {
    let stopped: HashSet<&str> = records
        .iter()
        .filter_map(|record| match &record.role {
            CallRole::StopTask { task_id: Some(id) } if record.status == NodeStatus::Completed => {
                Some(id.as_ref())
            }
            _ => None,
        })
        .collect();

    let mut items = Vec::new();
    for record in records {
        let timing = timings.get(&record.id).copied().unwrap_or_default();
        let (kind, command, description, timeout) = match &record.role {
            CallRole::Shell {
                command,
                description,
                background: true,
            } => (
                BackgroundKind::Shell,
                command.clone(),
                description.clone(),
                None,
            ),
            CallRole::Monitor {
                description,
                command,
                timeout,
            } => (
                BackgroundKind::Monitor,
                command.clone().unwrap_or_else(|| "Monitor".into()),
                description.clone(),
                *timeout,
            ),
            _ => {
                if let Some(terminal) = &record.terminal
                    && terminal.owned
                    && terminal.running
                    && !record.status.is_active()
                {
                    items.push(BackgroundItem {
                        call_id: record.id.clone(),
                        entry_ix: record.entry_ix,
                        owner: record.parent_id.clone(),
                        kind: BackgroundKind::Terminal,
                        command: record.title.clone(),
                        description: None,
                        cwd: terminal.cwd.clone(),
                        task_id: None,
                        output_path: None,
                        status: BackgroundStatus::Running,
                        started_at: Some(terminal.started_at),
                        expires_at: None,
                        last_line: terminal.last_line.clone(),
                        url: terminal
                            .last_line
                            .as_ref()
                            .and_then(|line| detect_url(&[line.as_ref()])),
                        last_output_at: None,
                        can_stop: true,
                    });
                }
                continue;
            }
        };

        let task = record.background_task.as_ref();
        let status = match record.status {
            NodeStatus::Failed => BackgroundStatus::Failed,
            NodeStatus::Canceled => BackgroundStatus::Stopped,
            // Still launching.
            status if status.is_active() => BackgroundStatus::Running,
            _ => {
                if task.is_some_and(|task| stopped.contains(task.task_id.as_ref())) {
                    BackgroundStatus::Stopped
                } else if kind == BackgroundKind::Shell && task.is_none() {
                    // It finished in the foreground after all (or the launch
                    // text changed): nothing runs in the background.
                    BackgroundStatus::Exited(None)
                } else {
                    BackgroundStatus::Running
                }
            }
        };
        let started_at = timing.started_at;
        let expires_at = match (kind, timeout) {
            (BackgroundKind::Monitor, Some(timeout)) => started_at.map(|at| at + timeout),
            _ => None,
        };
        // A monitor from history (no start time) is long gone.
        let status = if kind == BackgroundKind::Monitor
            && status == BackgroundStatus::Running
            && started_at.is_none()
        {
            BackgroundStatus::Expired
        } else {
            status
        };

        let tail = task
            .and_then(|task| task.output_path.as_ref())
            .and_then(|path| tails.get(path));
        items.push(BackgroundItem {
            call_id: record.id.clone(),
            entry_ix: record.entry_ix,
            owner: record.parent_id.clone(),
            kind,
            command,
            description,
            cwd: record.terminal.as_ref().and_then(|t| t.cwd.clone()),
            task_id: task.map(|task| task.task_id.clone()),
            output_path: task.and_then(|task| task.output_path.clone()),
            status,
            started_at,
            expires_at,
            last_line: tail.and_then(|tail| tail.last_line.clone()),
            url: tail.and_then(|tail| tail.url.clone()),
            last_output_at: tail.and_then(|tail| tail.modified_at),
            can_stop: false,
        });
    }
    items
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ThreadState {
    Idle,
    Generating,
    WaitingForPermission,
    Error,
    Finished,
}

impl ThreadState {
    pub fn label(self) -> &'static str {
        match self {
            ThreadState::Idle => "Idle",
            ThreadState::Generating => "Working",
            ThreadState::WaitingForPermission => "Needs permission",
            ThreadState::Error => "Error",
            ThreadState::Finished => "Finished",
        }
    }

    pub fn is_active(self) -> bool {
        matches!(
            self,
            ThreadState::Generating | ThreadState::WaitingForPermission
        )
    }

    pub fn as_node_status(self) -> NodeStatus {
        match self {
            ThreadState::Idle => NodeStatus::Pending,
            ThreadState::Generating => NodeStatus::Running,
            ThreadState::WaitingForPermission => NodeStatus::WaitingForPermission,
            ThreadState::Error => NodeStatus::Failed,
            ThreadState::Finished => NodeStatus::Completed,
        }
    }
}

pub fn thread_state(
    generating: bool,
    waiting_for_permission: bool,
    errored: bool,
    finished_a_turn: bool,
) -> ThreadState {
    if waiting_for_permission {
        ThreadState::WaitingForPermission
    } else if generating {
        ThreadState::Generating
    } else if errored {
        ThreadState::Error
    } else if finished_a_turn {
        ThreadState::Finished
    } else {
        ThreadState::Idle
    }
}

/// `(running agents, running background items)`: busy threads plus busy
/// sub-agents, and background work that is (believed to be) running.
pub fn running_counts<'a>(
    threads: impl IntoIterator<Item = (&'a ThreadState, &'a AgentNode, &'a [BackgroundItem])>,
    now: Instant,
) -> (usize, usize) {
    let mut agents = 0;
    let mut background = 0;
    for (state, root, items) in threads {
        if state.is_active() {
            agents += 1 + root.active_descendants();
        }
        background += items
            .iter()
            .filter(|item| item.status_at(now).is_running())
            .count();
    }
    (agents, background)
}

pub fn format_duration(duration: Duration) -> String {
    let secs = duration.as_secs();
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m {:02}s", secs / 60, secs % 60)
    } else {
        format!("{}h {:02}m", secs / 3600, (secs % 3600) / 60)
    }
}

pub fn format_tokens(tokens: u64) -> String {
    if tokens >= 1_000_000 {
        format!("{:.1}M", tokens as f64 / 1_000_000.0)
    } else if tokens >= 1_000 {
        format!("{:.1}k", tokens as f64 / 1_000.0)
    } else {
        tokens.to_string()
    }
}

#[cfg(test)]
#[path = "model_tests.rs"]
mod tests;
