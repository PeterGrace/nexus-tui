use std::collections::HashSet;

use crossterm::event::{KeyCode, KeyEvent};

use crate::types::{GroupIcon, GroupId, SelectionTarget, SessionSummary, TreeNode};

// ---------------------------------------------------------------------------
// Actions emitted by key handling
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
pub enum TreeAction {
    Select(SelectionTarget),
    ToggleExpand(GroupId),
    ScrollUp,
    ScrollDown,
}

// ---------------------------------------------------------------------------
// Flat node produced by visible_nodes()
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct FlatNode {
    pub depth: u16,
    pub node: FlatNodeKind,
}

#[derive(Debug, Clone)]
pub enum FlatNodeKind {
    Group {
        id: GroupId,
        name: String,
        icon: GroupIcon,
        child_count: usize,
        collapsed: bool,
    },
    Session {
        summary: Box<SessionSummary>,
    },
}

// ---------------------------------------------------------------------------
// Free function: flatten tree into visible nodes
// ---------------------------------------------------------------------------

fn flatten_tree(
    nodes: &[TreeNode],
    expanded: &HashSet<GroupId>,
    depth: u16,
    out: &mut Vec<FlatNode>,
) {
    for node in nodes {
        match node {
            TreeNode::Group(g) => {
                let is_expanded = expanded.contains(&g.id);
                out.push(FlatNode {
                    depth,
                    node: FlatNodeKind::Group {
                        id: g.id,
                        name: g.name.clone(),
                        icon: g.icon,
                        child_count: g.children.len(),
                        collapsed: !is_expanded,
                    },
                });
                if is_expanded {
                    flatten_tree(&g.children, expanded, depth + 1, out);
                }
            }
            TreeNode::Session(s) => {
                out.push(FlatNode {
                    depth,
                    node: FlatNodeKind::Session {
                        summary: Box::new(s.clone()),
                    },
                });
            }
        }
    }
}

/// Map a flattened node to its selection target.
fn flat_target(node: &FlatNode) -> SelectionTarget {
    match &node.node {
        FlatNodeKind::Group { id, .. } => SelectionTarget::Group(*id),
        FlatNodeKind::Session { summary } => SelectionTarget::Session(summary.session_id.clone()),
    }
}

// ---------------------------------------------------------------------------
// TreeState
// ---------------------------------------------------------------------------

pub struct TreeState {
    pub cursor_index: usize,
    pub expanded: HashSet<GroupId>,
    pub scroll_offset: usize,
    // Internal cache for visible nodes (Todo 014)
    cached_flat: Vec<FlatNode>,
    cache_valid: bool,
}

impl TreeState {
    /// Create a new TreeState with all groups expanded by default.
    pub fn new(tree: &[TreeNode]) -> Self {
        let mut expanded = HashSet::new();
        Self::collect_group_ids(tree, &mut expanded);
        let mut state = Self {
            cursor_index: 0,
            expanded,
            scroll_offset: 0,
            cached_flat: Vec::new(),
            cache_valid: false,
        };
        state.ensure_cache(tree);
        state
    }

    /// Recursively collect all group IDs so they start expanded.
    fn collect_group_ids(nodes: &[TreeNode], ids: &mut HashSet<GroupId>) {
        for node in nodes {
            if let TreeNode::Group(g) = node {
                ids.insert(g.id);
                Self::collect_group_ids(&g.children, ids);
            }
        }
    }

    /// Recompute the flat node cache if it has been invalidated.
    fn ensure_cache(&mut self, tree: &[TreeNode]) {
        if !self.cache_valid {
            self.cached_flat.clear();
            flatten_tree(tree, &self.expanded, 0, &mut self.cached_flat);
            self.cache_valid = true;
        }
    }

    /// Mark the cache as stale so it will be recomputed on next access.
    pub fn invalidate_cache(&mut self) {
        self.cache_valid = false;
    }

    /// Flatten the tree into a list of visible nodes, respecting collapsed state.
    /// This returns a freshly computed Vec (backward-compatible for external callers).
    pub fn visible_nodes(&self, tree: &[TreeNode]) -> Vec<FlatNode> {
        // If cache is valid, clone from cache to avoid recomputing
        if self.cache_valid {
            return self.cached_flat.clone();
        }
        let mut out = Vec::new();
        flatten_tree(tree, &self.expanded, 0, &mut out);
        out
    }

    /// Move cursor up, wrapping around to the bottom.
    pub fn move_cursor_up(&mut self, tree: &[TreeNode]) {
        self.ensure_cache(tree);
        let count = self.cached_flat.len();
        if count == 0 {
            return;
        }
        if self.cursor_index == 0 {
            self.cursor_index = count - 1;
        } else {
            self.cursor_index -= 1;
        }
    }

    /// Move cursor down, wrapping around to the top.
    pub fn move_cursor_down(&mut self, tree: &[TreeNode]) {
        self.ensure_cache(tree);
        let count = self.cached_flat.len();
        if count == 0 {
            return;
        }
        self.cursor_index = (self.cursor_index + 1) % count;
    }

    /// Toggle expand/collapse for a group.
    pub fn toggle_expand(&mut self, group_id: GroupId) {
        if self.expanded.contains(&group_id) {
            self.expanded.remove(&group_id);
        } else {
            self.expanded.insert(group_id);
        }
        self.invalidate_cache();
    }

    /// Get the selection target at the current cursor position.
    pub fn selected_target(&mut self, tree: &[TreeNode]) -> Option<SelectionTarget> {
        self.ensure_cache(tree);
        self.cached_flat.get(self.cursor_index).map(flat_target)
    }

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

    /// Handle a key event, returning an optional action.
    pub fn handle_key(&mut self, key: KeyEvent, tree: &[TreeNode]) -> Option<TreeAction> {
        match key.code {
            KeyCode::Char('j') | KeyCode::Down => {
                self.move_cursor_down(tree);
                Some(TreeAction::ScrollDown)
            }
            KeyCode::Char('k') | KeyCode::Up => {
                self.move_cursor_up(tree);
                Some(TreeAction::ScrollUp)
            }
            KeyCode::Enter => {
                self.ensure_cache(tree);
                if let Some(node) = self.cached_flat.get(self.cursor_index) {
                    match &node.node {
                        FlatNodeKind::Group { id, .. } => {
                            let gid = *id;
                            self.toggle_expand(gid);
                            Some(TreeAction::ToggleExpand(gid))
                        }
                        FlatNodeKind::Session { summary } => {
                            let target = SelectionTarget::Session(summary.session_id.clone());
                            Some(TreeAction::Select(target))
                        }
                    }
                } else {
                    None
                }
            }
            _ => None,
        }
    }

    /// Navigate cursor directly to a session by ID, auto-expanding its parent group.
    /// Returns true if the session was found and cursor was moved.
    pub fn jump_to_session(&mut self, session_id: &str, tree: &[TreeNode]) -> bool {
        // First, find and expand the parent group if the session is nested
        Self::expand_parent_of(session_id, tree, &mut self.expanded);
        self.invalidate_cache();
        self.ensure_cache(tree);

        // Find the session in the flat list and move cursor there
        for (i, flat) in self.cached_flat.iter().enumerate() {
            if let FlatNodeKind::Session { summary } = &flat.node {
                if summary.session_id == session_id {
                    self.cursor_index = i;
                    return true;
                }
            }
        }
        false
    }

    /// Recursively search for a session and expand its parent group.
    fn expand_parent_of(
        session_id: &str,
        nodes: &[TreeNode],
        expanded: &mut HashSet<GroupId>,
    ) -> bool {
        for node in nodes {
            if let TreeNode::Group(g) = node {
                for child in &g.children {
                    match child {
                        TreeNode::Session(s) if s.session_id == session_id => {
                            expanded.insert(g.id);
                            return true;
                        }
                        TreeNode::Group(_) => {
                            if Self::expand_parent_of(
                                session_id,
                                std::slice::from_ref(child),
                                expanded,
                            ) {
                                expanded.insert(g.id);
                                return true;
                            }
                        }
                        _ => {}
                    }
                }
            }
        }
        false
    }

    /// Ensure scroll_offset keeps cursor visible in the given viewport height.
    pub fn ensure_cursor_visible(&mut self, viewport_height: usize) {
        if viewport_height == 0 {
            return;
        }
        if self.cursor_index < self.scroll_offset {
            self.scroll_offset = self.cursor_index;
        } else if self.cursor_index >= self.scroll_offset + viewport_height {
            self.scroll_offset = self.cursor_index - viewport_height + 1;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mock;

    #[test]
    fn test_visible_nodes_all_expanded() {
        let tree = mock::mock_tree();
        let state = TreeState::new(&tree);
        let flat = state.visible_nodes(&tree);

        // mock_tree has: nexus(group) + 2 sessions, website(group) + 1 session +
        // api-work(group) + 1 session, ungrouped(group) + 1 session
        // All expanded: 3 groups + 5 sessions + 1 subgroup = 9 nodes
        // nexus, feat/scanner, fix/render-tick,
        // website, redesign-landing, api-work, api-auth-endpoints,
        // ungrouped, quick-question
        assert_eq!(flat.len(), 9);
    }

    #[test]
    fn test_visible_nodes_collapsed_group() {
        let tree = mock::mock_tree();
        let mut state = TreeState::new(&tree);

        // Collapse nexus group (id=1) -- hides 2 sessions
        state.toggle_expand(1);
        let flat = state.visible_nodes(&tree);
        // Was 9, minus 2 children of nexus = 7
        assert_eq!(flat.len(), 7);
    }

    #[test]
    fn test_cursor_wraps_down() {
        let tree = mock::mock_tree();
        let mut state = TreeState::new(&tree);
        let count = state.visible_nodes(&tree).len();

        state.cursor_index = count - 1;
        state.move_cursor_down(&tree);
        assert_eq!(state.cursor_index, 0);
    }

    #[test]
    fn test_cursor_wraps_up() {
        let tree = mock::mock_tree();
        let mut state = TreeState::new(&tree);

        state.cursor_index = 0;
        state.move_cursor_up(&tree);
        let count = state.visible_nodes(&tree).len();
        assert_eq!(state.cursor_index, count - 1);
    }

    #[test]
    fn test_toggle_expand() {
        let tree = mock::mock_tree();
        let mut state = TreeState::new(&tree);

        assert!(state.expanded.contains(&1));
        state.toggle_expand(1);
        assert!(!state.expanded.contains(&1));
        state.toggle_expand(1);
        assert!(state.expanded.contains(&1));
    }

    #[test]
    fn test_selected_target_group() {
        let tree = mock::mock_tree();
        let mut state = TreeState::new(&tree);
        state.cursor_index = 0; // first node is nexus group
        let target = state.selected_target(&tree);
        assert_eq!(target, Some(SelectionTarget::Group(1)));
    }

    #[test]
    fn test_selected_target_session() {
        let tree = mock::mock_tree();
        let mut state = TreeState::new(&tree);
        state.cursor_index = 1; // second node is feat/scanner session
        let target = state.selected_target(&tree);
        assert_eq!(
            target,
            Some(SelectionTarget::Session(
                "a1b2c3d4-e5f6-7890-abcd-ef1234567890".to_string()
            ))
        );
    }

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

        assert!(
            state.cursor_index < count,
            "cursor must be clamped in range"
        );
        assert_eq!(state.cursor_index, count - 1);
        assert_ne!(resolved, Some(gone), "must not return the missing target");
        assert!(
            resolved.is_some(),
            "a surviving neighbor target is returned"
        );
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

        assert!(
            !state.expanded.contains(&1),
            "must not auto-expand the group"
        );
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

    #[test]
    fn test_anchor_to_finds_group_by_id() {
        // Group targets re-anchor through the same lookup as sessions.
        let tree = mock::mock_tree();
        let mut state = TreeState::new(&tree);
        state.cursor_index = 4; // somewhere unrelated

        let target = SelectionTarget::Group(1); // nexus group at flat index 0
        let resolved = state.anchor_to(Some(&target), &tree);

        assert_eq!(state.cursor_index, 0);
        assert_eq!(resolved, Some(target));
    }

    #[test]
    fn test_flat_node_depths() {
        let tree = mock::mock_tree();
        let state = TreeState::new(&tree);
        let flat = state.visible_nodes(&tree);

        // nexus group at depth 0
        assert_eq!(flat[0].depth, 0);
        // its children at depth 1
        assert_eq!(flat[1].depth, 1);
        assert_eq!(flat[2].depth, 1);
        // website group at depth 0
        assert_eq!(flat[3].depth, 0);
        // website child sessions at depth 1
        assert_eq!(flat[4].depth, 1);
        // api-work subgroup at depth 1
        assert_eq!(flat[5].depth, 1);
        // api-work child at depth 2
        assert_eq!(flat[6].depth, 2);
    }

    #[test]
    fn test_ensure_cursor_visible() {
        let tree = mock::mock_tree();
        let mut state = TreeState::new(&tree);
        state.cursor_index = 8;
        state.scroll_offset = 0;
        state.ensure_cursor_visible(5);
        // cursor 8 needs offset at least 8-5+1=4
        assert_eq!(state.scroll_offset, 4);
    }

    #[test]
    fn test_handle_key_j_moves_down() {
        let tree = mock::mock_tree();
        let mut state = TreeState::new(&tree);
        state.cursor_index = 0;

        let key = KeyEvent::from(KeyCode::Char('j'));
        let action = state.handle_key(key, &tree);
        assert_eq!(action, Some(TreeAction::ScrollDown));
        assert_eq!(state.cursor_index, 1);
    }

    #[test]
    fn test_handle_key_enter_toggles_group() {
        let tree = mock::mock_tree();
        let mut state = TreeState::new(&tree);
        state.cursor_index = 0; // nexus group

        let key = KeyEvent::from(KeyCode::Enter);
        let action = state.handle_key(key, &tree);
        assert_eq!(action, Some(TreeAction::ToggleExpand(1)));
        assert!(!state.expanded.contains(&1));
    }

    #[test]
    fn test_cache_invalidated_on_toggle() {
        let tree = mock::mock_tree();
        let mut state = TreeState::new(&tree);

        // Cache starts valid after new()
        assert!(state.cache_valid);

        // Toggle invalidates
        state.toggle_expand(1);
        assert!(!state.cache_valid);

        // ensure_cache rebuilds
        state.ensure_cache(&tree);
        assert!(state.cache_valid);
        assert_eq!(state.cached_flat.len(), 7); // nexus collapsed: 9-2=7
    }

    #[test]
    fn test_visible_nodes_uses_cache_when_valid() {
        let tree = mock::mock_tree();
        let state = TreeState::new(&tree);

        // cache_valid should be true after new()
        assert!(state.cache_valid);

        let flat = state.visible_nodes(&tree);
        assert_eq!(flat.len(), 9);
    }

    #[test]
    fn test_jump_to_session_in_expanded_group() {
        let tree = mock::mock_tree();
        let mut state = TreeState::new(&tree);
        state.cursor_index = 0;

        // Jump to "redesign-landing" (in "website" group, already expanded)
        let found = state.jump_to_session("c3d4e5f6-a7b8-9012-cdef-123456789012", &tree);
        assert!(found);
        // Verify cursor is on that session
        if let FlatNodeKind::Session { summary } = &state.cached_flat[state.cursor_index].node {
            assert_eq!(summary.display_name, "redesign-landing");
        } else {
            panic!("cursor not on a session");
        }
    }

    #[test]
    fn test_jump_to_session_auto_expands_collapsed_group() {
        let tree = mock::mock_tree();
        let mut state = TreeState::new(&tree);

        // Collapse the "nexus" group (id=1)
        state.toggle_expand(1);
        assert!(!state.expanded.contains(&1));

        // Jump to "feat/scanner" which is in the collapsed nexus group
        let found = state.jump_to_session("a1b2c3d4-e5f6-7890-abcd-ef1234567890", &tree);
        assert!(found);
        // Group should now be expanded
        assert!(state.expanded.contains(&1));
        // Cursor should be on feat/scanner
        if let FlatNodeKind::Session { summary } = &state.cached_flat[state.cursor_index].node {
            assert_eq!(summary.display_name, "feat/scanner");
        } else {
            panic!("cursor not on a session");
        }
    }

    #[test]
    fn test_jump_to_ungrouped_session() {
        let tree = mock::mock_tree();
        let mut state = TreeState::new(&tree);

        // Jump to "quick-question" (in Ungrouped group)
        let found = state.jump_to_session("e5f6a7b8-c9d0-1234-efab-345678901234", &tree);
        assert!(found);
        if let FlatNodeKind::Session { summary } = &state.cached_flat[state.cursor_index].node {
            assert_eq!(summary.display_name, "quick-question");
        } else {
            panic!("cursor not on a session");
        }
    }

    #[test]
    fn test_jump_to_session_in_subgroup() {
        let tree = mock::mock_tree();
        let mut state = TreeState::new(&tree);

        // Collapse api-work subgroup (id=3)
        state.toggle_expand(3);
        assert!(!state.expanded.contains(&3));

        // Jump to "api-auth-endpoints" in the collapsed subgroup
        let found = state.jump_to_session("d4e5f6a7-b8c9-0123-defa-234567890123", &tree);
        assert!(found);
        assert!(state.expanded.contains(&3));
        if let FlatNodeKind::Session { summary } = &state.cached_flat[state.cursor_index].node {
            assert_eq!(summary.display_name, "api-auth-endpoints");
        } else {
            panic!("cursor not on a session");
        }
    }

    #[test]
    fn test_jump_to_nonexistent_session() {
        let tree = mock::mock_tree();
        let mut state = TreeState::new(&tree);
        state.cursor_index = 3;

        let found = state.jump_to_session("nonexistent-id", &tree);
        assert!(!found);
        // Cursor should not have moved
        assert_eq!(state.cursor_index, 3);
    }
}
