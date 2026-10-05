use std::path::Path;

use task::{SpawnInTerminal, TaskId};
use terminal::TaskStatus;
use terminal_view::terminal_panel::{CLAUDE_CODE_PROFILE, CLAUDE_REMOTE_CONTROL_PROFILE};

use super::*;

fn profile_task(name: &str) -> SpawnInTerminal {
    SpawnInTerminal {
        id: terminal_profile_task_id(name),
        full_label: name.to_string(),
        label: name.to_string(),
        ..Default::default()
    }
}

#[test]
fn test_only_remote_control_profile_tasks_are_sessions() {
    assert!(is_remote_control_task(&profile_task(
        CLAUDE_REMOTE_CONTROL_PROFILE
    )));
    assert!(!is_remote_control_task(&profile_task(CLAUDE_CODE_PROFILE)));
    assert!(!is_remote_control_task(&profile_task("PowerShell")));

    // A task of the user that merely carries the profile's name.
    let lookalike = SpawnInTerminal {
        id: TaskId("oneshot:1".to_string()),
        full_label: CLAUDE_REMOTE_CONTROL_PROFILE.to_string(),
        label: CLAUDE_REMOTE_CONTROL_PROFILE.to_string(),
        command: Some("claude".to_string()),
        args: vec!["--remote-control".to_string()],
        ..Default::default()
    };
    assert!(!is_remote_control_task(&lookalike));
}

#[test]
fn test_session_state() {
    let running = SessionState::from_task_status(TaskStatus::Running);
    assert_eq!(running, SessionState::Running);
    assert!(running.is_running());
    assert_eq!(running.label(), "Running");

    for status in [
        TaskStatus::Unknown,
        TaskStatus::Completed { success: true },
        TaskStatus::Completed { success: false },
    ] {
        let state = SessionState::from_task_status(status);
        assert_eq!(state, SessionState::Exited);
        assert!(!state.is_running());
        assert_eq!(state.label(), "Exited");
    }
}

#[test]
fn test_sessions_summary() {
    assert_eq!(sessions_summary(0, 0), "0 sessions · 0 running");
    assert_eq!(sessions_summary(1, 1), "1 session · 1 running");
    assert_eq!(sessions_summary(3, 1), "3 sessions · 1 running");
}

#[test]
fn test_working_directory_label() {
    let root = Path::new("/projects/app");
    assert_eq!(working_directory_label(None, Some(root)), None);
    assert_eq!(working_directory_label(Some(root), Some(root)), None);
    assert_eq!(
        working_directory_label(Some(Path::new("/projects/app/server")), Some(root)),
        Some("/projects/app/server".into())
    );
    assert_eq!(
        working_directory_label(Some(Path::new("/tmp")), None),
        Some("/tmp".into())
    );
}

#[test]
fn test_extract_remote_control_url() {
    assert_eq!(extract_remote_control_url(""), None);
    assert_eq!(
        extract_remote_control_url("See https://claude.ai/settings and https://example.com/code/1"),
        None
    );
    assert_eq!(
        extract_remote_control_url(
            "Remote Control is on\n  Continue at https://claude.ai/code/session_01AbC-dE_f?x=1.\n> "
        ),
        Some("https://claude.ai/code/session_01AbC-dE_f?x=1".into())
    );
    assert_eq!(
        extract_remote_control_url("│ https://claude.ai/code/session_1 │"),
        Some("https://claude.ai/code/session_1".into())
    );
    assert_eq!(
        extract_remote_control_url(
            "\u{1b}[2m\u{1b}[36mhttps://claude.ai/code/session_2\u{1b}[39m\u{1b}[22m (copied)"
        ),
        Some("https://claude.ai/code/session_2".into())
    );
    // The newest link wins, e.g. after `/remote-control` reconnected.
    assert_eq!(
        extract_remote_control_url(
            "https://claude.ai/code/session_old\nreconnected\nhttps://claude.ai/code/session_new"
        ),
        Some("https://claude.ai/code/session_new".into())
    );
    // Not put back together when Claude Code broke the line inside the link.
    assert_eq!(
        extract_remote_control_url("https://claude.ai/code/session_\n  abc"),
        Some("https://claude.ai/code/session_".into())
    );
}

#[test]
fn test_text_to_send() {
    assert_eq!(text_to_send(""), None);
    assert_eq!(text_to_send("   \t"), None);
    assert_eq!(text_to_send("/clear"), Some("/clear".to_string()));
    assert_eq!(
        text_to_send("fix the failing test"),
        Some("fix the failing test".to_string())
    );
    assert_eq!(
        text_to_send("run\u{1b}[A it\r\n"),
        Some("run[A it".to_string())
    );
}
