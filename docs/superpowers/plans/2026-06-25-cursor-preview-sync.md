# Cursor/Preview Sync Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** After any tree refresh, re-anchor the tree cursor to the previewed session so the tmux preview pane, the highlighted row, and the `alt-d` delete target never disagree.

**Architecture:** Add a single reconciliation method `TreeState::anchor_to` that moves `cursor_index` to the row matching the previewed `SelectionTarget` (by stable ID), falling back to the nearest surviving row when the target is gone. Call it from `App::refresh_tree`, the one place where the cursor currently drifts from the preview. No new sources of truth; we just close the one gap where the existing two drift apart.

**Tech Stack:** Rust, Ratatui, cargo test (unit tests in-module).

---

## File Structure

- **Modify:** `src/widgets/tree_state.rs`
  - Add free helper `flat_target(&FlatNode) -> SelectionTarget`.
  - Add public method `TreeState::anchor_to`.
  - Refactor `selected_target` to reuse `flat_target` (DRY).
  - Add unit tests in the existing `mod tests`.
- **Modify:** `src/app.rs`
  - Replace the bare `refresh_cached_selected()` call in `refresh_tree` (around line 1903) with the anchor-then-sync sequence.

No new files. No README change (bug fix, no user-facing feature/keybinding change).

---

## Task 1: `flat_target` helper + `anchor_to` on `TreeState`

**Files:**
- Modify: `src/widgets/tree_state.rs` (add helper near `flatten_tree`, add method in `impl TreeState`, refactor `selected_target` at lines 183-193)
- Test: `src/widgets/tree_state.rs` (`mod tests`)

- [ ] **Step 1: Write the failing tests**

Add these four tests inside the existing `mod tests` block (after `test_selected_target_session`):

```rust
    #[test]
    fn test_anchor_to_finds_session_by_id() {
        // Cursor is stale (pointing elsewhere); anchor_to must move it to the
        // row whose session_id matches the target, regardless of position.
        let tree = mock::mock_tree();
        let mut state = TreeState::new(&tree);
        state.cursor_index = 5; // somewhere unrelated

        // feat/scanner lives at flat index 1 in the all-expanded mock tree.
        let target = SelectionTarget::Session("a1b2c3d4-e5f6-7890-abcd-ef1234567890".to_string());
        let resolved = state.anchor_to(Some(&target), &tree);

        assert_eq!(state.cursor_index, 1);
        assert_eq!(resolved, Some(target));
    }

    #[test]
    fn test_anchor_to_missing_session_clamps_to_neighbor() {
        // Target was deleted: anchor_to clamps the cursor into range and returns
        // whatever target now sits under it (a different, surviving target).
        let tree = mock::mock_tree();
        let mut state = TreeState::new(&tree);
        let count = state.visible_nodes(&tree).len();
        state.cursor_index = 99; // out of bounds, as if rows were removed

        let gone = SelectionTarget::Session("does-not-exist".to_string());
        let resolved = state.anchor_to(Some(&gone), &tree);

        assert!(state.cursor_index < count, "cursor must be clamped in range");
        assert_eq!(state.cursor_index, count - 1);
        assert_ne!(resolved, Some(gone), "must not return the missing target");
        assert!(resolved.is_some(), "a surviving neighbor target is returned");
    }

    #[test]
    fn test_anchor_to_does_not_expand_collapsed_group() {
        // The selected session is hidden inside a collapsed group. anchor_to must
        // NOT re-expand it; it treats the hidden session as gone and clamps.
        let tree = mock::mock_tree();
        let mut state = TreeState::new(&tree);
        state.toggle_expand(1); // collapse nexus group (hides feat/scanner)
        assert!(!state.expanded.contains(&1));

        let hidden = SelectionTarget::Session("a1b2c3d4-e5f6-7890-abcd-ef1234567890".to_string());
        let resolved = state.anchor_to(Some(&hidden), &tree);

        assert!(!state.expanded.contains(&1), "must not auto-expand the group");
        assert_ne!(resolved, Some(hidden), "hidden session is not selectable");
        assert!(resolved.is_some());
    }

    #[test]
    fn test_anchor_to_empty_tree_returns_none() {
        let tree: Vec<TreeNode> = Vec::new();
        let mut state = TreeState::new(&tree);
        let target = SelectionTarget::Session("x".to_string());
        let resolved = state.anchor_to(Some(&target), &tree);
        assert_eq!(resolved, None);
        assert_eq!(state.cursor_index, 0);
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test --lib tree_state::tests::test_anchor_to`
Expected: FAIL — compile error `no method named `anchor_to` found for struct `TreeState``.

- [ ] **Step 3: Add the `flat_target` helper**

Add this free function in `src/widgets/tree_state.rs`, immediately after `flatten_tree` (after line 81):

```rust
/// Map a flattened node to its selection target.
fn flat_target(node: &FlatNode) -> SelectionTarget {
    match &node.node {
        FlatNodeKind::Group { id, .. } => SelectionTarget::Group(*id),
        FlatNodeKind::Session { summary } => {
            SelectionTarget::Session(summary.session_id.clone())
        }
    }
}
```

- [ ] **Step 4: Refactor `selected_target` to use the helper**

Replace the body of `selected_target` (lines 183-193) with:

```rust
    pub fn selected_target(&mut self, tree: &[TreeNode]) -> Option<SelectionTarget> {
        self.ensure_cache(tree);
        self.cached_flat.get(self.cursor_index).map(flat_target)
    }
```

- [ ] **Step 5: Add the `anchor_to` method**

Add this method inside `impl TreeState` (place it right after `selected_target`):

```rust
    /// Re-anchor the cursor to `target` after the tree changed underneath us.
    ///
    /// - If `target` still maps to a visible row, move `cursor_index` there and
    ///   return a clone of `target`.
    /// - If `target` is gone (deleted, or hidden inside a collapsed group) or is
    ///   `None`, clamp `cursor_index` into range and return whatever target now
    ///   sits under the cursor (or `None` when the tree is empty).
    ///
    /// Does NOT change collapse/expand state — a refresh must preserve the
    /// groups the user collapsed.
    pub fn anchor_to(
        &mut self,
        target: Option<&SelectionTarget>,
        tree: &[TreeNode],
    ) -> Option<SelectionTarget> {
        self.invalidate_cache();
        self.ensure_cache(tree);

        let count = self.cached_flat.len();
        if count == 0 {
            self.cursor_index = 0;
            return None;
        }

        // Preview is the source of truth: find the row matching it by ID.
        if let Some(t) = target {
            if let Some(i) = self.cached_flat.iter().position(|n| flat_target(n) == *t) {
                self.cursor_index = i;
                return Some(t.clone());
            }
        }

        // Target gone (or None): clamp and report what's under the cursor.
        if self.cursor_index >= count {
            self.cursor_index = count - 1;
        }
        self.cached_flat.get(self.cursor_index).map(flat_target)
    }
```

- [ ] **Step 6: Run the tests to verify they pass**

Run: `cargo test --lib tree_state::tests`
Expected: PASS — all `test_anchor_to_*` tests pass and existing `tree_state` tests (including `test_selected_target_group`, `test_selected_target_session`) still pass.

- [ ] **Step 7: Lint**

Run: `cargo clippy --lib -- -D warnings`
Expected: no warnings.

- [ ] **Step 8: Commit**

```bash
git add src/widgets/tree_state.rs
git commit -m "feat: add TreeState::anchor_to to re-align cursor with selection

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

---

## Task 2: Wire `anchor_to` into `App::refresh_tree`

**Files:**
- Modify: `src/app.rs` (`refresh_tree`, lines 1898-1906)

- [ ] **Step 1: Replace the refresh body**

Replace the body of `refresh_tree` (lines 1898-1906) with:

```rust
    pub(crate) fn refresh_tree(&mut self) {
        if let Ok(tree) = self.db.get_visible_tree(self.show_dead_sessions) {
            self.tree = tree;
            self.tree_state.invalidate_cache();
            self.cached_counts = count_sessions(&self.tree);

            // Keep the cursor anchored to the previewed session so the tmux
            // preview pane and the highlighted row never disagree (and `alt-d`
            // always deletes what the user sees).
            let resolved = self
                .tree_state
                .anchor_to(self.selection.selected.as_ref(), &self.tree);
            if resolved != self.selection.selected {
                // The previewed target was removed/hidden; the cursor landed on
                // a neighbor. Move the preview to match.
                self.selection.selected = resolved;
                self.refresh_cached_selected();
                self.sync_interactor_to_selection();
            } else {
                self.refresh_cached_selected();
            }

            self.dirty = true;
        }
    }
```

- [ ] **Step 2: Build and run the full test suite**

Run: `cargo test`
Expected: PASS — all ~160 tests pass (no behavioral regression).

- [ ] **Step 3: Lint and format**

Run: `cargo clippy -- -D warnings && cargo fmt --check`
Expected: no warnings; formatting clean. (If `fmt --check` reports diffs, run `cargo fmt` and re-run.)

- [ ] **Step 4: Manual smoke check (behavioral verification)**

Build and run the TUI, then verify the fix:

Run: `cargo build`
Then, in a real terminal session:
1. Create/have at least 3 sessions.
2. Select a session so its tmux output shows in the preview pane.
3. Trigger a refresh that reorders/removes rows (e.g. delete a *different* session, or let a session change active/detached status on the 2s poll).
4. Confirm the highlighted tree row still matches the previewed session.
5. Press `alt-d` and confirm the delete dialog names the previewed session.

Expected: highlight and preview stay together; `alt-d` targets the previewed session.

- [ ] **Step 5: Commit**

```bash
git add src/app.rs
git commit -m "fix: anchor tree cursor to previewed session on refresh

Prevents alt-d from deleting a different session than the one shown
in the preview pane when a tree refresh reorders or removes rows.

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

---

## Self-Review Notes

- **Spec coverage:** Cases 1-4 from the spec map to `anchor_to` branches (find-by-ID; group still exists — same find-by-ID path since groups are matched by `SelectionTarget::Group`; target gone → clamp + neighbor; no selection → clamp). `refresh_tree` wiring covers the preview/interactor sync. Tests cover find, clamp, no-auto-expand, empty.
- **No placeholders:** every code step is complete and copy-pasteable.
- **Type consistency:** `anchor_to(Option<&SelectionTarget>, &[TreeNode]) -> Option<SelectionTarget>` is used identically in Task 1 (definition/tests) and Task 2 (call site). `flat_target(&FlatNode) -> SelectionTarget` used in `selected_target` and `anchor_to`.
- **Group-target case:** matched implicitly — `flat_target` emits `SelectionTarget::Group` for group rows, so a `Group` preview target is re-found by the same `position(...)` lookup. No separate branch needed.
