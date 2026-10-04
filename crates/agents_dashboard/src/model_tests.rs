use super::*;
use agent_client_protocol::schema::v1::ToolKind;
use serde_json::json;

fn record(
    id: &str,
    parent: Option<&str>,
    entry_ix: usize,
    role: CallRole,
    status: NodeStatus,
) -> CallRecord {
    CallRecord {
        id: id.to_string().into(),
        parent_id: parent.map(|parent| parent.to_string().into()),
        entry_ix,
        title: id.to_string().into(),
        role,
        status,
        last_output_line: None,
        background_task: None,
        terminal: None,
    }
}

fn agent(background: bool) -> CallRole {
    CallRole::SubAgent {
        subagent_type: Some("general-purpose".into()),
        name: None,
        background,
    }
}

fn shell(command: &str, background: bool) -> CallRole {
    CallRole::Shell {
        command: command.to_string().into(),
        description: None,
        background,
    }
}

fn generating() -> TreeContext {
    TreeContext {
        generating: true,
        last_user_message_ix: Some(0),
    }
}

#[test]
fn builds_nested_sub_agent_tree() {
    let records = vec![
        record("read-1", None, 1, CallRole::Other, NodeStatus::Completed),
        record("agent-a", None, 2, agent(false), NodeStatus::Running),
        record("agent-b", None, 3, agent(false), NodeStatus::Completed),
        record(
            "a-edit",
            Some("agent-a"),
            4,
            CallRole::Edit,
            NodeStatus::Completed,
        ),
        record(
            "agent-a1",
            Some("agent-a"),
            5,
            agent(false),
            NodeStatus::Running,
        ),
        record(
            "a1-bash",
            Some("agent-a1"),
            6,
            shell("npm test", false),
            NodeStatus::Running,
        ),
        record(
            "b-read",
            Some("agent-b"),
            7,
            CallRole::Other,
            NodeStatus::Completed,
        ),
        // Parent unknown (e.g. history cut): belongs to the main agent.
        record(
            "orphan",
            Some("gone"),
            8,
            CallRole::Other,
            NodeStatus::Completed,
        ),
    ];
    let tree = build_tree(
        &records,
        &HashMap::default(),
        NodeStatus::Running,
        "Main".into(),
        generating(),
    );

    assert_eq!(tree.kind, AgentNodeKind::Main);
    assert_eq!(tree.tool_calls, 2, "read-1 and the orphan");
    assert_eq!(tree.children.len(), 2);
    assert_eq!(tree.descendant_count(), 3);

    let a = &tree.children[0];
    assert_eq!(a.id.as_deref(), Some("agent-a"));
    assert_eq!(a.status, NodeStatus::Running);
    assert_eq!(a.tool_calls, 1);
    assert_eq!(a.edits, 1);
    assert_eq!(a.children.len(), 1);

    let a1 = &a.children[0];
    assert_eq!(a1.id.as_deref(), Some("agent-a1"));
    let activity = a1.activity.as_ref().expect("a1 has an activity");
    assert_eq!(activity.text.as_ref(), "Running npm test");
    assert_eq!(activity.entry_ix, 6);
    assert_eq!(activity.status, NodeStatus::Running);

    let b = &tree.children[1];
    assert_eq!(b.status, NodeStatus::Completed);
    assert_eq!(b.tool_calls, 1);

    assert_eq!(tree.active_descendants(), 2, "agent-a and agent-a1");
}

#[test]
fn waiting_for_permission_bubbles_up() {
    let records = vec![
        record("agent-a", None, 1, agent(false), NodeStatus::Running),
        record(
            "agent-a1",
            Some("agent-a"),
            2,
            agent(false),
            NodeStatus::Running,
        ),
        record(
            "edit",
            Some("agent-a1"),
            3,
            CallRole::Edit,
            NodeStatus::WaitingForPermission,
        ),
    ];
    let tree = build_tree(
        &records,
        &HashMap::default(),
        NodeStatus::Running,
        "Main".into(),
        generating(),
    );
    assert_eq!(tree.status, NodeStatus::WaitingForPermission);
    assert_eq!(tree.children[0].status, NodeStatus::WaitingForPermission);
    assert_eq!(
        tree.children[0].children[0].status,
        NodeStatus::WaitingForPermission
    );
}

#[test]
fn background_sub_agents_run_while_their_turn_is_open() {
    let records = vec![
        // Spawned in an earlier turn (before the user message at 5).
        record("old", None, 2, agent(true), NodeStatus::Completed),
        record("new", None, 6, agent(true), NodeStatus::Completed),
        record("sync", None, 7, agent(false), NodeStatus::Completed),
    ];
    let context = TreeContext {
        generating: true,
        last_user_message_ix: Some(5),
    };
    let tree = build_tree(
        &records,
        &HashMap::default(),
        NodeStatus::Running,
        "M".into(),
        context,
    );
    assert_eq!(tree.children[0].status, NodeStatus::Completed);
    assert_eq!(tree.children[1].status, NodeStatus::Running);
    assert!(tree.children[1].background);
    assert_eq!(tree.children[2].status, NodeStatus::Completed);

    // Once the turn ends, every sub-agent is done; unfinished calls were cut off.
    let records = vec![
        record("new", None, 6, agent(true), NodeStatus::Completed),
        record("cut", None, 7, agent(false), NodeStatus::Running),
    ];
    let idle = TreeContext {
        generating: false,
        last_user_message_ix: Some(5),
    };
    let tree = build_tree(
        &records,
        &HashMap::default(),
        NodeStatus::Completed,
        "M".into(),
        idle,
    );
    assert_eq!(tree.children[0].status, NodeStatus::Completed);
    assert_eq!(tree.children[1].status, NodeStatus::Canceled);
    assert_eq!(tree.active_descendants(), 0);
}

#[test]
fn completed_spawn_with_running_children_is_running() {
    let records = vec![
        record("agent", None, 1, agent(false), NodeStatus::Completed),
        record(
            "child",
            Some("agent"),
            2,
            CallRole::Other,
            NodeStatus::Running,
        ),
        record(
            "nested",
            Some("agent"),
            3,
            agent(false),
            NodeStatus::Running,
        ),
    ];
    let tree = build_tree(
        &records,
        &HashMap::default(),
        NodeStatus::Running,
        "M".into(),
        generating(),
    );
    assert_eq!(tree.children[0].status, NodeStatus::Running);
}

#[test]
fn workflow_node_carries_phases() {
    let script = r#"export const meta = {
        name: 'spec',
        description: "Write, review, fix",
        phases: [
            'Draft',
            { title: "Review, carefully", agents: 3 },
            { name: `Fix` },
        ],
    }
    await phase('Draft', () => agent('write it'))"#;
    let role = call_role(
        Some("Workflow"),
        ToolKind::Other,
        Some(&json!({ "script": script })),
    );
    assert_eq!(
        role,
        CallRole::Workflow {
            name: Some("spec".into()),
            phases: vec!["Draft".into(), "Review, carefully".into(), "Fix".into()],
        }
    );

    let records = vec![
        record("wf", None, 1, role, NodeStatus::Completed),
        record(
            "wf-agent-1",
            Some("wf"),
            2,
            agent(false),
            NodeStatus::Running,
        ),
        record(
            "wf-agent-2",
            Some("wf"),
            3,
            agent(false),
            NodeStatus::Completed,
        ),
    ];
    let tree = build_tree(
        &records,
        &HashMap::default(),
        NodeStatus::Running,
        "M".into(),
        generating(),
    );
    let workflow = &tree.children[0];
    assert_eq!(workflow.kind, AgentNodeKind::Workflow);
    assert_eq!(workflow.label.as_ref(), "Workflow: spec");
    assert_eq!(workflow.phases.len(), 3);
    assert_eq!(workflow.children.len(), 2);
    assert_eq!(workflow.status, NodeStatus::Running);
}

#[test]
fn workflow_meta_edge_cases() {
    assert_eq!(parse_workflow_meta(""), (None, vec![]));
    assert_eq!(
        parse_workflow_meta("export const meta = { name: \"x\" }"),
        (Some("x".into()), vec![])
    );
    // Brackets inside strings don't confuse the parser.
    let (name, phases) =
        parse_workflow_meta("const meta = { name: 'a]b', phases: ['one [1]', \"two}\"] }");
    assert_eq!(name.as_deref(), Some("a]b"));
    assert_eq!(phases, vec![SharedString::from("one [1]"), "two}".into()]);
}

#[test]
fn classifies_calls() {
    assert_eq!(
        call_role(
            Some("Agent"),
            ToolKind::Think,
            Some(
                &json!({"description": "Find bugs", "prompt": "…", "subagent_type": "Explore", "run_in_background": true})
            ),
        ),
        CallRole::SubAgent {
            subagent_type: Some("Explore".into()),
            name: None,
            background: true
        }
    );
    assert_eq!(
        call_role(
            Some("Bash"),
            ToolKind::Execute,
            Some(&json!({"command": "npm run dev", "run_in_background": true})),
        ),
        CallRole::Shell {
            command: "npm run dev".into(),
            description: None,
            background: true
        }
    );
    assert_eq!(
        call_role(
            Some("TaskStop"),
            ToolKind::Other,
            Some(&json!({"shell_id": "b12"}))
        ),
        CallRole::StopTask {
            task_id: Some("b12".into())
        }
    );
    assert_eq!(
        call_role(Some("Write"), ToolKind::Edit, None),
        CallRole::Edit
    );
    assert_eq!(call_role(None, ToolKind::Edit, None), CallRole::Edit);
    assert_eq!(
        call_role(Some("Read"), ToolKind::Read, None),
        CallRole::Other
    );
}

#[test]
fn parses_background_launch_text() {
    let text = "Command running in background with ID: bx7k2p. Output is being written to: C:\\Users\\me\\AppData\\Local\\Temp\\claude\\tasks\\bx7k2p.output. You will be notified when it completes.";
    assert_eq!(
        parse_background_launch(text),
        Some(BackgroundTaskRef {
            task_id: "bx7k2p".into(),
            output_path: Some(PathBuf::from(
                "C:\\Users\\me\\AppData\\Local\\Temp\\claude\\tasks\\bx7k2p.output"
            )),
        })
    );
    assert_eq!(parse_background_launch("total 0\nfoo"), None);
}

#[test]
fn detects_urls_and_ports() {
    assert_eq!(
        detect_url(&[
            "compiling…",
            "  ➜  Local:   http://localhost:5173/",
            "  ➜  Network: use --host"
        ])
        .as_deref(),
        Some("http://localhost:5173/")
    );
    assert_eq!(
        detect_url(&["Server listening on port 8080"]).as_deref(),
        Some("http://localhost:8080")
    );
    assert_eq!(
        detect_url(&[
            "\u{1b}[32mready\u{1b}[0m - started server on 0.0.0.0:3000, url: http://0.0.0.0:3000"
        ])
        .as_deref(),
        Some("http://localhost:3000")
    );
    assert_eq!(detect_url(&["nothing here"]), None);
}

#[test]
fn output_preview_is_plain_text_of_the_last_lines() {
    assert_eq!(output_preview(""), None);
    assert_eq!(output_preview("\n  \n\x1b[0m\n"), None);
    assert_eq!(
        output_preview(
            "\n\x1b[32mready\x1b[0m in 120ms  \r\n\nbuilding 10%\rbuilding 100%\n  indented\n\n"
        )
        .as_deref(),
        Some("ready in 120ms\n\nbuilding 100%\n  indented")
    );

    let long = (0..PREVIEW_LINES + 50)
        .map(|line| format!("line {line}"))
        .collect::<Vec<_>>()
        .join("\n");
    let preview = output_preview(&long).unwrap_or_default();
    assert_eq!(preview.lines().count(), PREVIEW_LINES);
    assert_eq!(preview.lines().next(), Some("line 50"));
    assert_eq!(
        preview.lines().last(),
        Some(format!("line {}", PREVIEW_LINES + 49).as_str())
    );
}

#[test]
fn background_items_and_counts() {
    let now = Instant::now();
    let mut dev = record(
        "dev",
        None,
        1,
        shell("npm run dev", true),
        NodeStatus::Completed,
    );
    dev.background_task = Some(BackgroundTaskRef {
        task_id: "b1".into(),
        output_path: Some(PathBuf::from("/tmp/b1.output")),
    });
    let mut watcher = record(
        "watch",
        Some("agent"),
        3,
        shell("tsc -w", true),
        NodeStatus::Completed,
    );
    watcher.background_task = Some(BackgroundTaskRef {
        task_id: "b2".into(),
        output_path: None,
    });
    let stop = record(
        "stop",
        None,
        4,
        CallRole::StopTask {
            task_id: Some("b2".into()),
        },
        NodeStatus::Completed,
    );
    let monitor = record(
        "mon",
        None,
        5,
        CallRole::Monitor {
            description: Some("CI".into()),
            command: Some("gh run watch".into()),
            timeout: Some(Duration::from_secs(60)),
        },
        NodeStatus::Completed,
    );
    let foreground = record("ls", None, 6, shell("ls", false), NodeStatus::Completed);
    let records = vec![
        dev,
        record("agent", None, 2, agent(false), NodeStatus::Running),
        watcher,
        stop,
        monitor,
        foreground,
    ];

    let mut timings = HashMap::default();
    timings.insert(
        SharedString::from("mon"),
        CallTiming {
            started_at: Some(now),
            finished_at: Some(now),
        },
    );
    let mut tails = HashMap::default();
    tails.insert(
        PathBuf::from("/tmp/b1.output"),
        OutputTail {
            output: Some("vite ready\nLocal: http://localhost:5173/".into()),
            last_line: Some("Local: http://localhost:5173/".into()),
            url: Some("http://localhost:5173/".into()),
            modified_at: None,
        },
    );

    let items = build_background(&records, &timings, &tails);
    assert_eq!(items.len(), 3, "dev, watcher and monitor");
    assert_eq!(items[0].status, BackgroundStatus::Running);
    assert_eq!(items[0].url.as_deref(), Some("http://localhost:5173/"));
    assert_eq!(
        items[0].output.as_deref(),
        Some("vite ready\nLocal: http://localhost:5173/")
    );
    assert_eq!(items[1].status, BackgroundStatus::Stopped);
    assert_eq!(items[1].owner.as_deref(), Some("agent"));
    assert_eq!(items[2].kind, BackgroundKind::Monitor);
    assert_eq!(items[2].status_at(now), BackgroundStatus::Running);
    assert_eq!(
        items[2].status_at(now + Duration::from_secs(61)),
        BackgroundStatus::Expired
    );

    let tree = build_tree(
        &records,
        &timings,
        NodeStatus::Running,
        "M".into(),
        generating(),
    );
    let state = ThreadState::Generating;
    let (agents, background) = running_counts([(&state, &tree, items.as_slice())], now);
    assert_eq!(agents, 2, "the thread and its running sub-agent");
    assert_eq!(background, 2, "dev server and monitor");

    let idle = ThreadState::Finished;
    let (agents, _) = running_counts([(&idle, &tree, items.as_slice())], now);
    assert_eq!(agents, 0);
}

#[test]
fn thread_state_priority() {
    assert_eq!(
        thread_state(true, true, false, false),
        ThreadState::WaitingForPermission
    );
    assert_eq!(
        thread_state(true, false, true, true),
        ThreadState::Generating
    );
    assert_eq!(thread_state(false, false, true, true), ThreadState::Error);
    assert_eq!(
        thread_state(false, false, false, true),
        ThreadState::Finished
    );
    assert_eq!(thread_state(false, false, false, false), ThreadState::Idle);
}

#[test]
fn formats() {
    assert_eq!(format_duration(Duration::from_secs(4)), "4s");
    assert_eq!(format_duration(Duration::from_secs(65)), "1m 05s");
    assert_eq!(format_duration(Duration::from_secs(3720)), "1h 02m");
    assert_eq!(format_tokens(950), "950");
    assert_eq!(format_tokens(45_200), "45.2k");
    assert_eq!(format_tokens(1_260_000), "1.3M");
    assert_eq!(
        last_line("a\n\u{1b}[1mb\u{1b}[0m\n\n").as_deref(),
        Some("b")
    );
}
