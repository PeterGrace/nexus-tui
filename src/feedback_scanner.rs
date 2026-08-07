//! Background scanner for agent input-attention detection.
//!
//! Polls all live tmux sessions every ~2 seconds using lightweight text-only
//! capture (`capture_pane_tail`). It detects Claude halt signatures directly
//! and combines Pi's `Working...` transition with its latest JSONL message,
//! then sends the attention set to the main thread via mpsc channel.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::Duration;

use crate::pi_session;
use crate::tmux::TmuxManager;
use crate::types::SessionAgent;

const SCAN_INTERVAL: Duration = Duration::from_secs(2);
const CAPTURE_LINES: u32 = 20;

type PiStates = HashMap<String, PiTargetState>;

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
pub(crate) struct PiSessionLocator {
    pub(crate) cwd: String,
    pub(crate) session_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct FeedbackTarget {
    pub(crate) tmux_name: String,
    pub(crate) agent: SessionAgent,
    pub(crate) pi_locator: Option<PiSessionLocator>,
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
                resolve_pi_paths(&mut pi_states);

                for session in &sessions {
                    let Ok(text) = tmux.capture_pane_tail(&session.session_id, CAPTURE_LINES)
                    else {
                        continue;
                    };

                    let mut session_halted = has_halt_pattern(&text);

                    if let Some(target) = targets.get(&session.session_id) {
                        if target.agent == SessionAgent::Pi {
                            if let Some(pi_state) = pi_states.get_mut(&session.session_id) {
                                if pi_state.attention.observe(pi_is_working(&text)) {
                                    evaluate_pi_path(
                                        pi_state.path.as_deref(),
                                        &mut pi_state.attention,
                                    );
                                }
                                session_halted |= pi_state.attention.is_attention();
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

fn fence_marker(line: &str) -> Option<(char, usize, &str)> {
    let trimmed = line.trim_start();
    let delimiter = trimmed.chars().next()?;
    if !matches!(delimiter, '`' | '~') {
        return None;
    }
    let length = trimmed.chars().take_while(|ch| *ch == delimiter).count();
    (length >= 3).then(|| (delimiter, length, &trimmed[length..]))
}

fn prose_without_fences(text: &str) -> String {
    let mut open_fence: Option<(char, usize)> = None;
    let mut prose = Vec::new();
    for line in text.lines() {
        if let Some((delimiter, length, suffix)) = fence_marker(line) {
            match open_fence {
                None => open_fence = Some((delimiter, length)),
                Some((open_delimiter, open_length))
                    if delimiter == open_delimiter
                        && length >= open_length
                        && suffix.trim().is_empty() =>
                {
                    open_fence = None;
                }
                Some(_) => {}
            }
            continue;
        }
        if open_fence.is_none() {
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

#[derive(Debug, Clone)]
struct PiTargetState {
    locator: Option<PiSessionLocator>,
    path: Option<PathBuf>,
    attention: PiAttentionState,
}

impl PiTargetState {
    fn new(locator: Option<PiSessionLocator>) -> Self {
        Self {
            locator,
            path: None,
            attention: PiAttentionState::default(),
        }
    }

    fn update_locator(&mut self, locator: Option<&PiSessionLocator>) {
        // Missing metadata can be transient. Only a concrete, changed locator
        // identifies a different transcript and resets the state machine.
        let Some(locator) = locator else {
            return;
        };
        if self.locator.as_ref() != Some(locator) {
            self.locator = Some(locator.clone());
            self.path = None;
            self.attention = PiAttentionState::default();
        }
    }
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
            Some(state) => state.update_locator(target.pi_locator.as_ref()),
            None => {
                states.insert(
                    target.tmux_name.clone(),
                    PiTargetState::new(target.pi_locator.clone()),
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
            Some(state) => state.update_locator(target.pi_locator.as_ref()),
            None => {
                states.insert(
                    target.tmux_name.clone(),
                    PiTargetState::new(target.pi_locator.clone()),
                );
            }
        }
    }
}

fn apply_pi_path_resolution(state: &mut PiTargetState, resolved: Option<PathBuf>) {
    // A failed lookup is not evidence that the known transcript disappeared.
    // Preserve both its cached path and attention state for a later retry.
    let Some(path) = resolved else {
        return;
    };
    if state.path.as_ref() != Some(&path) {
        state.path = Some(path);
        state.attention = PiAttentionState::default();
    }
}

fn resolve_pi_paths(states: &mut PiStates) {
    resolve_pi_paths_with(states, |locator| {
        pi_session::find_path(&locator.cwd, &locator.session_id)
    });
}

fn resolve_pi_paths_with(
    states: &mut PiStates,
    mut resolver: impl FnMut(&PiSessionLocator) -> Option<PathBuf>,
) {
    for state in states.values_mut() {
        // Cached paths do not require another recursive discovery scan.
        if state.path.is_some() {
            continue;
        }
        let Some(locator) = state.locator.clone() else {
            continue;
        };
        apply_pi_path_resolution(state, resolver(&locator));
    }
}

fn evaluate_pi_path(path: Option<&Path>, state: &mut PiAttentionState) {
    let Some(path) = path else {
        return;
    };
    match pi_session::latest_assistant_text(path) {
        Ok(Some(assistant_text)) => state.resolve(requests_input(&assistant_text)),
        Ok(None) => state.resolve(false),
        Err(_) => {
            // Incomplete, absent, or unreadable JSONL remains pending.
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
    fn shorter_backtick_marker_does_not_close_a_four_backtick_fence() {
        let text = concat!(
            "Example:\n",
            "````markdown\n",
            "```\n",
            "Does this look right?\n",
            "````\n",
            "The example is complete."
        );
        assert!(!requests_input(text));
    }

    #[test]
    fn tilde_marker_does_not_close_a_backtick_fence() {
        let text = concat!(
            "Example:\n",
            "```text\n",
            "~~~\n",
            "Should I continue?\n",
            "```\n",
            "The example is complete."
        );
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

    fn locator(id: &str) -> PiSessionLocator {
        PiSessionLocator {
            cwd: "/tmp/project".to_string(),
            session_id: id.to_string(),
        }
    }

    fn pi_target(name: &str, locator: Option<PiSessionLocator>) -> FeedbackTarget {
        FeedbackTarget {
            tmux_name: name.to_string(),
            agent: SessionAgent::Pi,
            pi_locator: locator,
        }
    }

    fn attention_state(locator: PiSessionLocator, path: PathBuf) -> PiTargetState {
        PiTargetState {
            locator: Some(locator),
            path: Some(path),
            attention: PiAttentionState {
                phase: PiPhase::Attention,
                evaluation_pending: false,
            },
        }
    }

    #[test]
    fn sync_pi_states_removes_missing_targets_and_resets_changed_locators() {
        let mut states = HashMap::from([
            (
                "changed".to_string(),
                attention_state(locator("old-id"), PathBuf::from("old.jsonl")),
            ),
            (
                "removed".to_string(),
                PiTargetState::new(Some(locator("removed-id"))),
            ),
        ]);
        let targets = HashMap::from([(
            "changed".to_string(),
            pi_target("changed", Some(locator("new-id"))),
        )]);

        sync_pi_states(&targets, &mut states);

        assert_eq!(states.len(), 1);
        assert_eq!(states["changed"].locator, Some(locator("new-id")));
        assert_eq!(states["changed"].path, None);
        assert_eq!(states["changed"].attention.phase, PiPhase::Unknown);
    }

    #[test]
    fn unresolved_path_observation_preserves_cached_path_and_attention() {
        let path = PathBuf::from("session.jsonl");
        let mut state = attention_state(locator("pi-id"), path.clone());

        apply_pi_path_resolution(&mut state, None);

        assert_eq!(state.path, Some(path));
        assert_eq!(state.attention.phase, PiPhase::Attention);
    }

    #[test]
    fn missing_locator_snapshot_preserves_cached_path_and_attention() {
        let path = PathBuf::from("session.jsonl");
        let targets = HashMap::from([("pi-live".to_string(), pi_target("pi-live", None))]);
        let mut states = HashMap::from([(
            "pi-live".to_string(),
            attention_state(locator("pi-id"), path.clone()),
        )]);

        sync_pi_states(&targets, &mut states);

        assert_eq!(states["pi-live"].locator, Some(locator("pi-id")));
        assert_eq!(states["pi-live"].path, Some(path));
        assert_eq!(states["pi-live"].attention.phase, PiPhase::Attention);
    }

    #[test]
    fn path_resolution_retries_unresolved_and_skips_cached_paths() {
        let mut states = HashMap::from([
            (
                "known".to_string(),
                attention_state(locator("known-id"), PathBuf::from("known.jsonl")),
            ),
            (
                "unresolved".to_string(),
                PiTargetState::new(Some(locator("unresolved-id"))),
            ),
        ]);
        let mut lookups = Vec::new();

        resolve_pi_paths_with(&mut states, |locator| {
            lookups.push(locator.session_id.clone());
            None
        });
        resolve_pi_paths_with(&mut states, |locator| {
            lookups.push(locator.session_id.clone());
            Some(PathBuf::from("resolved.jsonl"))
        });

        assert_eq!(lookups, ["unresolved-id", "unresolved-id"]);
        assert_eq!(
            states["unresolved"].path,
            Some(PathBuf::from("resolved.jsonl"))
        );
        assert_eq!(states["known"].attention.phase, PiPhase::Attention);
    }

    #[test]
    fn idle_nested_pi_v3_assistant_question_enters_attention() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("session.jsonl");
        std::fs::write(
            &path,
            concat!(
                "{\"type\":\"session\",\"version\":3,\"id\":\"pi-id\",\"cwd\":\"/tmp/project\"}\n",
                "{\"type\":\"message\",\"id\":\"msg-1\",\"parentId\":null,",
                "\"timestamp\":\"2026-08-06T12:00:00.000Z\",\"message\":{",
                "\"role\":\"assistant\",\"content\":[",
                "{\"type\":\"text\",\"text\":\"The change is ready. Proceed?\"}],",
                "\"provider\":\"example\",\"model\":\"example-model\",",
                "\"timestamp\":1775476800000}}\n"
            ),
        )
        .unwrap();
        let mut state = PiAttentionState::default();

        assert!(state.observe(false));
        evaluate_pi_path(Some(&path), &mut state);

        assert!(state.is_attention());
    }

    #[test]
    fn begin_scan_cycle_preserves_published_attention_when_listing_fails() {
        let path = PathBuf::from("session.jsonl");
        let targets = HashMap::from([(
            "pi-live".to_string(),
            pi_target("pi-live", Some(locator("pi-id"))),
        )]);
        let last_set = HashSet::from(["pi-live".to_string()]);
        let mut states = HashMap::from([(
            "pi-live".to_string(),
            attention_state(locator("pi-id"), path),
        )]);

        let halted = begin_scan_cycle(&last_set, None, &targets, &mut states);

        assert_eq!(halted, last_set);
        assert_eq!(states["pi-live"].attention.phase, PiPhase::Attention);
    }

    #[test]
    fn begin_scan_cycle_preserves_live_attention_before_capture_refresh() {
        let path = PathBuf::from("session.jsonl");
        let targets = HashMap::from([(
            "pi-live".to_string(),
            pi_target("pi-live", Some(locator("pi-id"))),
        )]);
        let live = HashSet::from(["pi-live"]);
        let last_set = HashSet::from(["pi-live".to_string()]);
        let mut states = HashMap::from([(
            "pi-live".to_string(),
            attention_state(locator("pi-id"), path),
        )]);

        let halted = begin_scan_cycle(&last_set, Some(&live), &targets, &mut states);

        assert_eq!(halted, last_set);
    }

    #[test]
    fn sync_live_pi_states_recreates_still_targeted_live_sessions() {
        let targets = HashMap::from([(
            "pi-live".to_string(),
            pi_target("pi-live", Some(locator("pi-id"))),
        )]);
        let live = HashSet::from(["pi-live"]);
        let mut states = PiStates::new();

        sync_live_pi_states(&live, &targets, &mut states);

        assert_eq!(states.len(), 1);
        assert_eq!(states["pi-live"].locator, Some(locator("pi-id")));
        assert_eq!(states["pi-live"].path, None);
        assert_eq!(states["pi-live"].attention.phase, PiPhase::Unknown);
    }
}
