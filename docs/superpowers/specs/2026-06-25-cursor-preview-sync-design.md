# Keep tree cursor anchored to the previewed session

**Date:** 2026-06-25
**Status:** Approved — ready for implementation plan

## Problem

Deleting a session with `alt-d` can delete a *different* session than the one
shown in the tmux preview pane. The user sees session A previewed, presses
`alt-d`, and session B is deleted.

### Root cause

Nexus has two separate sources of truth for "the selected session":

- **`App.selection.selected`** — a `SelectionTarget` tracked by stable **ID**.
  Drives the tmux preview pane via `cached_selected` /
  `refresh_cached_selected` (`app.rs:1917`).
- **`TreeState.cursor_index`** — the highlighted tree row, tracked by
  **positional index**. This is what `start_delete` reads via
  `selected_target` (`app.rs:1447`), and likewise `kill_tmux_session` and
  fullscreen attach.

Normal keyboard/mouse navigation keeps the two in sync through
`handle_tree_action` (`app.rs:846`), which updates `selection.selected`
whenever the cursor moves.

The drift happens in `refresh_tree` (`app.rs:1898`). It rebuilds the tree,
invalidates the flat-node cache, and re-anchors the **preview by ID**
(`refresh_cached_selected`) — but leaves **`cursor_index` at its old numeric
position**. Any refresh that reorders or removes rows (a session changing
active/detached status, another session being deleted, a new session
appearing) leaves the cursor pointing at a *different* row than the one
previewed. `alt-d` then acts on the cursor, deleting the wrong session.

## Goal

Make "preview == cursor == action target" an invariant. The tmux preview pane
must always match the highlighted tree row, so that delete / kill / attach
always act on what the user sees.

## Approach

Chosen strategy (from brainstorming): **cursor follows the preview by ID.** The
preview (`selection.selected`) is the source of truth; after any tree refresh,
the cursor is re-anchored to the row matching it.

Navigation already keeps the two in sync, so the only drift source is
`refresh_tree`. Closing that single gap is sufficient.

### Reconciliation step

Add one reconciliation step to `refresh_tree`, run after the tree is rebuilt
and the cache invalidated, replacing the bare `refresh_cached_selected` call:

1. **Selected session still exists** → move `cursor_index` to its flat row.
   Cursor lands on the previewed session.
2. **Selected node is a group that still exists** → move cursor to that group's
   row.
3. **Selected target is gone** (the common post-delete case) → clamp
   `cursor_index` to the nearest valid row, then set `selection.selected` to
   whatever target now sits under the cursor, and refresh the preview +
   interactor. After a delete, cursor and preview both land on the neighbor.
4. **No selection** → leave `cursor_index` as-is, clamped to current bounds.

Re-anchoring must **not auto-expand** collapsed groups. The existing
`jump_to_session` helper expands the parent group as a side effect (it is meant
for explicit user jumps); reconciliation must preserve the user's collapse
state. The new logic reuses only the flat-list ID lookup, not the
expand-parent behavior.

### Component design

New private method on `TreeState`:

```rust
/// Re-anchor the cursor to `target` after the tree changed.
///
/// - If `target` still maps to a row, move the cursor there and return it.
/// - If `target` is gone (or `None`), clamp the cursor to a valid row and
///   return whatever target now sits under it (or `None` if the tree is empty).
///
/// Does NOT change collapse/expand state.
fn anchor_to(
    &mut self,
    target: Option<&SelectionTarget>,
    tree: &[TreeNode],
) -> Option<SelectionTarget>;
```

`refresh_tree` then:

```rust
let resolved = self.tree_state.anchor_to(self.selection.selected.as_ref(), &self.tree);
if resolved != self.selection.selected {
    self.selection.selected = resolved;
    // case 3: target changed under us
    self.refresh_cached_selected();
    self.sync_interactor_to_selection();
} else {
    self.refresh_cached_selected();
}
```

(Exact wiring may be simplified during implementation, but the contract is:
after `refresh_tree`, `cursor_index`, `selection.selected`, and `cached_selected`
all agree.)

## Data flow

```
refresh_tree
  → rebuild tree from db, invalidate flat cache
  → anchor_to(selection.selected)        // re-align cursor to preview by ID
      ├─ found        → cursor moved to that row;  selection unchanged
      └─ gone/none    → cursor clamped;            selection updated to neighbor
  → refresh_cached_selected               // preview matches cursor
  → (if selection changed) sync_interactor_to_selection
```

## Error handling / edge cases

- **Empty tree** → `anchor_to` returns `None`; cursor clamps to 0; preview
  clears. No panic (cursor reads use `.get()`).
- **Collapsed parent group** → reconciliation does not expand it. If the
  selected session is currently hidden inside a collapsed group, it has no flat
  row; treat as case 3 (clamp to nearest visible row, update selection). This
  matches the visible-tree semantics already used elsewhere.
- **Group deleted** → same as case 3 for a `Group` target.

## Testing

Unit tests in `src/widgets/tree_state.rs` (`mod tests`), no tmux required:

1. **Moved session is re-found** — build a tree, select a session by ID,
   reorder the underlying nodes, `anchor_to` moves `cursor_index` to the new
   row and returns the same target.
2. **Deleted session clamps to neighbor** — select a session, remove it from
   the tree, `anchor_to` returns a *different* (neighbor) target and the cursor
   index is in bounds.
3. **Collapse state preserved** — with a collapsed group containing the
   selected session, `anchor_to` does not expand the group (assert
   `expanded` unchanged) and treats the hidden session as case 3.
4. **Empty tree** — `anchor_to` returns `None` and does not panic.

Existing tests (`test_selected_target_session`, reconcile tests) must still
pass. Full suite: `cargo test`, plus `cargo clippy -- -D warnings` and
`cargo fmt --check`.

## Out of scope

- Collapsing `selection.selected` and `cursor_index` into a single field
  (larger refactor; not needed to fix this bug).
- Changing delete confirmation UX or worktree teardown flow.
- README changes — this is a bug fix with no user-facing keybinding or feature
  change.
