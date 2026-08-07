//! Background scanner for Claude Code halt-state detection.
//!
//! Polls all live tmux sessions every ~2 seconds using lightweight text-only
//! capture (`capture_pane_tail`). Pattern-matches the last 20 lines for known
//! halt signatures (permission prompts, MCP confirmations) and sends the set
//! of halted session names to the main thread via mpsc channel.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::mpsc;
use std::time::Duration;

use crate::pi_session;
use crate::tmux::TmuxManager;
use crate::types::SessionAgent;

const SCAN_INTERVAL: Duration = Duration::from_secs(2);
const CAPTURE_LINES: u32 = 20;

type PiStates = HashMap<String, (Option<PathBuf>, PiAttentionState)>;

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

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct FeedbackTarget {
    pub(crate) tmux_name: String,
    pub(crate) agent: SessionAgent,
    pub(crate) pi_session_path: Option<PathBuf>,
}

pub(crate) struct FeedbackScannerHandle {
    pub(crate) attention_rx: mpsc::Receiver<HashSet<String>>,
    pub(crate) targets_tx: mpsc::Sender<Vec<FeedbackTarget>>,
}

/// Spawn the feedback scanner thread.
///
/// Returns the attention receiver plus a target-update sender.
pub(crate) fn spawn(tmux: TmuxManager) -> FeedbackScannerHandle {
    let (attention_tx, attention_rx) = mpsc::channel();
    let (targets_tx, targets_rx) = mpsc::channel();
    std::thread::Builder::new()
        .name("nexus-feedback".to_string())
        .spawn(move || scanner_loop(tmux, attention_tx, targets_rx))
        .expect("failed to spawn feedback scanner thread");
    FeedbackScannerHandle {
        attention_rx,
        targets_tx,
    }
}

fn scanner_loop(
    tmux: TmuxManager,
    tx: mpsc::Sender<HashSet<String>>,
    targets_rx: mpsc::Receiver<Vec<FeedbackTarget>>,
) {
    let mut last_set = HashSet::new();
    let mut targets = HashMap::<String, FeedbackTarget>::new();
    let mut pi_states = PiStates::new();

    loop {
        if let Some(latest) = targets_rx.try_iter().last() {
            targets = latest
                .into_iter()
                .map(|target| (target.tmux_name.clone(), target))
                .collect();
            sync_pi_states(&targets, &mut pi_states);
        }

        let halted = match tmux.list_sessions() {
            Ok(sessions) => {
                let live: HashSet<&str> = sessions
                    .iter()
                    .map(|session| session.session_id.as_str())
                    .collect();
                let mut halted = begin_scan_cycle(&last_set, Some(&live), &targets, &mut pi_states);

                for session in &sessions {
                    let Ok(text) = tmux.capture_pane_tail(&session.session_id, CAPTURE_LINES)
                    else {
                        continue;
                    };

                    let mut session_halted = has_halt_pattern(&text);

                    if let Some(target) = targets.get(&session.session_id) {
                        if target.agent == SessionAgent::Pi {
                            if let Some((path, state)) = pi_states.get_mut(&session.session_id) {
                                if state.observe(pi_is_working(&text)) {
                                    let evaluation = path
                                        .as_deref()
                                        .map(pi_session::latest_assistant_text)
                                        .transpose();
                                    match evaluation {
                                        Ok(Some(Some(assistant_text))) => {
                                            state.resolve(requests_input(&assistant_text));
                                        }
                                        Ok(Some(None)) => state.resolve(false),
                                        Ok(None) | Err(_) => {}
                                    }
                                }
                                session_halted |= state.is_attention();
                            }
                        }
                    }

                    if session_halted {
                        halted.insert(session.session_id.clone());
                    } else {
                        halted.remove(&session.session_id);
                    }
                }

                halted
            }
            Err(_) => begin_scan_cycle(&last_set, None, &targets, &mut pi_states),
        };

        if halted != last_set {
            last_set.clone_from(&halted);
            if tx.send(halted).is_err() {
                return;
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
    let mut prose = Vec::new();
    for line in text.lines() {
        let trimmed = line.trim_start();
        if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
            inside_fence = !inside_fence;
            continue;
        }
        if !inside_fence {
            prose.push(line);
        }
    }
    prose.join("\n")
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

fn sync_pi_states(targets: &HashMap<String, FeedbackTarget>, states: &mut PiStates) {
    states.retain(|name, _| {
        targets
            .get(name)
            .is_some_and(|target| target.agent == SessionAgent::Pi)
    });
    for target in targets
        .values()
        .filter(|target| target.agent == SessionAgent::Pi)
    {
        match states.get_mut(&target.tmux_name) {
            Some((path, state)) if *path != target.pi_session_path => {
                *path = target.pi_session_path.clone();
                *state = PiAttentionState::default();
            }
            Some(_) => {}
            None => {
                states.insert(
                    target.tmux_name.clone(),
                    (target.pi_session_path.clone(), PiAttentionState::default()),
                );
            }
        }
    }
}

fn sync_live_pi_states(
    live: &HashSet<&str>,
    targets: &HashMap<String, FeedbackTarget>,
    states: &mut PiStates,
) {
    states.retain(|name, _| {
        live.contains(name.as_str())
            && targets
                .get(name)
                .is_some_and(|target| target.agent == SessionAgent::Pi)
    });
    for name in live {
        let Some(target) = targets.get(*name) else {
            continue;
        };
        if target.agent != SessionAgent::Pi {
            continue;
        }
        match states.get_mut(*name) {
            Some((path, state)) if *path != target.pi_session_path => {
                *path = target.pi_session_path.clone();
                *state = PiAttentionState::default();
            }
            Some(_) => {}
            None => {
                states.insert(
                    target.tmux_name.clone(),
                    (target.pi_session_path.clone(), PiAttentionState::default()),
                );
            }
        }
    }
}

fn begin_scan_cycle(
    last_set: &HashSet<String>,
    live: Option<&HashSet<&str>>,
    targets: &HashMap<String, FeedbackTarget>,
    states: &mut PiStates,
) -> HashSet<String> {
    let Some(live) = live else {
        return last_set.clone();
    };
    sync_live_pi_states(live, targets, states);
    last_set
        .iter()
        .filter(|name| live.contains(name.as_str()))
        .cloned()
        .collect()
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

    #[test]
    fn sync_pi_states_removes_missing_targets_and_resets_changed_paths() {
        let old_path = PathBuf::from("old.jsonl");
        let new_path = PathBuf::from("new.jsonl");
        let mut states = HashMap::from([
            (
                "changed".to_string(),
                (
                    Some(old_path),
                    PiAttentionState {
                        phase: PiPhase::Attention,
                        evaluation_pending: false,
                    },
                ),
            ),
            ("removed".to_string(), (None, PiAttentionState::default())),
        ]);
        let targets = HashMap::from([(
            "changed".to_string(),
            FeedbackTarget {
                tmux_name: "changed".to_string(),
                agent: SessionAgent::Pi,
                pi_session_path: Some(new_path.clone()),
            },
        )]);

        sync_pi_states(&targets, &mut states);

        assert_eq!(states.len(), 1);
        assert_eq!(states["changed"].0, Some(new_path));
        assert_eq!(states["changed"].1.phase, PiPhase::Unknown);
    }

    #[test]
    fn begin_scan_cycle_preserves_published_attention_when_listing_fails() {
        let path = PathBuf::from("session.jsonl");
        let targets = HashMap::from([(
            "pi-live".to_string(),
            FeedbackTarget {
                tmux_name: "pi-live".to_string(),
                agent: SessionAgent::Pi,
                pi_session_path: Some(path.clone()),
            },
        )]);
        let last_set = HashSet::from(["pi-live".to_string()]);
        let mut states = HashMap::from([(
            "pi-live".to_string(),
            (
                Some(path),
                PiAttentionState {
                    phase: PiPhase::Attention,
                    evaluation_pending: false,
                },
            ),
        )]);

        let halted = begin_scan_cycle(&last_set, None, &targets, &mut states);

        assert_eq!(halted, last_set);
        assert_eq!(states["pi-live"].1.phase, PiPhase::Attention);
    }

    #[test]
    fn begin_scan_cycle_preserves_live_attention_before_capture_refresh() {
        let path = PathBuf::from("session.jsonl");
        let targets = HashMap::from([(
            "pi-live".to_string(),
            FeedbackTarget {
                tmux_name: "pi-live".to_string(),
                agent: SessionAgent::Pi,
                pi_session_path: Some(path.clone()),
            },
        )]);
        let live = HashSet::from(["pi-live"]);
        let last_set = HashSet::from(["pi-live".to_string()]);
        let mut states = HashMap::from([(
            "pi-live".to_string(),
            (
                Some(path),
                PiAttentionState {
                    phase: PiPhase::Attention,
                    evaluation_pending: false,
                },
            ),
        )]);

        let halted = begin_scan_cycle(&last_set, Some(&live), &targets, &mut states);

        assert_eq!(halted, last_set);
    }

    #[test]
    fn sync_live_pi_states_recreates_still_targeted_live_sessions() {
        let path = PathBuf::from("session.jsonl");
        let targets = HashMap::from([(
            "pi-live".to_string(),
            FeedbackTarget {
                tmux_name: "pi-live".to_string(),
                agent: SessionAgent::Pi,
                pi_session_path: Some(path.clone()),
            },
        )]);
        let live = HashSet::from(["pi-live"]);
        let mut states = PiStates::new();

        sync_live_pi_states(&live, &targets, &mut states);

        assert_eq!(states.len(), 1);
        assert_eq!(states["pi-live"].0, Some(path));
        assert_eq!(states["pi-live"].1.phase, PiPhase::Unknown);
    }
}
