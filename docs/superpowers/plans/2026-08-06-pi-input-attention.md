# Pi Input Attention Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Pulse a Nexus session row when Pi transitions from working to idle and its latest assistant response heuristically requests user input.

**Architecture:** Move Pi session-file discovery and JSONL parsing into a focused `pi_session` module. Extend the background feedback scanner with live session metadata and a per-Pi-session working/idle state machine; use tmux only for the `Working...` transition and Pi JSONL for the latest assistant prose. Keep the existing Claude terminal signatures and tree attention rendering unchanged.

**Tech Stack:** Rust 2021, std threads and `mpsc`, tmux pane capture, `serde_json`, Ratatui/TachyonFX, Cargo test/clippy/rustfmt.

## Global Constraints

- The normal detection trigger is an observed Pi `Working...` to idle transition.
- On first discovery, an already-idle Pi session is evaluated once to recover input requests that predate Nexus startup.
- Question matching is intentionally broad; occasional false positives are acceptable.
- Do not repeatedly evaluate unchanged idle Pi transcript content.
- Clear Pi attention as soon as a new `Working...` cycle is observed.
- Preserve the existing Claude permission/confirmation detection and existing attention rendering.
- Ignore question-like text inside Markdown fenced code blocks.
- Do not add dependencies.
- Update `README.md` because this is user-facing functionality.

---

## File structure

- Create `src/pi_session.rs`: own Pi session-root discovery, session metadata lookup, and extraction of the latest assistant text from Pi JSONL.
- Modify `src/main.rs`: register the new module.
- Modify `src/app.rs`: consume `pi_session`, send feedback-target snapshots, and remove Pi filesystem parsing from this already-large file.
- Modify `src/feedback_scanner.rs`: own feedback target metadata, Pi state transitions, question heuristics, and worker integration.
- Modify `README.md`: document heuristic Pi question detection and false-positive behavior.

---

### Task 1: Extract Pi session discovery and latest-message parsing

**Files:**
- Create: `src/pi_session.rs`
- Modify: `src/main.rs:1-18`
- Modify: `src/app.rs:2124-2131,2199-2266,2515-2535`
- Test: `src/pi_session.rs` (`#[cfg(test)]` module)

**Interfaces:**
- Produces: `pi_session::sessions(cwd: &str) -> Vec<PiSessionFile>`
- Produces: `pi_session::find_path(cwd: &str, id: &str) -> Option<PathBuf>`
- Produces: `pi_session::latest_assistant_text(path: &Path) -> color_eyre::Result<Option<String>>`
- Produces: `PiSessionFile { id: String, modified: SystemTime, path: PathBuf }`
- Consumes: Pi JSONL roots selected in this order: `PI_CODING_AGENT_SESSION_DIR`, `PI_CODING_AGENT_DIR/sessions`, then `~/.pi/agent/sessions`.

- [ ] **Step 1: Register the new module and write failing discovery tests**

Add to `src/main.rs` with the other module declarations:

```rust
mod pi_session;
```

Create `src/pi_session.rs` with tests that define the required API and preserve current recursive/cwd-filter behavior:

```rust
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use color_eyre::Result;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PiSessionFile {
    pub(crate) id: String,
    pub(crate) modified: SystemTime,
    pub(crate) path: PathBuf,
}

pub(crate) fn sessions(_cwd: &str) -> Vec<PiSessionFile> {
    unimplemented!()
}

pub(crate) fn find_path(_cwd: &str, _id: &str) -> Option<PathBuf> {
    unimplemented!()
}

pub(crate) fn latest_assistant_text(_path: &Path) -> Result<Option<String>> {
    unimplemented!()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_session(path: &Path, id: &str, cwd: &str, body: &str) {
        std::fs::write(
            path,
            format!(
                "{{\"type\":\"session\",\"version\":3,\"id\":\"{id}\",\"cwd\":\"{cwd}\"}}\n{body}"
            ),
        )
        .unwrap();
    }

    #[test]
    fn sessions_in_reads_matching_headers_recursively() {
        let temp = tempfile::tempdir().unwrap();
        let nested = temp.path().join("--tmp-project--");
        std::fs::create_dir(&nested).unwrap();
        write_session(
            &nested.join("matching.jsonl"),
            "pi-session-id",
            "/tmp/project",
            "{\"type\":\"message\",\"role\":\"user\",\"content\":[]}",
        );
        write_session(
            &nested.join("other.jsonl"),
            "other-id",
            "/tmp/other",
            "",
        );
        std::fs::write(nested.join("malformed.jsonl"), "not json").unwrap();

        let found = sessions_in(temp.path(), "/tmp/project");

        assert_eq!(found.len(), 1);
        assert_eq!(found[0].id, "pi-session-id");
        assert_eq!(found[0].path, nested.join("matching.jsonl"));
    }

    #[test]
    fn find_path_in_returns_the_matching_id() {
        let temp = tempfile::tempdir().unwrap();
        write_session(
            &temp.path().join("one.jsonl"),
            "one",
            "/tmp/project",
            "",
        );
        let expected = temp.path().join("two.jsonl");
        write_session(&expected, "two", "/tmp/project", "");

        assert_eq!(find_path_in(temp.path(), "/tmp/project", "two"), Some(expected));
    }
}
```

The private test seams required by these tests are:

```rust
fn sessions_in(root: &Path, cwd: &str) -> Vec<PiSessionFile>;
fn find_path_in(root: &Path, cwd: &str, id: &str) -> Option<PathBuf>;
```

- [ ] **Step 2: Run the discovery tests and verify failure**

Run:

```sh
cargo test pi_session::tests::sessions_in_reads_matching_headers_recursively
cargo test pi_session::tests::find_path_in_returns_the_matching_id
```

Expected: FAIL because `sessions_in` and `find_path_in` do not exist.

- [ ] **Step 3: Implement session-root selection and discovery**

Move the logic currently in `app.rs::pi_sessions` and `pi_sessions_in` into `src/pi_session.rs`. Use these exact helpers and populate the path as well as ID and modification time:

```rust
fn session_root() -> Option<PathBuf> {
    std::env::var_os("PI_CODING_AGENT_SESSION_DIR")
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("PI_CODING_AGENT_DIR")
                .map(PathBuf::from)
                .or_else(|| dirs::home_dir().map(|home| home.join(".pi/agent")))
                .map(|agent_dir| agent_dir.join("sessions"))
        })
}

pub(crate) fn sessions(cwd: &str) -> Vec<PiSessionFile> {
    session_root()
        .map(|root| sessions_in(&root, cwd))
        .unwrap_or_default()
}

fn sessions_in(root: &Path, cwd: &str) -> Vec<PiSessionFile> {
    use std::io::BufRead;

    let mut pending = vec![root.to_path_buf()];
    let mut found = Vec::new();
    while let Some(dir) = pending.pop() {
        let Ok(entries) = std::fs::read_dir(dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                pending.push(path);
                continue;
            }
            if path.extension().is_none_or(|extension| extension != "jsonl") {
                continue;
            }
            let Ok(file) = std::fs::File::open(&path) else {
                continue;
            };
            let Some(Ok(first_line)) = std::io::BufReader::new(file).lines().next() else {
                continue;
            };
            let Ok(header) = serde_json::from_str::<serde_json::Value>(&first_line) else {
                continue;
            };
            if header["type"].as_str() != Some("session") || header["cwd"].as_str() != Some(cwd) {
                continue;
            }
            let Some(id) = header["id"].as_str() else {
                continue;
            };
            let modified = entry
                .metadata()
                .and_then(|metadata| metadata.modified())
                .unwrap_or(SystemTime::UNIX_EPOCH);
            found.push(PiSessionFile {
                id: id.to_string(),
                modified,
                path,
            });
        }
    }
    found.sort_by_key(|session| std::cmp::Reverse(session.modified));
    found
}

fn find_path_in(root: &Path, cwd: &str, id: &str) -> Option<PathBuf> {
    sessions_in(root, cwd)
        .into_iter()
        .find(|session| session.id == id)
        .map(|session| session.path)
}

pub(crate) fn find_path(cwd: &str, id: &str) -> Option<PathBuf> {
    find_path_in(&session_root()?, cwd, id)
}
```

- [ ] **Step 4: Update `app.rs` to consume the extracted discovery API**

Add `use crate::pi_session;` near the other crate imports. Change Pi snapshot and detection branches to use `PiSessionFile`:

```rust
SessionAgent::Pi => pi_session::sessions(cwd)
    .into_iter()
    .map(|session| session.id)
    .collect(),
```

Replace `detect_pi_session_id` with:

```rust
fn detect_pi_session_id(cwd: &str, pre_launch: Option<&HashSet<String>>) -> Option<String> {
    let sessions = pi_session::sessions(cwd);
    match pre_launch {
        Some(snapshot) => sessions
            .into_iter()
            .find(|session| !snapshot.contains(&session.id))
            .map(|session| session.id),
        None => sessions.into_iter().next().map(|session| session.id),
    }
}
```

Delete `pi_sessions`, `pi_sessions_in`, and `test_pi_sessions_reads_matching_session_headers_recursively` from `src/app.rs`.

- [ ] **Step 5: Run discovery tests and the existing App tests**

Run:

```sh
cargo test pi_session::tests
cargo test app::tests
```

Expected: PASS.

- [ ] **Step 6: Write failing latest-message parser tests**

Add these tests to `src/pi_session.rs`:

```rust
#[test]
fn latest_assistant_text_joins_text_blocks_and_skips_metadata() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("session.jsonl");
    write_session(
        &path,
        "pi-id",
        "/tmp/project",
        concat!(
            "{\"type\":\"message\",\"role\":\"assistant\",\"content\":[",
            "{\"type\":\"text\",\"text\":\"Does this\"},",
            "{\"type\":\"toolCall\",\"name\":\"read\"},",
            "{\"type\":\"text\",\"text\":\"look right?\"}]}\n",
            "{\"type\":\"model_change\",\"modelId\":\"example\"}"
        ),
    );

    assert_eq!(
        latest_assistant_text(&path).unwrap().as_deref(),
        Some("Does this\nlook right?")
    );
}

#[test]
fn latest_assistant_text_ignores_an_older_assistant_after_user_or_tool_activity() {
    for (role, content) in [
        ("user", "[{\"type\":\"text\",\"text\":\"Yes\"}]"),
        ("toolResult", "[{\"type\":\"text\",\"text\":\"done\"}]"),
    ] {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("session.jsonl");
        write_session(
            &path,
            "pi-id",
            "/tmp/project",
            &format!(
                "{{\"type\":\"message\",\"role\":\"assistant\",\"content\":[{{\"type\":\"text\",\"text\":\"Continue?\"}}]}}\n{{\"type\":\"message\",\"role\":\"{role}\",\"content\":{content}}}"
            ),
        );
        assert_eq!(latest_assistant_text(&path).unwrap(), None);
    }
}

#[test]
fn latest_assistant_text_rejects_a_partial_jsonl_record() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("session.jsonl");
    write_session(
        &path,
        "pi-id",
        "/tmp/project",
        "{\"type\":\"message\",\"role\":\"assistant\",\"content\":[",
    );

    assert!(latest_assistant_text(&path).is_err());
}
```

- [ ] **Step 7: Run parser tests and verify failure**

Run:

```sh
cargo test pi_session::tests::latest_assistant_text
```

Expected: FAIL because `latest_assistant_text` is still unimplemented.

- [ ] **Step 8: Implement latest-message extraction**

Implement `latest_assistant_text` so every non-empty line must be valid JSON, unrelated metadata records are skipped, and the latest `message` record controls the result:

```rust
pub(crate) fn latest_assistant_text(path: &Path) -> Result<Option<String>> {
    use std::io::BufRead;

    let file = std::fs::File::open(path)?;
    let mut latest_message: Option<Option<String>> = None;
    for line in std::io::BufReader::new(file).lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let value: serde_json::Value = serde_json::from_str(&line)?;
        if value["type"].as_str() != Some("message") {
            continue;
        }
        let text = if value["role"].as_str() == Some("assistant") {
            let blocks = value["content"].as_array();
            let parts: Vec<&str> = blocks
                .into_iter()
                .flatten()
                .filter(|block| block["type"].as_str() == Some("text"))
                .filter_map(|block| block["text"].as_str())
                .collect();
            (!parts.is_empty()).then(|| parts.join("\n"))
        } else {
            None
        };
        latest_message = Some(text);
    }
    Ok(latest_message.flatten())
}
```

- [ ] **Step 9: Run all Task 1 tests and commit**

Run:

```sh
cargo test pi_session::tests
cargo test app::tests
cargo fmt --check
```

Expected: PASS.

Commit:

```sh
git add src/main.rs src/app.rs src/pi_session.rs
git commit -m "refactor: centralize Pi session parsing"
```

---

### Task 2: Add Pi question heuristics and the working/idle state machine

**Files:**
- Modify: `src/feedback_scanner.rs:1-131`
- Test: `src/feedback_scanner.rs` (`tests` module)

**Interfaces:**
- Consumes: `pi_session::latest_assistant_text(path: &Path) -> Result<Option<String>>`
- Produces: `FeedbackTarget { tmux_name: String, agent: SessionAgent, pi_session_path: Option<PathBuf> }`
- Produces: `FeedbackScannerHandle { attention_rx: Receiver<HashSet<String>>, targets_tx: Sender<Vec<FeedbackTarget>> }`
- Produces: state methods `PiAttentionState::observe(working: bool) -> bool`, `resolve(matched: bool)`, and `is_attention() -> bool`.
- Produces: pure helpers `pi_is_working(text: &str) -> bool` and `requests_input(text: &str) -> bool`.

- [ ] **Step 1: Write failing question-heuristic tests**

Add to `feedback_scanner.rs::tests`:

```rust
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
    assert!(!requests_input("Implementation is complete and all tests pass."));
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
```

- [ ] **Step 2: Run heuristic tests and verify failure**

Run:

```sh
cargo test feedback_scanner::tests::detects_pi_input_request_phrases_case_insensitively
cargo test feedback_scanner::tests::detects_final_prose_question
cargo test feedback_scanner::tests::ignores_questions_confined_to_fenced_code
```

Expected: FAIL because `requests_input` does not exist.

- [ ] **Step 3: Implement fenced-code removal and broad matching**

Add constants and helpers above the test module:

```rust
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
```

- [ ] **Step 4: Run heuristic tests**

Run:

```sh
cargo test feedback_scanner::tests
```

Expected: all existing Claude-pattern and new heuristic tests PASS.

- [ ] **Step 5: Write failing state-machine tests**

Add the state tests:

```rust
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
```

- [ ] **Step 6: Run state tests and verify failure**

Run:

```sh
cargo test feedback_scanner::tests::unknown_idle_requests_one_recovery_evaluation
cargo test feedback_scanner::tests::working_to_idle_requests_one_evaluation
cargo test feedback_scanner::tests::attention_persists_until_working_resumes
```

Expected: FAIL because `PiAttentionState` does not exist.

- [ ] **Step 7: Implement the state machine**

Add these private types and methods:

```rust
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
```

- [ ] **Step 8: Run pure feedback tests and commit**

Run:

```sh
cargo test feedback_scanner::tests
cargo fmt --check
```

Expected: PASS.

Commit:

```sh
git add src/feedback_scanner.rs
git commit -m "feat: classify Pi input requests"
```

---

### Task 3: Integrate Pi state into the background feedback worker

**Files:**
- Modify: `src/feedback_scanner.rs:8-75`
- Modify: `src/app.rs:1-22,120-128,204-213,267-272,316-330,1960-2024,2031-2057`
- Test: `src/feedback_scanner.rs` and `src/app.rs`

**Interfaces:**
- Consumes: `FeedbackTarget` and `FeedbackScannerHandle` from Task 2.
- Consumes: `pi_session::find_path(cwd: &str, id: &str) -> Option<PathBuf>` from Task 1.
- Produces: `collect_feedback_targets(tree: &[TreeNode]) -> Vec<FeedbackTarget>`.
- Produces: `App::sync_feedback_targets(&self)`.
- Preserves: `App.attention_sessions: HashSet<String>` and existing tree pulse rendering.

- [ ] **Step 1: Add feedback target and handle types**

At module scope in `feedback_scanner.rs`, add:

```rust
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

use crate::pi_session;
use crate::types::SessionAgent;

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
```

Change `spawn` to create a second channel and return the handle:

```rust
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
```

- [ ] **Step 2: Write a failing target-reconciliation test**

Extract target/state synchronization into a pure helper and specify its reset/removal behavior with this test:

```rust
#[test]
fn sync_pi_states_removes_missing_targets_and_resets_changed_paths() {
    let old_path = PathBuf::from("old.jsonl");
    let new_path = PathBuf::from("new.jsonl");
    let mut states = HashMap::from([
        (
            "changed".to_string(),
            (Some(old_path), PiAttentionState {
                phase: PiPhase::Attention,
                evaluation_pending: false,
            }),
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
```

Use this exact state table type in the worker:

```rust
type PiStates = HashMap<String, (Option<PathBuf>, PiAttentionState)>;
```

- [ ] **Step 3: Run the reconciliation test and verify failure**

Run:

```sh
cargo test feedback_scanner::tests::sync_pi_states_removes_missing_targets_and_resets_changed_paths
```

Expected: FAIL because `sync_pi_states` and `PiStates` do not exist.

- [ ] **Step 4: Implement target and state synchronization**

Add:

```rust
fn sync_pi_states(
    targets: &HashMap<String, FeedbackTarget>,
    states: &mut PiStates,
) {
    states.retain(|name, _| {
        targets
            .get(name)
            .is_some_and(|target| target.agent == SessionAgent::Pi)
    });
    for target in targets.values().filter(|target| target.agent == SessionAgent::Pi) {
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
```

Run the reconciliation test again; expected PASS.

- [ ] **Step 5: Integrate target updates and Pi evaluation into `scanner_loop`**

Change the loop signature and add owned worker state:

```rust
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

        let mut halted = HashSet::new();
        if let Ok(sessions) = tmux.list_sessions() {
            let live: HashSet<&str> = sessions
                .iter()
                .map(|session| session.session_id.as_str())
                .collect();
            pi_states.retain(|name, _| live.contains(name.as_str()));

            for session in &sessions {
                let Ok(text) = tmux.capture_pane_tail(&session.session_id, CAPTURE_LINES) else {
                    continue;
                };

                // Preserve the existing terminal-signature behavior.
                if has_halt_pattern(&text) {
                    halted.insert(session.session_id.clone());
                }

                let Some(target) = targets.get(&session.session_id) else {
                    continue;
                };
                if target.agent != SessionAgent::Pi {
                    continue;
                }
                let Some((path, state)) = pi_states.get_mut(&session.session_id) else {
                    continue;
                };
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
                        Ok(None) | Err(_) => {
                            // No path or incomplete/unreadable JSONL: leave pending.
                        }
                    }
                }
                if state.is_attention() {
                    halted.insert(session.session_id.clone());
                }
            }
        }

        if halted != last_set {
            last_set.clone_from(&halted);
            if tx.send(halted).is_err() {
                return;
            }
        }
        std::thread::sleep(SCAN_INTERVAL);
    }
}
```

Keep `has_halt_pattern` and all existing Claude tests intact. If rustfmt expands the nested `Option` match differently, preserve these semantics:

- no path or parser error leaves evaluation pending;
- latest non-assistant message resolves to no attention;
- latest assistant text resolves via `requests_input`.

- [ ] **Step 6: Update `App` fields and scanner startup wiring**

Replace the single scanner receiver field with:

```rust
feedback_rx: Option<mpsc::Receiver<HashSet<String>>>,
feedback_targets_tx: Option<mpsc::Sender<Vec<feedback_scanner::FeedbackTarget>>>,
```

In `App::new`, return three values from the tmux-available branch:

```rust
let (interactor_state, feedback_rx, feedback_targets_tx) = if tmux_available {
    // existing tmux configuration and capture-worker setup
    let handle = feedback_scanner::spawn(tmux.clone());
    (
        Some(is),
        Some(handle.attention_rx),
        Some(handle.targets_tx),
    )
} else {
    (None, None, None)
};
```

Initialize `feedback_targets_tx` in `Self`. After `refresh_cached_selected` and
`sync_interactor_to_selection`, call:

```rust
app.sync_feedback_targets();
```

- [ ] **Step 7: Write the target collector and App synchronization method**

Add a free helper in `app.rs`:

```rust
fn collect_feedback_targets(tree: &[TreeNode]) -> Vec<feedback_scanner::FeedbackTarget> {
    let mut result = Vec::new();
    for node in tree {
        match node {
            TreeNode::Group(group) => result.extend(collect_feedback_targets(&group.children)),
            TreeNode::Session(session) if session.status != SessionStatus::Dead => {
                let Some(tmux_name) = session.tmux_name.clone() else {
                    continue;
                };
                let pi_session_path = if session.agent == SessionAgent::Pi {
                    session
                        .cwd
                        .as_deref()
                        .zip(session.agent_session_id.as_deref())
                        .and_then(|(cwd, id)| pi_session::find_path(&cwd.to_string_lossy(), id))
                } else {
                    None
                };
                result.push(feedback_scanner::FeedbackTarget {
                    tmux_name,
                    agent: session.agent,
                    pi_session_path,
                });
            }
            TreeNode::Session(_) => {}
        }
    }
    result
}
```

Add the method to `impl App`:

```rust
fn sync_feedback_targets(&self) {
    if let Some(tx) = &self.feedback_targets_tx {
        let _ = tx.send(collect_feedback_targets(&self.tree));
    }
}
```

At the end of a successful `refresh_tree`, before setting `dirty`, call:

```rust
self.sync_feedback_targets();
```

This covers session creation, resume, rename, deletion, and the `refresh_tree`
triggered after Pi session-ID detection. The worker independently drops state
when a tmux session is no longer live.

- [ ] **Step 8: Add an App target-collection regression test**

Use `crate::mock::mock_tree()` to verify non-Pi metadata and mutate one session
into a dead row to verify filtering:

```rust
#[test]
fn collect_feedback_targets_includes_only_live_named_sessions() {
    let mut tree = crate::mock::mock_tree();
    let TreeNode::Group(first_group) = &mut tree[0] else {
        panic!("first fixture node must be a group");
    };
    let TreeNode::Session(first_session) = &mut first_group.children[0] else {
        panic!("first fixture child must be a session");
    };
    first_session.agent = SessionAgent::Codex;
    first_session.agent_session_id = Some("codex-id".to_string());

    let targets = collect_feedback_targets(&tree);

    assert_eq!(targets.len(), 3);
    let target = targets
        .iter()
        .find(|target| {
            target.tmux_name == "a1b2c3d4-e5f6-7890-abcd-ef1234567890"
        })
        .unwrap();
    assert_eq!(target.agent, SessionAgent::Codex);
    assert_eq!(target.pi_session_path, None);
    assert!(targets.iter().all(|target| !target.tmux_name.is_empty()));
}
```

The two dead mock sessions have no tmux name and must be excluded; the active
and detached named sessions remain valid feedback targets.

- [ ] **Step 9: Run worker, App, and full unit tests**

Run:

```sh
cargo test feedback_scanner::tests
cargo test app::tests
cargo test
```

Expected: PASS. Existing Claude detection tests must still pass unchanged.

- [ ] **Step 10: Run formatting and lint checks, then commit**

Run:

```sh
cargo fmt
cargo fmt --check
cargo clippy -- -D warnings
```

Expected: PASS with zero warnings.

Commit:

```sh
git add src/app.rs src/feedback_scanner.rs
git commit -m "feat: detect Pi sessions waiting for input"
```

---

### Task 4: Document and validate Pi attention detection

**Files:**
- Modify: `README.md:24-27`

**Interfaces:**
- Consumes: completed feedback behavior from Tasks 1-3.
- Produces: user-facing description of Claude and Pi feedback detection.

- [ ] **Step 1: Update the README feature description**

Replace the existing Feedback detection bullet with:

```markdown
- **Feedback detection** — pulses a session row when Claude is waiting for permission or confirmation, or when Pi heuristically appears to be asking for user input (Pi detection is intentionally broad and may occasionally produce false positives)
```

- [ ] **Step 2: Run repository validation**

Run:

```sh
cargo fmt --check
cargo clippy -- -D warnings
cargo test
git diff --check
```

Expected: all commands PASS; tests include Pi JSONL parsing, broad heuristics,
state transitions, worker target synchronization, and unchanged Claude
patterns.

- [ ] **Step 3: Inspect the final diff for scope and accidental input**

Run:

```sh
git diff --stat HEAD~3
git diff HEAD~3 -- README.md src/main.rs src/app.rs src/pi_session.rs src/feedback_scanner.rs
git status --short
```

Confirm:

- only the planned source and README files changed;
- no dependencies were added;
- no arbitrary pasted SQL or database text was added as a heuristic;
- the design and implementation-plan commits remain present;
- the worktree is otherwise clean.

- [ ] **Step 4: Commit documentation**

```sh
git add README.md
git commit -m "docs: describe Pi feedback detection"
```

- [ ] **Step 5: Verify the committed branch**

Run:

```sh
git status --short --branch
git log -5 --oneline --decorate
```

Expected: clean worktree on `nexus-tui/nexus-wait-for-input`, with the implementation and documentation commits at the tip.
