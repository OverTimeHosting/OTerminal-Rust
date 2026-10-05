//! The Claude Code Remote Control sessions open in the app: the terminals
//! started from the Remote Control terminal profile, in every project tab of
//! every window.

use std::path::Path;
use std::sync::LazyLock;
use std::time::{Duration, Instant};

use collections::{HashMap, HashSet};
use gpui::{App, Entity, EntityId, Global, SharedString, TaskExt as _, WeakEntity, WindowHandle};
use regex::Regex;
use task::SpawnInTerminal;
use terminal::{TaskStatus, Terminal};
use terminal_view::{
    TerminalView,
    terminal_panel::{CLAUDE_REMOTE_CONTROL_PROFILE, TerminalPanel, terminal_profile_task_id},
};
use util::ResultExt as _;
use workspace::{MultiWorkspace, Workspace};

use crate::model;

const ENTER: &[u8] = b"\r";
const ESCAPE: &[u8] = b"\x1b";

/// How long after the text its Enter is sent. Claude Code reads a chunk of
/// input that has both as pasted text, where the Enter is a line break rather
/// than a submit.
const SUBMIT_DELAY: Duration = Duration::from_millis(150);

/// When each Remote Control terminal was created, as [`Terminal`] doesn't
/// record it.
#[derive(Default)]
struct SessionStartTimes(HashMap<EntityId, Instant>);

impl Global for SessionStartTimes {}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SessionState {
    Running,
    Exited,
}

impl SessionState {
    pub fn from_task_status(status: TaskStatus) -> Self {
        match status {
            TaskStatus::Running => Self::Running,
            TaskStatus::Unknown | TaskStatus::Completed { .. } => Self::Exited,
        }
    }

    pub fn is_running(self) -> bool {
        self == Self::Running
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Running => "Running",
            Self::Exited => "Exited",
        }
    }
}

#[derive(Clone)]
pub struct RemoteSession {
    pub terminal: WeakEntity<Terminal>,
    pub terminal_view: WeakEntity<TerminalView>,
    /// The entity id of `terminal`.
    pub entity_id: EntityId,
    pub window: WindowHandle<MultiWorkspace>,
    pub workspace: WeakEntity<Workspace>,
    pub project: Option<SharedString>,
    /// Set when it isn't the project's root.
    pub working_directory: Option<SharedString>,
    pub state: SessionState,
    pub started_at: Option<Instant>,
}

pub fn init(cx: &mut App) {
    cx.observe_new(|terminal: &mut Terminal, _, cx| {
        let is_session = terminal
            .task()
            .is_some_and(|task| is_remote_control_task(&task.spawned_task));
        if !is_session {
            return;
        }
        let entity_id = cx.entity_id();
        cx.default_global::<SessionStartTimes>()
            .0
            .insert(entity_id, Instant::now());
        cx.on_release(move |_, cx| {
            cx.default_global::<SessionStartTimes>()
                .0
                .remove(&entity_id);
        })
        .detach();
    })
    .detach();
}

/// Whether a terminal task was started from the Remote Control profile.
pub fn is_remote_control_task(spawned_task: &SpawnInTerminal) -> bool {
    spawned_task.id == terminal_profile_task_id(CLAUDE_REMOTE_CONTROL_PROFILE)
}

/// The Remote Control sessions of every project tab of every window, oldest
/// first.
///
/// Leaves out the sessions of a window that is being updated, as its
/// workspaces can't be read then: call this outside of window updates.
pub fn remote_sessions(cx: &App) -> Vec<RemoteSession> {
    let start_times = cx.try_global::<SessionStartTimes>();
    let mut sessions = Vec::new();
    let mut seen = HashSet::default();
    for window in cx.windows() {
        let Some(window) = window.downcast::<MultiWorkspace>() else {
            continue;
        };
        let Ok(multi_workspace) = window.read(cx) else {
            continue;
        };
        for workspace in multi_workspace.workspaces() {
            let workspace_ref = workspace.read(cx);
            let root = workspace_ref
                .project()
                .read(cx)
                .visible_worktrees(cx)
                .next()
                .map(|worktree| worktree.read(cx));
            let project = root
                .map(|root| SharedString::from(root.root_name_str().to_string()))
                .filter(|name| !name.is_empty());
            let project_root = root.map(|root| root.abs_path());

            let mut terminal_views = workspace_ref
                .panel::<TerminalPanel>(cx)
                .map(|panel| panel.read(cx).terminal_views(cx))
                .unwrap_or_default();
            terminal_views.extend(workspace_ref.items_of_type::<TerminalView>(cx));

            for terminal_view in terminal_views {
                let terminal = terminal_view.read(cx).terminal();
                let entity_id = terminal.entity_id();
                let terminal_ref = terminal.read(cx);
                let Some(task) = terminal_ref.task() else {
                    continue;
                };
                if !is_remote_control_task(&task.spawned_task) || !seen.insert(entity_id) {
                    continue;
                }
                let working_directory = terminal_ref
                    .working_directory()
                    .or_else(|| task.spawned_task.cwd.clone());
                sessions.push(RemoteSession {
                    terminal: terminal.downgrade(),
                    terminal_view: terminal_view.downgrade(),
                    entity_id,
                    window,
                    workspace: workspace.downgrade(),
                    project: project.clone(),
                    working_directory: working_directory_label(
                        working_directory.as_deref(),
                        project_root.as_deref(),
                    ),
                    state: SessionState::from_task_status(task.status),
                    started_at: start_times
                        .and_then(|start_times| start_times.0.get(&entity_id).copied()),
                });
            }
        }
    }
    sessions.sort_by_key(|session| (session.started_at, session.entity_id));
    sessions
}

/// The working directory to show for a session: none when it is the
/// project's root, which the project name already tells.
pub fn working_directory_label(
    working_directory: Option<&Path>,
    project_root: Option<&Path>,
) -> Option<SharedString> {
    let working_directory = working_directory?;
    if Some(working_directory) == project_root {
        return None;
    }
    Some(working_directory.to_string_lossy().to_string().into())
}

/// "3 sessions · 1 running".
pub fn sessions_summary(total: usize, running: usize) -> String {
    let noun = if total == 1 { "session" } else { "sessions" };
    format!("{total} {noun} · {running} running")
}

/// The link Claude Code prints to open a Remote Control session on claude.ai
/// (the newest in `output`).
///
/// Found when it is on one line of `output`; the terminal's own soft wrapping
/// doesn't split it there, a line break Claude Code puts inside it would.
pub fn extract_remote_control_url(output: &str) -> Option<SharedString> {
    static URL: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"https://claude\.ai/code[/?][A-Za-z0-9_\-./?=&%#~+:@]+").expect("valid regex")
    });
    let output = model::strip_ansi(output);
    let url = URL.find_iter(&output).last()?.as_str();
    Some(
        url.trim_end_matches(['.', ',', ';', ':'])
            .to_string()
            .into(),
    )
}

/// What to type into a session for the text of its input field: nothing for
/// blank text, and no control characters, which the terminal would take as
/// keys.
pub fn text_to_send(input: &str) -> Option<String> {
    let text: String = input
        .chars()
        .filter(|character| !character.is_control())
        .collect();
    if text.trim().is_empty() {
        None
    } else {
        Some(text)
    }
}

/// The end of the terminal's screen as plain text.
pub fn terminal_output(terminal: &Terminal) -> String {
    terminal
        .last_n_non_empty_lines(model::PREVIEW_LINES)
        .join("\n")
}

/// Types `text` into the session and submits it with Enter, like a message
/// or a slash command typed in its terminal.
pub fn send_text(terminal: &Entity<Terminal>, text: String, cx: &mut App) {
    terminal.update(cx, |terminal, _| terminal.input(text.into_bytes()));
    cx.spawn({
        let terminal = terminal.downgrade();
        async move |cx| {
            cx.background_executor().timer(SUBMIT_DELAY).await;
            terminal.update(cx, |terminal, _| terminal.input(ENTER))
        }
    })
    .detach_and_log_err(cx);
}

/// Presses Escape in the session, which interrupts Claude Code.
pub fn interrupt(terminal: &Entity<Terminal>, cx: &mut App) {
    terminal.update(cx, |terminal, _| terminal.input(ESCAPE));
}

/// Ends the session by killing its process; its terminal stays open.
pub fn stop(terminal: &Entity<Terminal>, cx: &mut App) {
    terminal.update(cx, |terminal, _| terminal.kill_active_task());
}

/// Brings the session's terminal to the front and focuses it, switching
/// window and project tab.
///
/// Must be called outside of an update of the session's window.
pub fn show_terminal(session: &RemoteSession, cx: &mut App) {
    let (Some(workspace), Some(terminal_view)) =
        (session.workspace.upgrade(), session.terminal_view.upgrade())
    else {
        return;
    };
    session
        .window
        .update(cx, |multi_workspace, window, cx| {
            window.activate_window();
            multi_workspace.activate(workspace.clone(), None, window, cx);
            workspace.update(cx, |workspace, cx| {
                let panel_pane = workspace.panel::<TerminalPanel>(cx).and_then(|panel| {
                    panel.read(cx).panes().into_iter().find_map(|pane| {
                        let index = pane.read(cx).index_for_item(&terminal_view)?;
                        Some((pane.clone(), index))
                    })
                });
                match panel_pane {
                    Some((pane, index)) => {
                        workspace.focus_panel::<TerminalPanel>(window, cx);
                        pane.update(cx, |pane, cx| {
                            pane.activate_item(index, true, true, window, cx);
                        });
                    }
                    None => {
                        workspace.activate_item(&terminal_view, true, true, window, cx);
                    }
                }
            });
        })
        .log_err();
}

#[cfg(test)]
#[path = "remote_sessions_tests.rs"]
mod tests;
