use std::path::Path;

use color_eyre::eyre::WrapErr;
use color_eyre::Result;
use rusqlite::{params, Connection};

use crate::types::{
    GroupIcon, GroupId, GroupNode, SessionOrigin, SessionStatus, SessionSummary, TreeNode,
    WorktreeInfo,
};

// ---------------------------------------------------------------------------
// Schema SQL
// ---------------------------------------------------------------------------

const SCHEMA_SQL: &str = r#"
CREATE TABLE IF NOT EXISTS sessions (
    session_id    TEXT PRIMARY KEY,
    display_name  TEXT NOT NULL,
    cwd           TEXT,
    last_active   TEXT NOT NULL,
    is_active     INTEGER NOT NULL DEFAULT 0,
    tmux_name     TEXT,
    status        TEXT NOT NULL DEFAULT 'dead',
    created_by    TEXT NOT NULL DEFAULT 'scanner',
    created_at    TEXT NOT NULL DEFAULT '',
    worktree_branch    TEXT,
    worktree_repo_root TEXT
);

CREATE TABLE IF NOT EXISTS groups (
    id         INTEGER PRIMARY KEY AUTOINCREMENT,
    name       TEXT NOT NULL UNIQUE,
    icon       TEXT NOT NULL DEFAULT '',
    sort_order INTEGER NOT NULL DEFAULT 0
);

CREATE TABLE IF NOT EXISTS session_groups (
    session_id TEXT NOT NULL,
    group_id   INTEGER NOT NULL,
    PRIMARY KEY (session_id, group_id),
    FOREIGN KEY (session_id) REFERENCES sessions(session_id) ON DELETE CASCADE,
    FOREIGN KEY (group_id) REFERENCES groups(id) ON DELETE CASCADE
);

CREATE TABLE IF NOT EXISTS settings (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
);
"#;

// ---------------------------------------------------------------------------
// Database wrapper
// ---------------------------------------------------------------------------

pub struct Database {
    conn: Connection,
}

impl Database {
    /// Open (or create) a database at the given file path.
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .wrap_err_with(|| format!("cannot create db directory {}", parent.display()))?;
        }

        let conn = Connection::open(path)
            .wrap_err_with(|| format!("cannot open database at {}", path.display()))?;

        conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA foreign_keys=ON;")?;

        let db = Self { conn };
        db.init_schema()?;
        Ok(db)
    }

    /// Open an in-memory database (for tests).
    #[cfg(test)]
    pub fn open_in_memory() -> Result<Self> {
        let conn = Connection::open_in_memory().wrap_err("cannot open in-memory database")?;

        conn.execute_batch("PRAGMA foreign_keys=ON;")?;

        let db = Self { conn };
        db.init_schema()?;
        Ok(db)
    }

    /// Create all tables if they don't already exist, then migrate.
    pub fn init_schema(&self) -> Result<()> {
        self.conn
            .execute_batch(SCHEMA_SQL)
            .wrap_err("failed to initialise database schema")?;
        self.migrate()?;
        Ok(())
    }

    /// Add columns that may be missing from a pre-overhaul database.
    fn migrate(&self) -> Result<()> {
        // Each ALTER is a no-op if the column already exists (duplicate column error → skip).
        let additions = [
            "ALTER TABLE sessions ADD COLUMN tmux_name TEXT",
            "ALTER TABLE sessions ADD COLUMN status TEXT NOT NULL DEFAULT 'dead'",
            "ALTER TABLE sessions ADD COLUMN created_by TEXT NOT NULL DEFAULT 'scanner'",
            "ALTER TABLE sessions ADD COLUMN created_at TEXT NOT NULL DEFAULT ''",
            "ALTER TABLE sessions ADD COLUMN claude_session_id TEXT",
            "ALTER TABLE sessions ADD COLUMN worktree_branch TEXT",
            "ALTER TABLE sessions ADD COLUMN worktree_repo_root TEXT",
            "ALTER TABLE sessions ADD COLUMN launch_command TEXT NOT NULL DEFAULT 'claude'",
        ];
        for sql in &additions {
            match self.conn.execute_batch(sql) {
                Ok(()) => {}
                Err(e) if e.to_string().contains("duplicate column") => {}
                Err(e) => return Err(e).wrap_err("migration failed"),
            }
        }
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Settings (key-value)
    // -----------------------------------------------------------------------

    pub fn get_setting(&self, key: &str) -> Result<Option<String>> {
        let mut stmt = self
            .conn
            .prepare("SELECT value FROM settings WHERE key = ?1")?;
        let result = stmt
            .query_row(params![key], |row| row.get::<_, String>(0))
            .ok();
        Ok(result)
    }

    pub fn set_setting(&self, key: &str, value: &str) -> Result<()> {
        self.conn.execute(
            "INSERT OR REPLACE INTO settings (key, value) VALUES (?1, ?2)",
            params![key, value],
        )?;
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Session CRUD
    // -----------------------------------------------------------------------

    /// Create a new Nexus-managed session and return its UUID.
    pub fn create_nexus_session(
        &self,
        name: &str,
        cwd: &str,
        tmux_name: &str,
        command: &str,
        worktree: Option<&WorktreeInfo>,
    ) -> Result<String> {
        let id = uuid::Uuid::new_v4().to_string();
        let now = crate::time_utils::epoch_to_iso(crate::time_utils::now_epoch());

        let wt_branch = worktree.map(|w| w.branch.as_str());
        let wt_repo = worktree.map(|w| w.repo_root.to_string_lossy().to_string());

        self.conn.execute(
            "INSERT INTO sessions
                (session_id, display_name, cwd, last_active, is_active,
                 tmux_name, status, created_by, created_at,
                 worktree_branch, worktree_repo_root, launch_command)
             VALUES (?1, ?2, ?3, ?4, 1, ?5, 'active', 'nexus', ?6, ?7, ?8, ?9)",
            params![id, name, cwd, now, tmux_name, now, wt_branch, wt_repo, command],
        )?;

        Ok(id)
    }

    /// Update a session's status.
    pub fn update_session_status(&self, session_id: &str, status: SessionStatus) -> Result<()> {
        let is_active: i32 = if status == SessionStatus::Active {
            1
        } else {
            0
        };
        self.conn.execute(
            "UPDATE sessions SET status = ?1, is_active = ?2 WHERE session_id = ?3",
            params![status.as_str(), is_active, session_id],
        )?;
        Ok(())
    }

    /// Update a session's display name and tmux name.
    pub fn update_session_name(
        &self,
        session_id: &str,
        new_name: &str,
        new_tmux_name: &str,
    ) -> Result<()> {
        self.conn.execute(
            "UPDATE sessions SET display_name = ?1, tmux_name = ?2 WHERE session_id = ?3",
            params![new_name, new_tmux_name, session_id],
        )?;
        Ok(())
    }

    /// Delete a session entirely (cascades to session_groups).
    pub fn delete_session(&self, session_id: &str) -> Result<()> {
        self.conn.execute(
            "DELETE FROM sessions WHERE session_id = ?1",
            params![session_id],
        )?;
        Ok(())
    }

    /// Store the Claude Code session ID for a Nexus session.
    pub fn set_claude_session_id(&self, session_id: &str, claude_id: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE sessions SET claude_session_id = ?1 WHERE session_id = ?2",
            params![claude_id, session_id],
        )?;
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Group CRUD
    // -----------------------------------------------------------------------

    /// Create a group and return its id.
    pub fn create_group(&self, name: &str, icon: &str) -> Result<i64> {
        let max_order: i64 = self
            .conn
            .query_row("SELECT COALESCE(MAX(sort_order), 0) FROM groups", [], |r| {
                r.get(0)
            })
            .unwrap_or(0);

        self.conn.execute(
            "INSERT INTO groups (name, icon, sort_order) VALUES (?1, ?2, ?3)",
            params![name, icon, max_order + 1],
        )?;

        Ok(self.conn.last_insert_rowid())
    }

    /// Delete a group by id. Assignments referencing it are cascade-deleted.
    pub fn delete_group(&self, id: i64) -> Result<()> {
        self.conn
            .execute("DELETE FROM groups WHERE id = ?1", params![id])?;
        Ok(())
    }

    /// Rename a group.
    pub fn rename_group(&self, group_id: GroupId, new_name: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE groups SET name = ?1 WHERE id = ?2",
            params![new_name, group_id],
        )?;
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Session <-> Group assignment
    // -----------------------------------------------------------------------

    /// Assign a session to a group.
    pub fn assign_session_to_group(&self, session_id: &str, group_id: i64) -> Result<()> {
        self.conn.execute(
            "INSERT OR IGNORE INTO session_groups (session_id, group_id)
             VALUES (?1, ?2)",
            params![session_id, group_id],
        )?;
        Ok(())
    }

    /// Remove a session from all groups.
    pub fn unassign_session(&self, session_id: &str) -> Result<()> {
        self.conn.execute(
            "DELETE FROM session_groups WHERE session_id = ?1",
            params![session_id],
        )?;
        Ok(())
    }

    /// Move a session to a different group (remove old assignments, add new).
    pub fn move_session_to_group(&self, session_id: &str, new_group_id: GroupId) -> Result<()> {
        self.unassign_session(session_id)?;
        self.assign_session_to_group(session_id, new_group_id)?;
        Ok(())
    }

    /// Return session_ids that are not assigned to any group.
    #[cfg(test)]
    pub fn get_ungrouped_sessions(&self) -> Result<Vec<String>> {
        let mut stmt = self.conn.prepare(
            "SELECT session_id FROM sessions
             WHERE session_id NOT IN (SELECT session_id FROM session_groups)
             ORDER BY last_active DESC",
        )?;

        let ids = stmt
            .query_map([], |row| row.get::<_, String>(0))?
            .filter_map(|r| r.ok())
            .collect();

        Ok(ids)
    }

    // -----------------------------------------------------------------------
    // Tree building
    // -----------------------------------------------------------------------

    /// Build the tree, optionally filtering out dead sessions.
    pub fn get_visible_tree(&self, show_dead: bool) -> Result<Vec<TreeNode>> {
        let status_filter = if show_dead {
            ""
        } else {
            "AND s.status != 'dead'"
        };

        // Fetch all groups
        let mut group_stmt = self
            .conn
            .prepare("SELECT id, name, icon, sort_order FROM groups ORDER BY sort_order")?;

        let groups: Vec<(i64, String, String)> = group_stmt
            .query_map([], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                ))
            })?
            .filter_map(|r| r.ok())
            .collect();

        // Fetch ALL grouped sessions in one query
        let grouped_sql = format!(
            "SELECT sg.group_id, s.session_id, s.display_name, s.cwd,
                    s.last_active, s.is_active,
                    s.tmux_name, s.status, s.created_by, s.created_at,
                    s.claude_session_id,
                    s.worktree_branch, s.worktree_repo_root,
                    s.launch_command
             FROM sessions s
             JOIN session_groups sg ON s.session_id = sg.session_id
             WHERE 1=1 {status_filter}
             ORDER BY sg.group_id, s.last_active DESC"
        );
        let mut sess_stmt = self.conn.prepare(&grouped_sql)?;

        let mut group_children: std::collections::HashMap<i64, Vec<TreeNode>> =
            std::collections::HashMap::new();
        sess_stmt
            .query_map([], |row| {
                let gid: i64 = row.get(0)?;
                let summary = row_to_summary_at(row, 1);
                Ok((gid, summary))
            })?
            .filter_map(|r| r.ok())
            .for_each(|(gid, summary)| {
                group_children
                    .entry(gid)
                    .or_default()
                    .push(TreeNode::Session(summary));
            });

        // Build tree from groups
        let mut tree: Vec<TreeNode> = Vec::new();
        for (gid, gname, _icon) in &groups {
            let children = group_children.remove(gid).unwrap_or_default();
            tree.push(TreeNode::Group(GroupNode {
                id: *gid,
                name: gname.clone(),
                icon: GroupIcon::SubGroup,
                children,
            }));
        }

        // Ungrouped sessions
        let ungrouped = self.ungrouped_session_summaries(show_dead)?;
        if !ungrouped.is_empty() {
            tree.push(TreeNode::Group(GroupNode {
                id: 0,
                name: "Ungrouped".to_string(),
                icon: GroupIcon::Root,
                children: ungrouped.into_iter().map(TreeNode::Session).collect(),
            }));
        }

        Ok(tree)
    }

    /// Build the full tree (all sessions including dead). Backward-compat wrapper.
    pub fn get_tree(&self) -> Result<Vec<TreeNode>> {
        self.get_visible_tree(true)
    }

    /// Return all groups as (id, name) pairs, for the group picker.
    pub fn get_all_groups(&self) -> Result<Vec<(GroupId, String)>> {
        let mut stmt = self
            .conn
            .prepare("SELECT id, name FROM groups ORDER BY sort_order")?;
        let groups = stmt
            .query_map([], |row| {
                Ok((row.get::<_, GroupId>(0)?, row.get::<_, String>(1)?))
            })?
            .filter_map(|r| r.ok())
            .collect();
        Ok(groups)
    }

    // -----------------------------------------------------------------------
    // Tmux name uniqueness
    // -----------------------------------------------------------------------

    /// Return a tmux_name guaranteed unique among existing sessions.
    /// If `base` is free, returns it unchanged; otherwise appends `-2`, `-3`, …
    /// Optionally excludes a session_id (for renames — the session's own row shouldn't block itself).
    pub fn next_unique_tmux_name(&self, base: &str, exclude_id: Option<&str>) -> Result<String> {
        let mut stmt = self
            .conn
            .prepare("SELECT tmux_name FROM sessions WHERE tmux_name IS NOT NULL")?;

        let existing: std::collections::HashSet<String> = stmt
            .query_map([], |row| row.get::<_, String>(0))?
            .filter_map(|r| r.ok())
            .collect();

        // If excluding a session, fetch its current tmux_name so we don't count it.
        let excluded_name: Option<String> = if let Some(eid) = exclude_id {
            self.conn
                .prepare("SELECT tmux_name FROM sessions WHERE session_id = ?1")?
                .query_row(params![eid], |row| row.get::<_, Option<String>>(0))
                .ok()
                .flatten()
        } else {
            None
        };

        let is_taken = |name: &str| -> bool {
            if let Some(ref exc) = excluded_name {
                if name == exc {
                    return false;
                }
            }
            existing.contains(name)
        };

        if !is_taken(base) {
            return Ok(base.to_string());
        }

        let mut suffix = 2u32;
        loop {
            let candidate = format!("{base}-{suffix}");
            if !is_taken(&candidate) {
                return Ok(candidate);
            }
            suffix += 1;
        }
    }

    // -----------------------------------------------------------------------
    // Worktree management
    // -----------------------------------------------------------------------

    /// Clear stale worktree columns for sessions whose cwd directory no longer exists.
    /// Called at startup for crash recovery.
    pub fn reconcile_worktrees(&self) -> Result<usize> {
        let mut stmt = self
            .conn
            .prepare("SELECT session_id, cwd FROM sessions WHERE worktree_branch IS NOT NULL")?;

        let stale: Vec<String> = stmt
            .query_map([], |row| {
                let id: String = row.get(0)?;
                let cwd: Option<String> = row.get(1)?;
                Ok((id, cwd))
            })?
            .filter_map(|r| r.ok())
            .filter(|(_, cwd)| {
                cwd.as_ref()
                    .map(|p| !std::path::Path::new(p).exists())
                    .unwrap_or(true)
            })
            .map(|(id, _)| id)
            .collect();

        let count = stale.len();
        for id in &stale {
            self.clear_worktree_columns(id)?;
        }
        Ok(count)
    }

    /// Clear the worktree columns for a session (mark as no longer worktree-isolated).
    pub fn clear_worktree_columns(&self, session_id: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE sessions SET worktree_branch = NULL, worktree_repo_root = NULL
             WHERE session_id = ?1",
            params![session_id],
        )?;
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Internal helpers
    // -----------------------------------------------------------------------

    /// Fetch sessions not assigned to any group.
    fn ungrouped_session_summaries(&self, show_dead: bool) -> Result<Vec<SessionSummary>> {
        let status_filter = if show_dead {
            ""
        } else {
            "AND s.status != 'dead'"
        };
        let sql = format!(
            "SELECT s.session_id, s.display_name, s.cwd,
                    s.last_active, s.is_active,
                    s.tmux_name, s.status, s.created_by, s.created_at,
                    s.claude_session_id,
                    s.worktree_branch, s.worktree_repo_root,
                    s.launch_command
             FROM sessions s
             WHERE s.session_id NOT IN (SELECT session_id FROM session_groups)
             {status_filter}
             ORDER BY s.last_active DESC"
        );
        let mut stmt = self.conn.prepare(&sql)?;

        let rows: Vec<SessionSummary> = stmt
            .query_map([], |row| Ok(row_to_summary(row)))?
            .filter_map(|r| r.ok())
            .collect();

        Ok(rows)
    }

    /// Look up a session's cwd by its id.
    pub fn get_session_cwd(&self, session_id: &str) -> Result<Option<String>> {
        let mut stmt = self
            .conn
            .prepare("SELECT cwd FROM sessions WHERE session_id = ?1")?;

        let result = stmt
            .query_row(params![session_id], |row| row.get::<_, Option<String>>(0))
            .ok()
            .flatten();

        Ok(result)
    }

    /// Look up a group id by name, returning `None` if not found.
    pub fn get_group_id_by_name(&self, name: &str) -> Result<Option<i64>> {
        let mut stmt = self.conn.prepare("SELECT id FROM groups WHERE name = ?1")?;

        let result = stmt
            .query_row(params![name], |row| row.get::<_, i64>(0))
            .ok();

        Ok(result)
    }
}

// ---------------------------------------------------------------------------
// Free helpers
// ---------------------------------------------------------------------------

/// Map a rusqlite Row into a `SessionSummary`, reading 12 columns starting at column 0.
fn row_to_summary(row: &rusqlite::Row<'_>) -> SessionSummary {
    row_to_summary_at(row, 0)
}

/// Map a rusqlite Row into a `SessionSummary`, reading 12 columns starting at
/// the given `start` offset.
///
/// Column layout (relative to `start`):
///   0: session_id, 1: display_name, 2: cwd, 3: last_active, 4: is_active,
///   5: tmux_name, 6: status, 7: created_by, 8: created_at, 9: claude_session_id,
///   10: worktree_branch, 11: worktree_repo_root, 12: launch_command
fn row_to_summary_at(row: &rusqlite::Row<'_>, start: usize) -> SessionSummary {
    let cwd_str: Option<String> = row.get(start + 2).unwrap_or(None);
    let status_str: String = row.get(start + 6).unwrap_or_default();
    let created_by_str: String = row.get(start + 7).unwrap_or_default();

    // Worktree: both columns must be present for a valid WorktreeInfo
    let wt_branch: Option<String> = row.get(start + 10).unwrap_or(None);
    let wt_repo: Option<String> = row.get(start + 11).unwrap_or(None);
    let worktree = match (wt_branch, wt_repo) {
        (Some(branch), Some(repo)) => Some(WorktreeInfo {
            branch,
            repo_root: std::path::PathBuf::from(repo),
        }),
        _ => None,
    };

    SessionSummary {
        session_id: row.get(start).unwrap_or_default(),
        display_name: row.get(start + 1).unwrap_or_default(),
        cwd: cwd_str.map(std::path::PathBuf::from),
        last_active: row.get(start + 3).unwrap_or_default(),
        is_active: row.get::<_, i32>(start + 4).unwrap_or(0) != 0,
        tmux_name: row.get(start + 5).unwrap_or(None),
        status: SessionStatus::from_str(&status_str),
        created_by: SessionOrigin::from_str(&created_by_str),
        created_at: row.get(start + 8).unwrap_or_default(),
        claude_session_id: row.get(start + 9).unwrap_or(None),
        worktree,
        launch_command: row
            .get::<_, Option<String>>(start + 12)
            .ok()
            .flatten()
            .unwrap_or_else(|| "claude".to_string()),
        jsonl_path: None,
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // Helper: shorthand for creating sessions without worktree
    fn create_session(db: &Database, name: &str, cwd: &str, tmux: &str) -> String {
        db.create_nexus_session(name, cwd, tmux, "claude", None)
            .unwrap()
    }

    #[test]
    fn test_init_schema_idempotent() {
        let db = Database::open_in_memory().unwrap();
        db.init_schema().unwrap();
        db.init_schema().unwrap();
    }

    #[test]
    fn test_create_nexus_session() {
        let db = Database::open_in_memory().unwrap();
        let id = create_session(&db, "my-session", "/tmp/project", "my-session");
        assert!(!id.is_empty());

        let ungrouped = db.get_ungrouped_sessions().unwrap();
        assert_eq!(ungrouped.len(), 1);
        assert_eq!(ungrouped[0], id);
    }

    #[test]
    fn test_launch_command_roundtrip() {
        let db = Database::open_in_memory().unwrap();
        // Default helper uses "claude"
        let claude_id = create_session(&db, "claude-sess", "/tmp", "claude-sess");
        // Explicit non-claude command
        let bash_id = db
            .create_nexus_session("cook", "/tmp", "cook", "bash", None)
            .unwrap();

        let tree = db.get_tree().unwrap();
        assert_eq!(
            find_session(&tree, &claude_id).unwrap().launch_command,
            "claude"
        );
        assert_eq!(
            find_session(&tree, &bash_id).unwrap().launch_command,
            "bash"
        );
    }

    #[test]
    fn test_update_session_status() {
        let db = Database::open_in_memory().unwrap();
        let id = create_session(&db, "test", "/tmp", "test");

        // Starts as active
        let tree = db.get_tree().unwrap();
        let sess = find_session(&tree, &id).unwrap();
        assert_eq!(sess.status, SessionStatus::Active);

        // Mark detached
        db.update_session_status(&id, SessionStatus::Detached)
            .unwrap();
        let tree = db.get_tree().unwrap();
        let sess = find_session(&tree, &id).unwrap();
        assert_eq!(sess.status, SessionStatus::Detached);
        assert!(!sess.is_active);
    }

    #[test]
    fn test_update_session_name() {
        let db = Database::open_in_memory().unwrap();
        let id = create_session(&db, "old-name", "/tmp", "test");

        db.update_session_name(&id, "new-name", "new-name").unwrap();

        let tree = db.get_tree().unwrap();
        let sess = find_session(&tree, &id).unwrap();
        assert_eq!(sess.display_name, "new-name");
        assert_eq!(sess.tmux_name.as_deref(), Some("new-name"));
    }

    #[test]
    fn test_delete_session() {
        let db = Database::open_in_memory().unwrap();
        let id = create_session(&db, "doomed", "/tmp", "doomed");

        let gid = db.create_group("G", "").unwrap();
        db.assign_session_to_group(&id, gid).unwrap();

        db.delete_session(&id).unwrap();

        let ungrouped = db.get_ungrouped_sessions().unwrap();
        assert!(ungrouped.is_empty());

        let tree = db.get_tree().unwrap();
        assert!(find_session(&tree, &id).is_none());
    }

    #[test]
    fn test_create_and_delete_group() {
        let db = Database::open_in_memory().unwrap();

        let id = db.create_group("Work", "briefcase").unwrap();
        assert!(id > 0);

        let id2 = db.create_group("Personal", "home").unwrap();
        assert_ne!(id, id2);

        db.delete_group(id).unwrap();

        let gid = db.get_group_id_by_name("Work").unwrap();
        assert!(gid.is_none());

        let gid2 = db.get_group_id_by_name("Personal").unwrap();
        assert_eq!(gid2, Some(id2));
    }

    #[test]
    fn test_rename_group() {
        let db = Database::open_in_memory().unwrap();
        let gid = db.create_group("Old", "").unwrap();
        db.rename_group(gid, "New").unwrap();

        assert!(db.get_group_id_by_name("Old").unwrap().is_none());
        assert_eq!(db.get_group_id_by_name("New").unwrap(), Some(gid));
    }

    #[test]
    fn test_move_session_to_group() {
        let db = Database::open_in_memory().unwrap();
        let id = create_session(&db, "test", "/tmp", "test");
        let g1 = db.create_group("G1", "").unwrap();
        let g2 = db.create_group("G2", "").unwrap();

        db.assign_session_to_group(&id, g1).unwrap();
        db.move_session_to_group(&id, g2).unwrap();

        let tree = db.get_tree().unwrap();
        // Session should be in G2, not G1
        for node in &tree {
            if let TreeNode::Group(g) = node {
                if g.id == g1 {
                    assert!(g.children.is_empty());
                }
                if g.id == g2 {
                    assert_eq!(g.children.len(), 1);
                }
            }
        }
    }

    #[test]
    fn test_assign_and_unassign_session() {
        let db = Database::open_in_memory().unwrap();
        let id1 = create_session(&db, "aaa", "/tmp/a", "aaa");
        let id2 = create_session(&db, "bbb", "/tmp/b", "bbb");
        let gid = db.create_group("Work", "").unwrap();

        db.assign_session_to_group(&id1, gid).unwrap();

        let ungrouped = db.get_ungrouped_sessions().unwrap();
        assert_eq!(ungrouped, vec![id2.clone()]);

        db.unassign_session(&id1).unwrap();
        let ungrouped = db.get_ungrouped_sessions().unwrap();
        assert_eq!(ungrouped.len(), 2);
    }

    #[test]
    fn test_duplicate_assign_is_ignored() {
        let db = Database::open_in_memory().unwrap();
        let id = create_session(&db, "aaa", "/tmp", "aaa");
        let gid = db.create_group("Work", "").unwrap();

        db.assign_session_to_group(&id, gid).unwrap();
        db.assign_session_to_group(&id, gid).unwrap();

        let ungrouped = db.get_ungrouped_sessions().unwrap();
        assert!(ungrouped.is_empty());
    }

    #[test]
    fn test_get_tree_empty() {
        let db = Database::open_in_memory().unwrap();
        let tree = db.get_tree().unwrap();
        assert!(tree.is_empty());
    }

    #[test]
    fn test_get_tree_with_groups_and_ungrouped() {
        let db = Database::open_in_memory().unwrap();

        let id1 = create_session(&db, "aaa", "/tmp/a", "aaa");
        let _id2 = create_session(&db, "bbb", "/tmp/b", "bbb");
        let _id3 = create_session(&db, "ccc", "/tmp/c", "ccc");

        let gid = db.create_group("Work", "briefcase").unwrap();
        db.assign_session_to_group(&id1, gid).unwrap();

        let tree = db.get_tree().unwrap();

        // Should have 2 nodes: "Work" group + "Ungrouped"
        assert_eq!(tree.len(), 2);

        match &tree[0] {
            TreeNode::Group(g) => {
                assert_eq!(g.name, "Work");
                assert_eq!(g.children.len(), 1);
            }
            _ => panic!("Expected group node"),
        }

        match &tree[1] {
            TreeNode::Group(g) => {
                assert_eq!(g.name, "Ungrouped");
                assert_eq!(g.children.len(), 2);
            }
            _ => panic!("Expected group node"),
        }
    }

    #[test]
    fn test_get_visible_tree_filters_dead() {
        let db = Database::open_in_memory().unwrap();

        let _id1 = create_session(&db, "alive", "/tmp/a", "alive");
        let id2 = create_session(&db, "dead-one", "/tmp/b", "dead");

        db.update_session_status(&id2, SessionStatus::Dead).unwrap();

        // show_dead=true should show both
        let tree_all = db.get_visible_tree(true).unwrap();
        let count_all = count_sessions(&tree_all);
        assert_eq!(count_all, 2);

        // show_dead=false should hide the dead one
        let tree_live = db.get_visible_tree(false).unwrap();
        let count_live = count_sessions(&tree_live);
        assert_eq!(count_live, 1);
    }

    #[test]
    fn test_get_tree_no_ungrouped_when_all_assigned() {
        let db = Database::open_in_memory().unwrap();

        let id = create_session(&db, "aaa", "/tmp", "aaa");
        let gid = db.create_group("Work", "").unwrap();
        db.assign_session_to_group(&id, gid).unwrap();

        let tree = db.get_tree().unwrap();
        assert_eq!(tree.len(), 1);

        match &tree[0] {
            TreeNode::Group(g) => {
                assert_eq!(g.name, "Work");
                assert_eq!(g.children.len(), 1);
            }
            _ => panic!("Expected group node"),
        }
    }

    #[test]
    fn test_get_session_cwd() {
        let db = Database::open_in_memory().unwrap();
        let id = create_session(&db, "aaa", "/home/user/aaa", "aaa");

        let cwd = db.get_session_cwd(&id).unwrap();
        assert_eq!(cwd, Some("/home/user/aaa".to_string()));

        let cwd_missing = db.get_session_cwd("nonexistent").unwrap();
        assert!(cwd_missing.is_none());
    }

    #[test]
    fn test_cascade_delete_group_removes_assignments() {
        let db = Database::open_in_memory().unwrap();

        let id = create_session(&db, "aaa", "/tmp", "aaa");
        let gid = db.create_group("Work", "").unwrap();
        db.assign_session_to_group(&id, gid).unwrap();

        let ungrouped = db.get_ungrouped_sessions().unwrap();
        assert!(ungrouped.is_empty());

        db.delete_group(gid).unwrap();

        let ungrouped = db.get_ungrouped_sessions().unwrap();
        assert_eq!(ungrouped, vec![id]);
    }

    #[test]
    fn test_group_sort_order() {
        let db = Database::open_in_memory().unwrap();

        db.create_group("Charlie", "").unwrap();
        db.create_group("Alpha", "").unwrap();
        db.create_group("Bravo", "").unwrap();

        let _id = create_session(&db, "aaa", "/tmp", "aaa");
        let tree = db.get_tree().unwrap();

        // 3 empty named groups + 1 Ungrouped
        assert_eq!(tree.len(), 4);

        let names: Vec<&str> = tree
            .iter()
            .filter_map(|n| match n {
                TreeNode::Group(g) => Some(g.name.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(names, vec!["Charlie", "Alpha", "Bravo", "Ungrouped"]);
    }

    #[test]
    fn test_get_all_groups() {
        let db = Database::open_in_memory().unwrap();
        db.create_group("A", "").unwrap();
        db.create_group("B", "").unwrap();

        let groups = db.get_all_groups().unwrap();
        assert_eq!(groups.len(), 2);
        assert_eq!(groups[0].1, "A");
        assert_eq!(groups[1].1, "B");
    }

    #[test]
    fn test_get_set_setting_roundtrip() {
        let db = Database::open_in_memory().unwrap();
        assert!(db.get_setting("theme_index").unwrap().is_none());

        db.set_setting("theme_index", "3").unwrap();
        assert_eq!(
            db.get_setting("theme_index").unwrap(),
            Some("3".to_string())
        );
    }

    #[test]
    fn test_set_setting_overwrites() {
        let db = Database::open_in_memory().unwrap();
        db.set_setting("theme_index", "1").unwrap();
        db.set_setting("theme_index", "5").unwrap();
        assert_eq!(
            db.get_setting("theme_index").unwrap(),
            Some("5".to_string())
        );
    }

    // Test helpers
    fn find_session<'a>(tree: &'a [TreeNode], id: &str) -> Option<&'a SessionSummary> {
        for node in tree {
            match node {
                TreeNode::Session(s) if s.session_id == id => return Some(s),
                TreeNode::Group(g) => {
                    if let Some(s) = find_session(&g.children, id) {
                        return Some(s);
                    }
                }
                _ => {}
            }
        }
        None
    }

    #[test]
    fn test_next_unique_tmux_name() {
        let db = Database::open_in_memory().unwrap();

        // First use of "foo" should be free.
        assert_eq!(db.next_unique_tmux_name("foo", None).unwrap(), "foo");

        // Insert a session with tmux_name "foo".
        create_session(&db, "foo", "/tmp", "foo");
        assert_eq!(db.next_unique_tmux_name("foo", None).unwrap(), "foo-2");

        // Insert "foo-2".
        create_session(&db, "foo2", "/tmp", "foo-2");
        assert_eq!(db.next_unique_tmux_name("foo", None).unwrap(), "foo-3");

        // Unrelated name is unaffected.
        assert_eq!(db.next_unique_tmux_name("bar", None).unwrap(), "bar");
    }

    #[test]
    fn test_next_unique_tmux_name_exclude_self() {
        let db = Database::open_in_memory().unwrap();

        let id = create_session(&db, "foo", "/tmp", "foo");

        // Without exclude: "foo" is taken.
        assert_eq!(db.next_unique_tmux_name("foo", None).unwrap(), "foo-2");

        // With exclude: renaming to own name is fine.
        assert_eq!(db.next_unique_tmux_name("foo", Some(&id)).unwrap(), "foo");
    }

    #[test]
    fn test_worktree_roundtrip() {
        let db = Database::open_in_memory().unwrap();
        let wt = WorktreeInfo {
            branch: "nexus/my-feature".to_string(),
            repo_root: std::path::PathBuf::from("/home/user/repo"),
        };
        let id = db
            .create_nexus_session(
                "wt-test",
                "/home/user/repo/.worktrees/my-feature",
                "wt-test",
                "claude",
                Some(&wt),
            )
            .unwrap();

        let tree = db.get_tree().unwrap();
        let sess = find_session(&tree, &id).unwrap();
        assert!(sess.worktree.is_some());
        let wt_read = sess.worktree.as_ref().unwrap();
        assert_eq!(wt_read.branch, "nexus/my-feature");
        assert_eq!(
            wt_read.repo_root,
            std::path::PathBuf::from("/home/user/repo")
        );
    }

    #[test]
    fn test_worktree_none_roundtrip() {
        let db = Database::open_in_memory().unwrap();
        let id = create_session(&db, "plain", "/tmp", "plain");

        let tree = db.get_tree().unwrap();
        let sess = find_session(&tree, &id).unwrap();
        assert!(sess.worktree.is_none());
    }

    #[test]
    fn test_reconcile_worktrees() {
        let db = Database::open_in_memory().unwrap();
        // Create a session with a worktree pointing to a non-existent path
        let wt = WorktreeInfo {
            branch: "nexus/stale".to_string(),
            repo_root: std::path::PathBuf::from("/tmp"),
        };
        let id = db
            .create_nexus_session(
                "stale",
                "/nonexistent/path/.worktrees/stale",
                "stale",
                "claude",
                Some(&wt),
            )
            .unwrap();

        let cleaned = db.reconcile_worktrees().unwrap();
        assert_eq!(cleaned, 1);

        // Worktree columns should be cleared
        let tree = db.get_tree().unwrap();
        let sess = find_session(&tree, &id).unwrap();
        assert!(sess.worktree.is_none());
    }

    #[test]
    fn test_clear_worktree_columns() {
        let db = Database::open_in_memory().unwrap();
        let wt = WorktreeInfo {
            branch: "nexus/feat".to_string(),
            repo_root: std::path::PathBuf::from("/tmp"),
        };
        let id = db
            .create_nexus_session("feat", "/tmp/.worktrees/feat", "feat", "claude", Some(&wt))
            .unwrap();

        db.clear_worktree_columns(&id).unwrap();

        let tree = db.get_tree().unwrap();
        let sess = find_session(&tree, &id).unwrap();
        assert!(sess.worktree.is_none());
    }

    fn count_sessions(tree: &[TreeNode]) -> usize {
        let mut count = 0;
        for node in tree {
            match node {
                TreeNode::Session(_) => count += 1,
                TreeNode::Group(g) => count += count_sessions(&g.children),
            }
        }
        count
    }
}
