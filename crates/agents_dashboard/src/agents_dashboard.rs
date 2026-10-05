//! OTerminal: the Agents dashboard.
//!
//! A live view of every Claude Code thread in the app (all project tabs and
//! windows, agent panel and Claude tabs alike): its status, current activity,
//! the tree of sub-agents it spawned (and theirs), workflows with their
//! phases, and the background commands it left running. See [`model`] for
//! what the Claude Code ACP adapter exposes and how it is interpreted.
//!
//! Also the Remote Control tab: the Claude Code Remote Control sessions running
//! in terminals, see [`remote_sessions`].

mod dashboard;
pub mod model;
mod remote_dashboard;
pub mod remote_sessions;
mod status_item;
pub mod store;

use gpui::{App, actions};
use terminal_view::terminal_panel::TerminalPanel;
use workspace::Workspace;

pub use dashboard::AgentsDashboard;
pub use remote_dashboard::RemoteControlDashboard;
pub use status_item::AgentsStatusItem;
pub use store::{AgentsStore, ThreadSnapshot};

actions!(
    agents_dashboard,
    [
        /// Opens the Agents dashboard: every Claude Code thread across project
        /// tabs and windows, with its sub-agents and background commands.
        Open,
        /// Opens the Remote Control tab: every Claude Code Remote Control session
        /// open in OTerminal.
        OpenRemoteControl,
        /// Starts a new Claude Code session with Remote Control in this project's
        /// terminal panel.
        NewRemoteControlSession
    ]
);

pub fn init(cx: &mut App) {
    store::init(cx);
    remote_sessions::init(cx);
    cx.observe_new(|workspace: &mut Workspace, _, _| {
        workspace.register_action(|workspace, _: &Open, window, cx| {
            AgentsDashboard::deploy(workspace, window, cx);
        });
        workspace.register_action(|workspace, _: &OpenRemoteControl, window, cx| {
            RemoteControlDashboard::deploy(workspace, window, cx);
        });
        workspace.register_action(|workspace, _: &NewRemoteControlSession, window, cx| {
            TerminalPanel::new_claude_remote_control_terminal(workspace, None, window, cx);
        });
    })
    .detach();
}
