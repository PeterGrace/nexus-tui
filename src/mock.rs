use std::path::PathBuf;

use crate::types::*;

pub fn mock_tree() -> Vec<TreeNode> {
    vec![
        TreeNode::Group(GroupNode {
            id: 1,
            name: "nexus".to_string(),
            icon: GroupIcon::Root,

            children: vec![
                TreeNode::Session(SessionSummary {
                    session_id: "a1b2c3d4-e5f6-7890-abcd-ef1234567890".to_string(),
                    display_name: "feat/scanner".to_string(),
                    cwd: Some(PathBuf::from("/Users/dev/Code/nexus")),
                    last_active: "2026-02-28T15:30:00Z".to_string(),
                    is_active: true,
                    status: SessionStatus::Active,
                    tmux_name: Some("a1b2c3d4-e5f6-7890-abcd-ef1234567890".to_string()),
                    created_by: SessionOrigin::Nexus,
                    created_at: "2026-02-28T10:00:00Z".to_string(),
                    claude_session_id: None,
                    worktree: None,
                    launch_command: "claude".to_string(),
                    jsonl_path: None,
                }),
                TreeNode::Session(SessionSummary {
                    session_id: "b2c3d4e5-f6a7-8901-bcde-f12345678901".to_string(),
                    display_name: "fix/render-tick".to_string(),
                    cwd: Some(PathBuf::from("/Users/dev/Code/nexus")),
                    last_active: "2026-02-27T09:15:00Z".to_string(),
                    is_active: false,
                    status: SessionStatus::Detached,
                    tmux_name: Some("b2c3d4e5-f6a7-8901-bcde-f12345678901".to_string()),
                    created_by: SessionOrigin::Nexus,
                    created_at: "2026-02-27T08:00:00Z".to_string(),
                    claude_session_id: None,
                    worktree: None,
                    launch_command: "claude".to_string(),
                    jsonl_path: None,
                }),
            ],
        }),
        TreeNode::Group(GroupNode {
            id: 2,
            name: "website".to_string(),
            icon: GroupIcon::Root,

            children: vec![
                TreeNode::Session(SessionSummary {
                    session_id: "c3d4e5f6-a7b8-9012-cdef-123456789012".to_string(),
                    display_name: "redesign-landing".to_string(),
                    cwd: Some(PathBuf::from("/Users/dev/Code/website")),
                    last_active: "2026-02-28T14:00:00Z".to_string(),
                    is_active: true,
                    status: SessionStatus::Active,
                    tmux_name: Some("c3d4e5f6-a7b8-9012-cdef-123456789012".to_string()),
                    created_by: SessionOrigin::Nexus,
                    created_at: "2026-02-28T12:00:00Z".to_string(),
                    claude_session_id: None,
                    worktree: None,
                    launch_command: "claude".to_string(),
                    jsonl_path: None,
                }),
                TreeNode::Group(GroupNode {
                    id: 3,
                    name: "api-work".to_string(),
                    icon: GroupIcon::SubGroup,

                    children: vec![TreeNode::Session(SessionSummary {
                        session_id: "d4e5f6a7-b8c9-0123-defa-234567890123".to_string(),
                        display_name: "api-auth-endpoints".to_string(),
                        cwd: Some(PathBuf::from("/Users/dev/Code/website")),
                        last_active: "2026-02-25T11:30:00Z".to_string(),
                        is_active: false,
                        status: SessionStatus::Dead,
                        tmux_name: None,
                        created_by: SessionOrigin::Scanner,
                        created_at: "2026-02-25T10:00:00Z".to_string(),
                        claude_session_id: None,
                        worktree: None,
                        launch_command: "claude".to_string(),
                        jsonl_path: None,
                    })],
                }),
            ],
        }),
        TreeNode::Group(GroupNode {
            id: 4,
            name: "Ungrouped".to_string(),
            icon: GroupIcon::Root,

            children: vec![SessionSummary {
                session_id: "e5f6a7b8-c9d0-1234-efab-345678901234".to_string(),
                display_name: "quick-question".to_string(),
                cwd: Some(PathBuf::from("/Users/dev")),
                last_active: "2026-02-20T08:00:00Z".to_string(),
                is_active: false,
                status: SessionStatus::Dead,
                tmux_name: None,
                created_by: SessionOrigin::Scanner,
                created_at: "2026-02-20T08:00:00Z".to_string(),
                claude_session_id: None,
                worktree: None,
                launch_command: "claude".to_string(),
                jsonl_path: None,
            }]
            .into_iter()
            .map(TreeNode::Session)
            .collect(),
        }),
    ]
}

pub fn mock_tmux_sessions() -> Vec<TmuxSessionInfo> {
    vec![
        TmuxSessionInfo {
            session_id: "a1b2c3d4-e5f6-7890-abcd-ef1234567890".to_string(),
            window_name: "nexus:feat-scanner".to_string(),
            is_active: true,
            status: TmuxSessionStatus::Running,
        },
        TmuxSessionInfo {
            session_id: "c3d4e5f6-a7b8-9012-cdef-123456789012".to_string(),
            window_name: "website:redesign".to_string(),
            is_active: false,
            status: TmuxSessionStatus::Idle,
        },
    ]
}
