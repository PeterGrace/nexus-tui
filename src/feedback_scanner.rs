//! Background scanner for Claude Code halt-state detection.
//!
//! Polls all live tmux sessions every ~2 seconds using lightweight text-only
//! capture (`capture_pane_tail`). Pattern-matches the last 20 lines for known
//! halt signatures (permission prompts, MCP confirmations) and sends the set
//! of halted session names to the main thread via mpsc channel.

use std::collections::HashSet;
use std::sync::mpsc;
use std::time::Duration;

use crate::tmux::TmuxManager;

const SCAN_INTERVAL: Duration = Duration::from_secs(2);
const CAPTURE_LINES: u32 = 20;

/// Known halt patterns — substring matches against each captured line.
///
/// These match Claude Code's distinctive permission/confirmation prompts.
/// Kept simple (no regex) for speed and clarity.
const HALT_PATTERNS: &[&str] = &[
    // Tool permission prompts: "Allow? (Y)es | (N)o | (A)lways"
    "(Y)es",
    // MCP / destructive action confirmations
    "Do you want to proceed?",
    // AskUserQuestion interactive selection prompts
    "Enter to select",
];

/// Spawn the feedback scanner thread.
///
/// Returns a receiver that yields `HashSet<String>` of tmux session names
/// currently in a halt state. Only sends when the set changes.
pub fn spawn(tmux: TmuxManager) -> mpsc::Receiver<HashSet<String>> {
    let (tx, rx) = mpsc::channel();
    std::thread::Builder::new()
        .name("nexus-feedback".to_string())
        .spawn(move || scanner_loop(tmux, tx))
        .expect("failed to spawn feedback scanner thread");
    rx
}

fn scanner_loop(tmux: TmuxManager, tx: mpsc::Sender<HashSet<String>>) {
    let mut last_set: HashSet<String> = HashSet::new();

    loop {
        let mut halted = HashSet::new();

        if let Ok(sessions) = tmux.list_sessions() {
            for session in &sessions {
                if let Ok(text) = tmux.capture_pane_tail(&session.session_id, CAPTURE_LINES) {
                    if has_halt_pattern(&text) {
                        halted.insert(session.session_id.clone());
                    }
                }
            }
        }

        // Only send if the set changed
        if halted != last_set {
            last_set.clone_from(&halted);
            if tx.send(halted).is_err() {
                return; // Main thread dropped receiver
            }
        }

        std::thread::sleep(SCAN_INTERVAL);
    }
}

/// Check if any line in the captured text contains a known halt pattern.
fn has_halt_pattern(text: &str) -> bool {
    text.lines()
        .any(|line| HALT_PATTERNS.iter().any(|p| line.contains(p)))
}

const PI_INPUT_PHRASES: &[&str] = &[
    "please review",
    "does this look",
    "what do you think",
    "would you like",
    "do you want",
    "should i",
    "shall i",
    "please confirm",
    "can you confirm",
    "let me know",
    "which option",
    "which approach",
    "choose one",
    "select one",
];

fn prose_without_fences(text: &str) -> String {
    let mut inside_fence = false;
    text.lines()
        .filter_map(|line| {
            let trimmed = line.trim_start();
            if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
                inside_fence = !inside_fence;
                return None;
            }
            (!inside_fence).then_some(line)
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn requests_input(text: &str) -> bool {
    let prose = prose_without_fences(text);
    let lowercase = prose.to_lowercase();
    PI_INPUT_PHRASES
        .iter()
        .any(|phrase| lowercase.contains(phrase))
        || prose
            .lines()
            .rev()
            .find(|line| !line.trim().is_empty())
            .is_some_and(|line| line.trim_end().ends_with('?'))
}

fn pi_is_working(text: &str) -> bool {
    text.lines().any(|line| line.contains("Working..."))
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
enum PiPhase {
    #[default]
    Unknown,
    Working,
    Idle,
    Attention,
}

#[derive(Debug, Clone, Copy, Default)]
struct PiAttentionState {
    phase: PiPhase,
    evaluation_pending: bool,
}

impl PiAttentionState {
    /// Observe the pane state and return whether JSONL evaluation is due.
    fn observe(&mut self, working: bool) -> bool {
        if working {
            self.phase = PiPhase::Working;
            self.evaluation_pending = false;
            return false;
        }
        if matches!(self.phase, PiPhase::Unknown | PiPhase::Working) {
            self.phase = PiPhase::Idle;
            self.evaluation_pending = true;
        }
        self.evaluation_pending
    }

    fn resolve(&mut self, matched: bool) {
        self.phase = if matched {
            PiPhase::Attention
        } else {
            PiPhase::Idle
        };
        self.evaluation_pending = false;
    }

    fn is_attention(self) -> bool {
        self.phase == PiPhase::Attention
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_permission_prompt() {
        let text = "  ○ Read src/main.rs\n\n  Allow? (Y)es | (N)o | (A)lways\n";
        assert!(has_halt_pattern(text));
    }

    #[test]
    fn detects_yes_no_always_line() {
        let text = "Some output\n  (Y)es | (N)o | (A)lways allow\n";
        assert!(has_halt_pattern(text));
    }

    #[test]
    fn detects_proceed_prompt() {
        let text = "Warning: this will delete files\nDo you want to proceed? (y/n)\n";
        assert!(has_halt_pattern(text));
    }

    #[test]
    fn ignores_normal_output() {
        let text = "Building project...\n✓ All tests passed\n> ";
        assert!(!has_halt_pattern(text));
    }

    #[test]
    fn ignores_empty() {
        assert!(!has_halt_pattern(""));
        assert!(!has_halt_pattern("\n\n\n"));
    }

    #[test]
    fn ignores_claude_idle_prompt() {
        // The normal "ready for next message" state should NOT trigger
        let text = "\n> \n";
        assert!(!has_halt_pattern(text));
    }

    #[test]
    fn detects_ask_user_question() {
        let text = "Which option?\n\n❯ 1. Option A\n  2. Option B\n\nEnter to select · ↑/↓ to navigate · Esc to cancel\n";
        assert!(has_halt_pattern(text));
    }

    #[test]
    fn detects_with_surrounding_content() {
        let text = "line 1\nline 2\nline 3\nline 4\nline 5\n\
                    line 6\nline 7\nline 8\n\
                    Allow? (Y)es | (N)o | (A)lways\nline 10\n";
        assert!(has_halt_pattern(text));
    }

    #[test]
    fn detects_pi_input_request_phrases_case_insensitively() {
        for text in [
            "Please review the plan.",
            "DOES THIS LOOK right",
            "What do you think",
            "Would you like me to continue",
            "Do you want option A",
            "Should I implement it",
            "Shall I proceed",
            "Please confirm the scope",
            "Can you confirm the path",
            "Let me know which you prefer",
            "Which option works best",
            "Which approach should we take",
            "Choose one of these",
            "Select one of these",
        ] {
            assert!(requests_input(text), "expected match for {text:?}");
        }
    }

    #[test]
    fn detects_final_prose_question() {
        assert!(requests_input("Implementation is ready.\nProceed with it?"));
    }

    #[test]
    fn ignores_non_question_completion() {
        assert!(!requests_input(
            "Implementation is complete and all tests pass."
        ));
    }

    #[test]
    fn recognizes_only_the_pi_working_status_text() {
        assert!(pi_is_working("⠴ Working..."));
        assert!(!pi_is_working("Implementation is complete."));
    }

    #[test]
    fn ignores_questions_confined_to_fenced_code() {
        let text = "Example:\n```text\nDoes this look right?\n```\nThe example is complete.";
        assert!(!requests_input(text));
    }

    #[test]
    fn unknown_idle_requests_one_recovery_evaluation() {
        let mut state = PiAttentionState::default();
        assert!(state.observe(false));
        state.resolve(false);
        assert!(!state.observe(false));
    }

    #[test]
    fn working_to_idle_requests_one_evaluation() {
        let mut state = PiAttentionState::default();
        assert!(!state.observe(true));
        assert!(state.observe(false));
        state.resolve(false);
        assert!(!state.observe(false));
    }

    #[test]
    fn failed_evaluation_remains_pending() {
        let mut state = PiAttentionState::default();
        assert!(state.observe(false));
        assert!(state.observe(false));
    }

    #[test]
    fn attention_persists_until_working_resumes() {
        let mut state = PiAttentionState::default();
        assert!(state.observe(false));
        state.resolve(true);
        assert!(state.is_attention());
        assert!(!state.observe(false));
        assert!(state.is_attention());
        assert!(!state.observe(true));
        assert!(!state.is_attention());
    }
}
