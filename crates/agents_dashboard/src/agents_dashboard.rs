//! OTerminal: the Agents dashboard.
//!
//! A live view of every Claude Code thread in the app (all project tabs and
//! windows, agent panel and Claude tabs alike): its status, current activity,
//! the tree of sub-agents it spawned (and theirs), workflows with their
//! phases, and the background commands it left running. See [`model`] for
//! what the Claude Code ACP adapter exposes and how it is interpreted.

mod dashboard;
pub mod model;
mod status_item;
pub mod store;

use gpui::{App, actions};
use workspace::Workspace;

pub use dashboard::AgentsDashboard;
pub use status_item::AgentsStatusItem;
pub use store::{AgentsStore, ThreadSnapshot};

actions!(
    agents_dashboard,
    [
        /// Opens the Agents dashboard: every Claude Code thread across project
        /// tabs and windows, with its sub-agents and background commands.
        Open
    ]
);

pub fn init(cx: &mut App) {
    store::init(cx);
    cx.observe_new(|workspace: &mut Workspace, _, _| {
        workspace.register_action(|workspace, _: &Open, window, cx| {
            AgentsDashboard::deploy(workspace, window, cx);
        });
    })
    .detach();
}
