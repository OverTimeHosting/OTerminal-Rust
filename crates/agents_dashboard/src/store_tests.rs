use std::path::Path;
use std::rc::Rc;

use acp_thread::{AgentConnection as _, StubAgentConnection};
use agent_client_protocol::schema::v1 as acp;
use gpui::TestAppContext;
use project::{FakeFs, Project};
use serde_json::json;
use settings::SettingsStore;
use util::{path, path_list::PathList};

use super::*;
use crate::model::{AgentNodeKind, BackgroundStatus};

fn init_test(cx: &mut TestAppContext) {
    cx.update(|cx| {
        let mut settings_store = SettingsStore::test(cx);
        settings_store.register_setting::<feature_flags::FeatureFlagsSettings>();
        cx.set_global(settings_store);
        super::init(cx);
    });
}

fn meta(value: serde_json::Value) -> acp::Meta {
    serde_json::from_value(value).expect("meta is an object")
}

/// A tool call as the Claude Code adapter reports it; `parent` is the
/// sub-agent (`_meta.claudeCode.parentToolUseId`).
fn tool_call(
    id: &str,
    name: &str,
    title: &str,
    kind: acp::ToolKind,
    input: serde_json::Value,
    parent: Option<&str>,
) -> acp::SessionUpdate {
    let mut claude_code = json!({ "toolName": name });
    if let Some(parent) = parent {
        claude_code["parentToolUseId"] = json!(parent);
    }
    acp::SessionUpdate::ToolCall(
        acp::ToolCall::new(id.to_string(), title.to_string())
            .name(name.to_string())
            .kind(kind)
            .status(acp::ToolCallStatus::InProgress)
            .raw_input(input)
            .meta(meta(json!({ "claudeCode": claude_code }))),
    )
}

fn completed(id: &str, output: &str, parent: Option<&str>) -> acp::SessionUpdate {
    let mut claude_code = json!({});
    if let Some(parent) = parent {
        claude_code["parentToolUseId"] = json!(parent);
    }
    acp::SessionUpdate::ToolCallUpdate(
        acp::ToolCallUpdate::new(
            id.to_string(),
            acp::ToolCallUpdateFields::new()
                .status(acp::ToolCallStatus::Completed)
                .content(vec![acp::ToolCallContent::Content(acp::Content::new(
                    output.to_string(),
                ))]),
        )
        .meta(meta(json!({ "claudeCode": claude_code }))),
    )
}

fn snapshot(store: &Entity<AgentsStore>, cx: &mut TestAppContext) -> ThreadSnapshot {
    cx.executor().advance_clock(REFRESH_DEBOUNCE * 2);
    cx.run_until_parked();
    store.read_with(cx, |store, _| {
        store
            .snapshots()
            .next()
            .cloned()
            .expect("the thread is tracked")
    })
}

#[gpui::test]
async fn test_tracks_threads_sub_agents_and_background(cx: &mut TestAppContext) {
    init_test(cx);
    let store = cx.update(|cx| AgentsStore::global(cx).expect("initialized"));
    let fs = FakeFs::new(cx.executor());
    let project = Project::test(fs, [], cx).await;
    let connection = Rc::new(StubAgentConnection::new());
    let thread = cx
        .update(|cx| {
            connection
                .clone()
                .new_session(project, PathList::new(&[Path::new(path!("/test"))]), cx)
        })
        .await
        .unwrap();
    let session_id = thread.read_with(cx, |thread, _| thread.session_id().clone());

    // Picked up on creation, but an empty thread isn't listed.
    let initial = snapshot(&store, cx);
    assert_eq!(initial.state, ThreadState::Idle);
    assert!(!initial.is_listed());

    let _turn = thread.update(cx, |thread, cx| thread.send(vec!["Fix the bug".into()], cx));
    cx.run_until_parked();

    let send = |update: acp::SessionUpdate, cx: &mut TestAppContext| {
        cx.update(|cx| connection.send_update(session_id.clone(), update, cx));
    };
    send(
        tool_call(
            "agent-1",
            "Agent",
            "Explore the parser",
            acp::ToolKind::Think,
            json!({"description": "Explore the parser", "prompt": "Look around", "subagent_type": "Explore"}),
            None,
        ),
        cx,
    );
    send(
        tool_call(
            "read-1",
            "Read",
            "Read src/parser.rs",
            acp::ToolKind::Read,
            json!({"file_path": "src/parser.rs"}),
            Some("agent-1"),
        ),
        cx,
    );
    // A sub-agent of the sub-agent.
    send(
        tool_call(
            "agent-2",
            "Agent",
            "Check the tests",
            acp::ToolKind::Think,
            json!({"description": "Check the tests", "prompt": "…", "subagent_type": "general-purpose"}),
            Some("agent-1"),
        ),
        cx,
    );
    send(
        tool_call(
            "bash-1",
            "Bash",
            "cargo test",
            acp::ToolKind::Execute,
            json!({"command": "cargo test"}),
            Some("agent-2"),
        ),
        cx,
    );
    // The main agent starts a dev server in the background.
    send(
        tool_call(
            "dev",
            "Bash",
            "npm run dev",
            acp::ToolKind::Execute,
            json!({"command": "npm run dev", "run_in_background": true}),
            None,
        ),
        cx,
    );
    send(
        completed(
            "dev",
            "Command running in background with ID: b7. Output is being written to: /tmp/tasks/b7.output. You will be notified when it completes.",
            None,
        ),
        cx,
    );

    let live = snapshot(&store, cx);
    assert_eq!(live.state, ThreadState::Generating);
    assert!(live.is_listed());
    assert!(live.turn_started_at.is_some());
    assert_eq!(live.tool_calls, 5);

    let root = &live.root;
    assert_eq!(root.kind, AgentNodeKind::Main);
    assert_eq!(root.children.len(), 1);
    let explorer = &root.children[0];
    assert_eq!(explorer.label.as_ref(), "Explore the parser");
    assert_eq!(explorer.subagent_type.as_deref(), Some("Explore"));
    assert_eq!(explorer.tool_calls, 1);
    assert!(explorer.status.is_active());
    assert!(explorer.timing.started_at.is_some());
    let checker = &explorer.children[0];
    assert_eq!(checker.label.as_ref(), "Check the tests");
    assert_eq!(
        checker
            .activity
            .as_ref()
            .map(|activity| activity.text.as_ref()),
        Some("Running cargo test")
    );

    assert_eq!(live.background.len(), 1);
    let dev = &live.background[0];
    assert_eq!(dev.task_id.as_deref(), Some("b7"));
    assert_eq!(
        dev.output_path.as_deref(),
        Some(Path::new("/tmp/tasks/b7.output"))
    );
    assert_eq!(dev.status, BackgroundStatus::Running);

    let counts = store.read_with(cx, |store, _| store.running_counts(Instant::now()));
    assert_eq!(
        counts,
        (3, 1),
        "thread + 2 sub-agents, 1 background command"
    );

    // The inner sub-agent finishes, the model stops the dev server.
    send(completed("bash-1", "ok", Some("agent-2")), cx);
    send(
        completed("agent-2", "Tests pass.\nAll 12 green.", Some("agent-1")),
        cx,
    );
    send(
        tool_call(
            "stop",
            "TaskStop",
            "TaskStop",
            acp::ToolKind::Other,
            json!({"task_id": "b7"}),
            None,
        ),
        cx,
    );
    send(completed("stop", "Stopped", None), cx);

    let later = snapshot(&store, cx);
    let checker = &later.root.children[0].children[0];
    assert_eq!(checker.status, NodeStatus::Completed);
    assert_eq!(checker.last_output.as_deref(), Some("All 12 green."));
    assert!(checker.timing.finished_at.is_some());
    assert_eq!(later.background[0].status, BackgroundStatus::Stopped);
    let counts = store.read_with(cx, |store, _| store.running_counts(Instant::now()));
    assert_eq!(counts, (2, 0));

    // The turn ends: the unfinished sub-agent was cut off.
    connection.end_turn(session_id.clone(), acp::StopReason::EndTurn);
    let done = snapshot(&store, cx);
    assert_eq!(done.state, ThreadState::Finished);
    assert!(done.turn_ended_at.is_some());
    assert_eq!(done.root.children[0].status, NodeStatus::Canceled);
    let counts = store.read_with(cx, |store, _| store.running_counts(Instant::now()));
    assert_eq!(counts, (0, 0));

    // Released threads disappear.
    cx.update(|_| drop(thread));
    cx.executor().advance_clock(REFRESH_DEBOUNCE * 2);
    cx.run_until_parked();
    assert_eq!(store.read_with(cx, |store, _| store.snapshots().count()), 0);
}

#[gpui::test]
async fn test_permission_request_marks_thread_waiting(cx: &mut TestAppContext) {
    init_test(cx);
    let store = cx.update(|cx| AgentsStore::global(cx).expect("initialized"));
    let fs = FakeFs::new(cx.executor());
    let project = Project::test(fs, [], cx).await;
    let connection = Rc::new(StubAgentConnection::new());
    let thread = cx
        .update(|cx| {
            connection
                .clone()
                .new_session(project, PathList::new(&[Path::new(path!("/test"))]), cx)
        })
        .await
        .unwrap();
    let session_id = thread.read_with(cx, |thread, _| thread.session_id().clone());
    let _turn = thread.update(cx, |thread, cx| thread.send(vec!["Edit it".into()], cx));
    cx.run_until_parked();

    cx.update(|cx| {
        connection.send_update(
            session_id.clone(),
            tool_call(
                "agent-1",
                "Agent",
                "Refactor",
                acp::ToolKind::Think,
                json!({"description": "Refactor", "prompt": "…"}),
                None,
            ),
            cx,
        )
    });
    let edit = acp::ToolCall::new("edit-1".to_string(), "Edit src/lib.rs".to_string())
        .name("Edit".to_string())
        .kind(acp::ToolKind::Edit)
        .meta(meta(
            json!({"claudeCode": {"toolName": "Edit", "parentToolUseId": "agent-1"}}),
        ));
    let _authorization = thread.update(cx, |thread, cx| {
        thread.request_tool_call_authorization(
            edit.into(),
            acp_thread::PermissionOptions::Flat(vec![acp::PermissionOption::new(
                "allow".to_string(),
                "Allow",
                acp::PermissionOptionKind::AllowOnce,
            )]),
            acp_thread::AuthorizationKind::PermissionGrant,
            cx,
        )
    });

    let waiting = snapshot(&store, cx);
    assert_eq!(waiting.state, ThreadState::WaitingForPermission);
    assert_eq!(waiting.edits, 1);
    let agent = &waiting.root.children[0];
    assert_eq!(agent.status, NodeStatus::WaitingForPermission);
    assert_eq!(agent.edits, 1);
    assert_eq!(waiting.root.status, NodeStatus::WaitingForPermission);
}
