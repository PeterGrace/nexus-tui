# Detect when Pi is waiting for user input

**Date:** 2026-08-06
**Status:** Approved — ready for implementation plan

## Problem

Nexus pulses a session row when Claude Code is waiting for permission or
confirmation, but it does not do so when Pi finishes a turn by asking the user
a question.

The existing feedback scanner in `src/feedback_scanner.rs` polls the last 20
lines of every live tmux pane every two seconds and looks for Claude-specific
terminal strings such as `(Y)es`, `Do you want to proceed?`, and
`Enter to select`. Pi does not render those strings. A conversational Pi
question is ordinary assistant text followed by Pi returning to its editor, so
there is no equivalent structured terminal prompt to match.

## Goal

Pulse a Pi session's tree row when Pi has finished working and its latest
assistant response heuristically appears to request user input.

False positives from broad question heuristics are acceptable. Detection must
avoid repeatedly matching stale transcript content and must preserve the
existing Claude behavior.

## Chosen approach

Use Pi's `Working...` indicator as the primary state-transition signal and its
structured JSONL session as the source of assistant text.

Nexus will evaluate question heuristics only:

1. after observing a Pi session transition from working to idle; or
2. once when Nexus first discovers an already-idle Pi session, so a question
   that predates Nexus startup can be recovered.

It will not repeatedly re-evaluate an unchanged idle session. This makes the
working-to-idle transition the normal trigger while still handling restarts.

Terminal-only parsing was rejected because wrapping, scrolling, and retained
transcript lines make it difficult to identify the latest assistant response.
Applying generic question matching to every agent's pane was rejected because
it would also match user prompts, source code, and command output.

## State machine

The scanner keeps transient state for each live Pi tmux session:

- **Unknown** — the session was just discovered and has not been classified.
- **Working** — the pane currently contains Pi's `Working...` indicator.
- **Idle** — Pi is ready for input and the latest eligible assistant response
  did not match, or has already been evaluated.
- **Attention** — Pi is idle and the latest eligible assistant response matched
  the heuristics.

Transitions:

| Current state | Observation | Next state | Action |
|---|---|---|---|
| Unknown | Working visible | Working | Do not evaluate or alert |
| Unknown | Working absent | Idle or Attention | Evaluate the latest assistant response once |
| Working | Working visible | Working | No action |
| Working | Working absent | Idle or Attention | Evaluate the latest assistant response |
| Idle | Working absent | Idle | Do not re-evaluate stale content |
| Attention | Working absent | Attention | Preserve attention |
| Idle or Attention | Working visible | Working | Clear attention immediately |

If an eligible JSONL read fails because the file is absent, unreadable,
malformed, or temporarily ends in a partial record, evaluation remains pending
and is retried on later scans while the session remains idle. A successful
non-match completes the evaluation and enters Idle. Returning to Working
cancels any pending evaluation and starts a new cycle.

Sessions removed from tmux are removed from both the state table and the
attention set.

## Components

### Feedback targets

The feedback worker needs an up-to-date snapshot describing live Nexus
sessions. Each target contains enough information to distinguish agent types
and locate Pi's session data:

- tmux session name;
- `SessionAgent`;
- Pi JSONL path when known.

`feedback_scanner::spawn` will expose both its existing attention receiver and
a sender for replacing the current target snapshot. `App` sends a snapshot at
startup and whenever tree, tmux, or detected agent-session metadata changes.
The worker owns the target snapshot and state machine, keeping filesystem and
tmux polling off the render/event thread.

Pi JSONL path resolution will use the same configured session roots already
honored by Pi session-ID discovery:

1. `PI_CODING_AGENT_SESSION_DIR`;
2. `PI_CODING_AGENT_DIR/sessions`;
3. `~/.pi/agent/sessions`.

A Pi target without a resolved JSONL path remains unalerted until a later
metadata snapshot supplies one. Supplying or changing the path resets that
target to Unknown so startup-style recovery is attempted once.

### Working-state detection

The scanner continues using `TmuxManager::capture_pane_tail`. For Pi targets it
checks the current tail for the literal `Working...` status text. The spinner
glyph preceding it is intentionally ignored because it changes between
frames.

This signal gates JSONL evaluation; the pane transcript itself is not used as
the source of question text.

### Latest Pi message extraction

When evaluation is due, read complete JSONL records from the end of the Pi
session file and identify the latest `message` record, skipping unrelated
metadata records. It qualifies only when its role is `assistant` and its
content contains text. Assistant tool-call blocks are ignored; text blocks are
joined for heuristic matching.

A latest user or tool-result message is a successful non-match. This prevents
stale assistant questions from being selected when newer conversational or
tool activity exists without allowing a trailing metadata record to hide the
latest assistant response.

### Question heuristics

Before matching, remove Markdown fenced-code blocks from the assistant text.
This reduces obvious matches in code examples while retaining intentionally
broad prose matching.

Matching is case-insensitive and succeeds when either condition holds:

1. prose contains a user-directed phrase such as:
   - `please review`;
   - `does this look`;
   - `what do you think`;
   - `would you like`;
   - `do you want`;
   - `should I`;
   - `shall I`;
   - `please confirm`;
   - `can you confirm`;
   - `let me know`;
   - `which option`;
   - `which approach`;
   - `choose one`;
   - `select one`;
2. the final meaningful prose line ends in `?`.

The phrase set is a small, explicit constant so additions and regressions are
easy to test. Broad matching and occasional false positives are intentional.
Arbitrary command or SQL text is not a special pattern.

## Data flow

```text
App tree/tmux/session-ID refresh
  -> build feedback target snapshot
  -> feedback worker control channel

feedback worker every ~2 seconds
  -> list/capture live tmux sessions
  -> retain existing Claude terminal-pattern detection
  -> for each Pi target:
       capture tail and classify Working vs idle
       apply state transition
       if evaluation became pending:
         read latest complete Pi JSONL record
         extract latest assistant prose
         strip fenced code
         apply phrase/final-question heuristics
  -> send changed set of attention tmux names

App event loop
  -> replace attention_sessions
  -> rebuild pulse effects
  -> tree renders the existing pulsing attention row
```

The currently selected session remains excluded from the visible tree pulse,
matching existing behavior.

## Error handling and edge cases

- **Nexus starts while Pi is already asking a question:** Unknown-to-idle
  recovery evaluates the latest message once.
- **New Pi process is idle before it has a session file:** no alert; metadata
  refresh resets it to Unknown after the file is discovered.
- **Partial JSONL append:** do not alert or consume the evaluation; retry later.
- **Unreadable or missing file:** do not alert; retry while evaluation remains
  pending.
- **Pi starts working after an alert:** clear the alert on the next scan before
  evaluating any later response.
- **User has typed but not submitted a reply:** preserve attention because Pi
  has not started a new working cycle.
- **Pi finishes without a question:** enter Idle without attention and do not
  reconsider that response until another working cycle.
- **Question text inside fenced code:** ignore it unless qualifying prose
  outside the fence also matches.
- **Unknown agent type:** do not apply Pi heuristics.
- **Claude and Codex:** preserve current behavior; this design adds no generic
  idle detection for them.

## Testing

Unit tests in or alongside `src/feedback_scanner.rs` will cover pure parsing,
heuristics, and state transitions without requiring live tmux:

1. detect `Please review this plan`;
2. detect `Does this look right?`;
3. detect a final prose line ending in `?`;
4. detect representative confirmation and choice phrases without a trailing
   question mark;
5. match phrases case-insensitively;
6. ignore question text confined to fenced code;
7. ignore a non-question assistant completion;
8. ignore latest user and tool-result records even if an older assistant asked
   a question;
9. Unknown-to-idle performs one recovery evaluation;
10. Working-to-idle performs one evaluation;
11. Idle-to-idle does not re-evaluate;
12. Attention-to-idle preserves attention;
13. Attention-to-Working clears attention;
14. a failed JSONL read leaves evaluation pending for retry;
15. removing a session clears its scanner state and attention;
16. existing Claude permission, confirmation, and AskUserQuestion tests remain
    unchanged and passing.

Validation for implementation will run:

```sh
cargo fmt --check
cargo clippy -- -D warnings
cargo test
```

## Documentation

Update `README.md` feedback detection to say that Nexus detects Claude
permission/confirmation prompts and heuristically detects Pi assistant
questions. Mention that Pi question detection is heuristic and may occasionally
produce false positives.

## Out of scope

- Adding a custom Pi `AskUserQuestion` tool or extension.
- Changing Pi's prompts or system instructions.
- Treating every idle/completed Pi turn as requiring attention.
- Persisting attention state across Nexus restarts; startup recovery derives it
  from current pane and session data.
- User-configurable heuristic phrases.
- Extending broad question heuristics to Claude, Codex, or unknown agents.
