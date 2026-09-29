//! OTerminal: this project's Claude Code threads, for the agent panel's
//! header dropdown and its searchable "View All…" picker.

use std::{cmp, path::Path, sync::Arc};

use acp_thread::ThreadStatus;
use chrono::{DateTime, Utc};
use fuzzy::{StringMatch, StringMatchCandidate};
use gpui::{
    App, Context, DismissEvent, Entity, EventEmitter, FocusHandle, Focusable, IntoElement,
    ParentElement, Render, Subscription, Task, WeakEntity, Window, rems,
};
use picker::{Picker, PickerDelegate};
use project::Project;
use ui::{HighlightedLabel, Indicator, ListItem, ListItemSpacing, prelude::*};
use util::ResultExt as _;
use workspace::{ModalView, Workspace};

use crate::thread_metadata_store::{ThreadId, ThreadMetadata, ThreadMetadataStore};
use crate::{Agent, ConversationView};

/// What an open thread is doing, for the status dot next to it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ThreadActivity {
    Idle,
    Running,
    NeedsInput,
    /// Finished (or needs input) while the user wasn't looking.
    Unseen,
}

impl ThreadActivity {
    pub(crate) fn of(view: &ConversationView, cx: &App) -> Self {
        let Some(thread) = view.root_thread(cx) else {
            return Self::Idle;
        };
        let thread = thread.read(cx);
        if thread.is_waiting_for_confirmation() {
            Self::NeedsInput
        } else if thread.status() == ThreadStatus::Generating {
            Self::Running
        } else if view.has_unseen_activity() {
            Self::Unseen
        } else {
            Self::Idle
        }
    }

    pub(crate) fn label(self) -> Option<&'static str> {
        match self {
            Self::Idle => None,
            Self::Running => Some("running"),
            Self::NeedsInput => Some("needs input"),
            Self::Unseen => Some("new"),
        }
    }

    pub(crate) fn indicator(self) -> Option<Indicator> {
        match self {
            Self::Idle => None,
            Self::Running => Some(Indicator::dot().color(Color::Accent)),
            Self::NeedsInput => Some(Indicator::dot().color(Color::Warning)),
            Self::Unseen => Some(Indicator::dot().color(Color::Success)),
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) struct ProjectThread {
    pub metadata: ThreadMetadata,
    pub activity: ThreadActivity,
    pub is_current: bool,
}

/// The sent (non-draft), unarchived, local threads that belong to one of
/// `worktree_paths`, most recently updated first.
pub(crate) fn threads_for_paths<'a>(
    entries: impl Iterator<Item = &'a ThreadMetadata>,
    worktree_paths: &[&Path],
) -> Vec<ThreadMetadata> {
    let mut threads = entries
        .filter(|thread| {
            !thread.archived
                && !thread.is_draft()
                && thread.remote_connection.is_none()
                && worktree_paths
                    .iter()
                    .any(|path| thread.references_folder_path(path))
        })
        .cloned()
        .collect::<Vec<_>>();
    threads.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
    threads
}

/// This project's threads with their live status. `open_views` are the
/// conversations currently loaded (agent panel and Claude Code tabs).
pub(crate) fn project_threads(
    project: &Entity<Project>,
    open_views: &[Entity<ConversationView>],
    current: Option<ThreadId>,
    cx: &App,
) -> Vec<ProjectThread> {
    let Some(store) = ThreadMetadataStore::try_global(cx) else {
        return Vec::new();
    };
    let worktrees = project
        .read(cx)
        .visible_worktrees(cx)
        .map(|worktree| worktree.read(cx).abs_path())
        .collect::<Vec<_>>();
    let worktree_paths = worktrees.iter().map(|path| path.as_ref()).collect::<Vec<_>>();
    threads_for_paths(store.read(cx).entries(), &worktree_paths)
        .into_iter()
        .map(|metadata| {
            let activity = open_views
                .iter()
                .find(|view| view.read(cx).thread_id == metadata.thread_id)
                .map_or(ThreadActivity::Idle, |view| {
                    ThreadActivity::of(view.read(cx), cx)
                });
            ProjectThread {
                is_current: Some(metadata.thread_id) == current,
                metadata,
                activity,
            }
        })
        .collect()
}

/// Compact relative time ("5m", "3h", "2d").
pub(crate) fn relative_time(time: DateTime<Utc>) -> String {
    crate::threads_archive_view::format_history_entry_timestamp(time)
}

/// Opens a thread chosen from the dropdown or picker: focuses its Claude Code
/// tab when it has one, otherwise shows it in the agent panel.
pub(crate) fn open_thread(
    workspace: &Entity<Workspace>,
    metadata: &ThreadMetadata,
    window: &mut Window,
    cx: &mut App,
) {
    ConversationView::reveal_thread(
        workspace,
        Agent::from(metadata.agent_id.clone()),
        metadata.thread_id,
        Some(metadata.folder_paths().clone()),
        metadata.title(),
        window,
        cx,
    );
}

/// "View All…": a searchable modal over this project's threads.
pub struct ThreadPicker {
    picker: Entity<Picker<ThreadPickerDelegate>>,
    _subscription: Subscription,
}

impl ThreadPicker {
    pub(crate) fn toggle(
        workspace: &mut Workspace,
        threads: Vec<ProjectThread>,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) {
        let weak_workspace = cx.weak_entity();
        workspace.toggle_modal(window, cx, move |window, cx| {
            let delegate = ThreadPickerDelegate {
                workspace: weak_workspace,
                threads,
                matches: Vec::new(),
                selected_index: 0,
            };
            let picker = cx.new(|cx| {
                Picker::uniform_list(delegate, window, cx).initial_width(rems(34.))
            });
            let _subscription = cx.subscribe(&picker, |_, _, _, cx| cx.emit(DismissEvent));
            Self {
                picker,
                _subscription,
            }
        });
    }
}

impl ModalView for ThreadPicker {}
impl EventEmitter<DismissEvent> for ThreadPicker {}

impl Focusable for ThreadPicker {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.picker.focus_handle(cx)
    }
}

impl Render for ThreadPicker {
    fn render(&mut self, _: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        v_flex().key_context("ClaudeThreadPicker").child(self.picker.clone())
    }
}

pub struct ThreadPickerDelegate {
    workspace: WeakEntity<Workspace>,
    threads: Vec<ProjectThread>,
    matches: Vec<StringMatch>,
    selected_index: usize,
}

impl ThreadPickerDelegate {
    /// The thread behind the `ix`th visible match.
    pub(crate) fn thread_at(&self, ix: usize) -> Option<&ProjectThread> {
        self.threads.get(self.matches.get(ix)?.candidate_id)
    }
}

impl PickerDelegate for ThreadPickerDelegate {
    type ListItem = ListItem;

    fn name() -> &'static str {
        "claude thread picker"
    }

    fn placeholder_text(&self, _window: &mut Window, _cx: &mut App) -> Arc<str> {
        "Search this project's threads\u{2026}".into()
    }

    fn match_count(&self) -> usize {
        self.matches.len()
    }

    fn selected_index(&self) -> usize {
        self.selected_index
    }

    fn set_selected_index(&mut self, ix: usize, _: &mut Window, _: &mut Context<Picker<Self>>) {
        self.selected_index = ix;
    }

    fn update_matches(
        &mut self,
        query: String,
        window: &mut Window,
        cx: &mut Context<Picker<Self>>,
    ) -> Task<()> {
        let candidates = self
            .threads
            .iter()
            .enumerate()
            .map(|(ix, thread)| {
                StringMatchCandidate::new(ix, thread.metadata.display_title().as_ref())
            })
            .collect::<Vec<_>>();
        cx.spawn_in(window, async move |picker, cx| {
            let matches = if query.is_empty() {
                candidates
                    .into_iter()
                    .map(|candidate| StringMatch {
                        candidate_id: candidate.id,
                        string: candidate.string,
                        positions: Vec::new(),
                        score: 0.0,
                    })
                    .collect()
            } else {
                fuzzy::match_strings(
                    &candidates,
                    &query,
                    true,
                    true,
                    10000,
                    &Default::default(),
                    cx.background_executor().clone(),
                )
                .await
            };
            picker
                .update(cx, |picker, _| {
                    let delegate = &mut picker.delegate;
                    delegate.matches = matches;
                    delegate.selected_index = cmp::min(
                        delegate.selected_index,
                        delegate.matches.len().saturating_sub(1),
                    );
                })
                .log_err();
        })
    }

    fn confirm(&mut self, _secondary: bool, window: &mut Window, cx: &mut Context<Picker<Self>>) {
        let Some(thread) = self.thread_at(self.selected_index).cloned() else {
            return;
        };
        cx.emit(DismissEvent);
        if let Some(workspace) = self.workspace.upgrade() {
            window.defer(cx, move |window, cx| {
                open_thread(&workspace, &thread.metadata, window, cx);
            });
        }
    }

    fn dismissed(&mut self, _: &mut Window, cx: &mut Context<Picker<Self>>) {
        cx.emit(DismissEvent);
    }

    fn render_match(
        &self,
        ix: usize,
        selected: bool,
        _window: &mut Window,
        _cx: &mut Context<Picker<Self>>,
    ) -> Option<Self::ListItem> {
        let hit = self.matches.get(ix)?;
        let thread = self.threads.get(hit.candidate_id)?;
        let activity = thread.activity;
        Some(
            ListItem::new(ix)
                .inset(true)
                .spacing(ListItemSpacing::Sparse)
                .toggle_state(selected)
                .start_slot::<Indicator>(activity.indicator())
                .child(
                    h_flex()
                        .w_full()
                        .gap_2()
                        .justify_between()
                        .child(HighlightedLabel::new(
                            hit.string.clone(),
                            hit.positions.clone(),
                        ))
                        .child(
                            h_flex()
                                .gap_1()
                                .flex_none()
                                .when_some(activity.label(), |this, label| {
                                    this.child(
                                        Label::new(label).size(LabelSize::Small).color(Color::Muted),
                                    )
                                })
                                .child(
                                    Label::new(relative_time(thread.metadata.updated_at))
                                        .size(LabelSize::Small)
                                        .color(Color::Muted),
                                ),
                        ),
                ),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone as _;
    use project::{AgentId, WorktreePaths};
    use util::path_list::PathList;

    fn thread(title: &str, folder: &str, minute: u32, draft: bool) -> ThreadMetadata {
        ThreadMetadata {
            thread_id: ThreadId::new(),
            session_id: (!draft).then(|| agent_client_protocol::schema::v1::SessionId::new(title)),
            agent_id: AgentId::new("claude-acp"),
            title: Some(title.to_string().into()),
            title_override: None,
            updated_at: Utc.with_ymd_and_hms(2026, 9, 1, 12, minute, 0).unwrap(),
            created_at: None,
            interacted_at: None,
            worktree_paths: WorktreePaths::from_folder_paths(&PathList::new(&[Path::new(folder)])),
            remote_connection: None,
            archived: false,
        }
    }

    #[test]
    fn dropdown_lists_this_projects_threads_most_recent_first() {
        let root = if cfg!(windows) { "C:\\repo" } else { "/repo" };
        let other = if cfg!(windows) { "C:\\other" } else { "/other" };
        let mut archived = thread("Archived", root, 50, false);
        archived.archived = true;
        let entries = vec![
            thread("Older", root, 1, false),
            thread("Newest", root, 30, false),
            thread("Other project", other, 40, false),
            thread("Draft", root, 45, true),
            archived,
        ];
        let threads = threads_for_paths(entries.iter(), &[Path::new(root)]);
        let titles = threads
            .iter()
            .map(|thread| thread.display_title().to_string())
            .collect::<Vec<_>>();
        assert_eq!(titles, vec!["Newest", "Older"]);
    }
}
