# Session Working Directory Resolution Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Prevent new sessions from silently launching in Nexus's startup directory by resolving and validating the requested working directory before session creation proceeds.

**Architecture:** Add one filesystem-boundary helper in `path_complete.rs` that expands `~`, rejects non-directories, and returns a canonical absolute `PathBuf`. Call it at both creation entry points: the TUI CWD transition and `nexus new`; the TUI retains its prompt on errors, while CLI errors propagate before session records or tmux sessions are created.

**Tech Stack:** Rust 2021, color-eyre, tempfile, crossterm/Ratatui application state, Cargo test/clippy/rustfmt.

## Global Constraints

- Valid directories resolve with or without a trailing `/`.
- Only canonical absolute paths proceed to git detection, database storage, worktree handling, and tmux launch.
- Invalid TUI input remains editable at the working-directory prompt and displays an error.
- Invalid CLI input exits with an error before session creation side effects.
- Do not automatically accept completions or create missing directories.
- Update `README.md` for the user-facing behavior.

---

### Task 1: Central directory resolver

**Files:**
- Modify: `src/path_complete.rs`
- Test: `src/path_complete.rs` (`#[cfg(test)]` module)

**Interfaces:**
- Consumes: user-entered path text and the existing private `expand_tilde(&str) -> (String, bool)` helper.
- Produces: `pub fn resolve_directory(input: &str) -> color_eyre::Result<std::path::PathBuf>` for TUI and CLI callers.

- [ ] **Step 1: Write failing resolver tests**

Add deterministic tempfile-based tests covering slash handling, canonicalization, missing paths, and files:

```rust
#[test]
fn test_resolve_directory_without_trailing_slash() {
    let temp = tempfile::tempdir().unwrap();
    let directory = temp.path().join("project");
    std::fs::create_dir(&directory).unwrap();

    assert_eq!(
        resolve_directory(directory.to_str().unwrap()).unwrap(),
        directory.canonicalize().unwrap()
    );
}

#[test]
fn test_resolve_directory_with_trailing_slash() {
    let temp = tempfile::tempdir().unwrap();
    let directory = temp.path().join("project");
    std::fs::create_dir(&directory).unwrap();
    let input = format!("{}/", directory.display());

    assert_eq!(
        resolve_directory(&input).unwrap(),
        directory.canonicalize().unwrap()
    );
}

#[test]
fn test_resolve_relative_directory_returns_canonical_path() {
    let temp = tempfile::Builder::new()
        .prefix("nexus-relative-path-")
        .tempdir_in(".")
        .unwrap();
    let directory = temp.path().join("project");
    std::fs::create_dir(&directory).unwrap();
    assert!(!directory.is_absolute());

    assert_eq!(
        resolve_directory(directory.to_str().unwrap()).unwrap(),
        directory.canonicalize().unwrap()
    );
}

#[test]
fn test_resolve_directory_rejects_nonexistent_path() {
    let temp = tempfile::tempdir().unwrap();
    let missing = temp.path().join("missing");

    let error = resolve_directory(missing.to_str().unwrap()).unwrap_err();
    assert!(error.to_string().contains("cannot resolve working directory"));
}

#[test]
fn test_resolve_directory_rejects_file() {
    let file = tempfile::NamedTempFile::new().unwrap();

    let error = resolve_directory(file.path().to_str().unwrap()).unwrap_err();
    assert!(error.to_string().contains("is not a directory"));
}
```

- [ ] **Step 2: Run the focused tests and verify RED**

Run:

```bash
cargo test path_complete::tests::test_resolve_directory
```

Expected: compilation fails because `resolve_directory` is not defined.

- [ ] **Step 3: Implement the minimal resolver**

Import `PathBuf`, `bail`, and `WrapErr`, then add:

```rust
pub fn resolve_directory(input: &str) -> color_eyre::Result<PathBuf> {
    let (expanded, _) = expand_tilde(input);
    let path = Path::new(&expanded);
    let resolved = path.canonicalize().wrap_err_with(|| {
        format!(
            "cannot resolve working directory '{}'",
            path.display()
        )
    })?;

    if !resolved.is_dir() {
        bail!("working directory '{}' is not a directory", path.display());
    }

    Ok(resolved)
}
```

Canonicalizing first retains filesystem context for missing or inaccessible paths; the subsequent type check gives existing files a specific error.

- [ ] **Step 4: Run resolver tests and the path-completion module tests**

Run:

```bash
cargo test path_complete::tests
```

Expected: all path-completion and resolver tests pass.

- [ ] **Step 5: Commit the resolver**

```bash
git add src/path_complete.rs
git commit -m "fix: resolve session working directories"
```

---

### Task 2: Enforce resolution in TUI and CLI creation flows

**Files:**
- Modify: `src/app.rs:948-1020`
- Modify: `src/main.rs:72-126`
- Modify: `README.md:91-110`
- Test: `src/app.rs` (`#[cfg(test)]` module)

**Interfaces:**
- Consumes: `crate::path_complete::resolve_directory(&str) -> color_eyre::Result<PathBuf>` from Task 1.
- Produces: TUI creation state that advances only with canonical CWD text, and CLI creation that rejects invalid CWD values before database insertion.

- [ ] **Step 1: Add a test-only App constructor and failing TUI regression tests**

In `src/app.rs`'s test module, add a helper that keeps the temporary database and update-check path aligned:

```rust
fn test_app(temp: &tempfile::TempDir) -> App {
    let db_path = temp.path().join("nexus.db");
    let db = Database::open(&db_path).unwrap();
    let mut config = NexusConfig::default();
    config.general.db_path = db_path;
    App::new(
        config,
        Vec::new(),
        TmuxManager::new("nexus-cwd-resolution-test"),
        false,
        Vec::new(),
        db,
    )
}
```

Add the invalid-input regression:

```rust
#[test]
fn test_new_session_cwd_rejects_invalid_path_without_advancing() {
    let temp = tempfile::tempdir().unwrap();
    let mut app = test_app(&temp);
    let missing = temp.path().join("project").to_string_lossy().to_string();
    app.input_mode = InputMode::TextInput;
    app.input_buffer = missing.clone();
    app.input_context = Some(InputContext::NewSessionCwd {
        name: "test-session".to_string(),
    });

    app.process_text_input(missing.clone());

    assert_eq!(app.input_mode, InputMode::TextInput);
    assert_eq!(app.input_buffer, missing);
    assert!(matches!(
        app.input_context,
        Some(InputContext::NewSessionCwd { ref name }) if name == "test-session"
    ));
    assert!(app
        .status_message
        .as_ref()
        .is_some_and(|(message, _)| message.contains("invalid working directory")));
}
```

Add the no-trailing-slash canonical-path regression:

```rust
#[test]
fn test_new_session_cwd_advances_with_canonical_directory() {
    let temp = tempfile::tempdir().unwrap();
    let directory = temp.path().join("project");
    std::fs::create_dir(&directory).unwrap();
    let mut app = test_app(&temp);
    let input = directory.to_string_lossy().to_string();
    app.input_mode = InputMode::TextInput;
    app.input_buffer = input.clone();
    app.input_context = Some(InputContext::NewSessionCwd {
        name: "test-session".to_string(),
    });

    app.process_text_input(input);

    let canonical = directory.canonicalize().unwrap().to_string_lossy().to_string();
    assert_eq!(app.input_mode, InputMode::Confirm);
    assert!(matches!(
        app.input_context,
        Some(InputContext::NewSessionAgent { ref cwd, .. }) if cwd == &canonical
    ));
}
```

- [ ] **Step 2: Run the focused TUI tests and verify RED**

Run:

```bash
cargo test app::tests::test_new_session_cwd
```

Expected: the invalid-path test advances to agent confirmation instead of retaining `NewSessionCwd`; the canonical-path test may pass only accidentally for already-canonical temp paths, while the focused command remains failed overall.

- [ ] **Step 3: Resolve paths at the TUI CWD transition**

Remove the Enter-key branch's ad hoc `~` expansion so all expansion lives in the resolver. Replace the start of `InputContext::NewSessionCwd { name }` with:

```rust
InputContext::NewSessionCwd { name } => {
    let cwd = match crate::path_complete::resolve_directory(&buffer) {
        Ok(path) => path.to_string_lossy().into_owned(),
        Err(error) => {
            self.input_mode = InputMode::TextInput;
            self.input_context = Some(InputContext::NewSessionCwd { name });
            self.status_message = Some((
                format!("invalid working directory: {error}"),
                Instant::now(),
            ));
            return;
        }
    };
    if let Some(repo) = git::detect_repo(&cwd) {
```

Pass `cwd`, not the raw `buffer`, into both subsequent `InputContext::NewSessionWorktree` and `transition_to_group_or_create` paths.

- [ ] **Step 4: Resolve CLI CWD before worktree detection or database insertion**

After selecting the explicit or default CWD in `cli::Commands::New`, resolve it and convert the canonical path to owned text:

```rust
let cwd = cwd.unwrap_or_else(|| {
    std::env::current_dir()
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_else(|_| "/tmp".to_string())
});
let cwd = path_complete::resolve_directory(&cwd)?;
let cwd = cwd.to_string_lossy().into_owned();
```

Keep all existing git/worktree, database, and tmux logic downstream of this resolution.

- [ ] **Step 5: Document creation-path requirements**

Immediately after the CLI command example block in `README.md`, add:

```markdown
Session working directories must already exist. Nexus expands `~` and resolves the directory to a canonical absolute path before creating a session; invalid paths are rejected instead of falling back to Nexus's startup directory.
```

- [ ] **Step 6: Run focused tests and verify GREEN**

Run:

```bash
cargo test app::tests::test_new_session_cwd
cargo test path_complete::tests
```

Expected: all focused tests pass.

- [ ] **Step 7: Format and run complete verification**

Run:

```bash
cargo fmt
cargo fmt --check
cargo clippy -- -D warnings
cargo test
```

Expected: formatting is clean, clippy reports no warnings, and all tests pass.

- [ ] **Step 8: Commit integration and documentation**

```bash
git add src/app.rs src/main.rs README.md
git commit -m "fix: validate cwd before session creation"
```
