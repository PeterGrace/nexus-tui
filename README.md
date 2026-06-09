# Nexus

TUI session manager for [Claude Code](https://docs.anthropic.com/en/docs/claude-code).

![CI](https://github.com/markx3/nexus-tui/actions/workflows/ci.yml/badge.svg)

![screenshot](assets/screenshot.png)

Nexus gives you a persistent, organized workspace for managing multiple Claude Code sessions. It wraps each session in a tmux pane with a live terminal preview, groups sessions by project, and lets you switch between them instantly — all in a single terminal window.

## Features

- **Live terminal preview** — see Claude Code output in real-time without switching windows
- **Session grouping** — organize sessions by project via config or on-the-fly
- **8 color themes** — cycle with `Alt+t`, persisted across restarts
- **Session lifecycle** — create, rename, move, delete, and kill sessions from the TUI or CLI
- **Worktree isolation** — optionally create a dedicated git worktree per session for branch-level isolation
- **Custom launch commands** — launch `claude`, a plain `bash` shell, or any command you define as a session type (e.g. for async "cooking" sessions)
- **Claude session resume** — automatically detects Claude Code session IDs so relaunched Claude sessions resume where they left off
- **CLI + JSON output** — scriptable interface for all operations (`nexus list --json`)
- **Lazygit integration** — open lazygit in any session's working directory with `Alt+l`
- **Editor integration** — open your editor in any session's working directory with `Alt+v`
- **Text selection** — click+drag in the session panel to select and copy text (via OSC 52)
- **Feedback detection** — automatically detects when Claude is waiting for permission or confirmation across all sessions, pulsing the session tree row with a glow effect (no setup required)

## Install

Requires [tmux](https://github.com/tmux/tmux) and a [Rust toolchain](https://rustup.rs/).

```sh
# From GitHub
cargo install --git https://github.com/markx3/nexus-tui

# From source
git clone https://github.com/markx3/nexus-tui.git
cd nexus-tui
cargo install --path .
```

Optional: [lazygit](https://github.com/jesseduffield/lazygit) for the `Alt+l` git integration.
Optional: `nvim` or `vim` for the `Alt+v` editor integration (or set `$EDITOR`).

## Usage

### TUI Mode

Run `nexus` with no arguments to launch the interactive dashboard.

```sh
nexus
```

The TUI shows a session tree on the left and a live terminal preview on the right. All Nexus controls use the **Alt+key** namespace — every other key is forwarded directly to the embedded Claude Code session.

### Keybindings

| Key | Action |
|-----|--------|
| `Alt+q` | Quit Nexus |
| `Alt+h` / `Alt+?` | Toggle help overlay |
| `Alt+j` | Cursor down |
| `Alt+k` | Cursor up |
| `Alt+Enter` | Toggle expand/collapse group |
| `Alt+n` | New session |
| `Alt+g` | New group |
| `Alt+r` | Rename selected item |
| `Alt+m` | Move session to group |
| `Alt+d` | Delete selected item |
| `Alt+x` | Kill tmux session (mark detached) |
| `Alt+H` | Toggle past/dead sessions |
| `Alt+t` / `Alt+T` | Cycle theme forward/backward |
| `Alt+l` | Open lazygit in session cwd |
| `Alt+v` | Open editor ($EDITOR/nvim/vim) in session cwd |
| `Alt+p` | Session finder (fuzzy search) |

**Scrolling:**

| Key | Action |
|-----|--------|
| `Shift+Up/Down` | Scroll live view line-by-line |
| `Shift+PageUp/PageDown` | Scroll live view by 10 lines |
| Mouse scroll | Scroll live view or conversation log |
| Click+drag (session panel) | Select text and copy to clipboard |
| `Up/Down/PageUp/PageDown` | Scroll conversation log (dead/detached sessions) |

### CLI Mode

Most subcommands support `--json` for machine-readable output.

```sh
nexus list                           # List active sessions
nexus list --all                     # Include dead/past sessions
nexus show <id>                      # Show session details (ID prefix supported)
nexus new <name>                     # Create and launch a new session
nexus new <name> -c /path -g mygroup # With cwd and group
nexus new <name> -w                  # Create with an isolated git worktree
nexus new <name> --command bash      # Launch a command other than claude
nexus launch <id>                    # Launch/resume a session in tmux
nexus kill <name>                    # Kill a running tmux session
nexus groups                         # List configured groups
nexus send <name> <text>             # Send text to a tmux session
nexus capture <name>                 # Capture pane contents
nexus capture <name> --strip         # Capture without ANSI codes
nexus delete <id>                    # Delete a session from the database
nexus delete <id> --remove-worktree  # Also clean up the git worktree
nexus rename <id> <name>             # Rename a session
nexus move <id> --group <name>       # Move session to a group
nexus group-create <name>            # Create a new group
nexus update                         # Update nexus to the latest version
```

## Configuration

Nexus reads its config from `~/.config/nexus/config.toml`. All fields are optional.

```toml
[general]
# db_path = "~/.local/share/nexus/nexus.db"  # default

[tmux]
# socket_name = "nexus"  # default; uses `tmux -L nexus`

# Define groups for organizing sessions
[[groups]]
name = "work"

[[groups]]
name = "personal"

# Define custom session launch types (offered in the new-session picker)
[[session_types]]
name = "Cook"
cmd = "bash"

[[session_types]]
name = "Notes"
cmd = "nvim ~/notes.md"
```

### Groups

Groups organize your sessions in the tree view. Create them in the config file or on-the-fly with `Alt+g` in the TUI or `nexus group-create` from the CLI.

### Session Types

By default a new session launches `claude`. The final step of the new-session flow (`Alt+n`) is a **session type** picker offering:

- **Claude** — runs `claude`; this is the only type that resumes a prior conversation on relaunch.
- Any `[[session_types]]` you define in the config (each with a `name` shown in the picker and a `cmd` that tmux runs).
- **Custom…** — prompts for a free-text command to run once.

Non-Claude sessions relaunch their command verbatim and show only the live terminal (no conversation log). From the CLI, use `nexus new <name> --command <cmd>`. Commands are split on whitespace, so `bash` and `npm run cook` work, but quoting and shell pipes are not supported.

### Themes

8 built-in themes, cycled with `Alt+t` / `Alt+T`. Your selection is persisted.

| # | Theme |
|---|-------|
| 0 | Current Baseline |
| 1 | Outrun Sunset |
| 2 | Cyberpunk 2077 |
| 3 | Blade Runner 2049 |
| 4 | Neon Deep Ocean |
| 5 | Synthwave Nights |
| 6 | Retrowave Pure *(default)* |
| 7 | Matrix Phosphor |

### Worktree Isolation

When creating a session in a git repo, Nexus can create a dedicated git worktree so each session works on an isolated branch. In the TUI, you'll be prompted with "Isolate in git worktree? (y/n)" after entering a CWD that is a git repo. From the CLI, use `nexus new <name> -w`.

Worktree sessions show a branch badge (e.g., `[my-app/fix-bug]`) in the session tree. When deleting a worktree session, you'll be prompted with `y` (delete both), `n` (cancel), or `s` (session only, keep worktree on disk).

**Branch prefix:** By default, worktree branches are prefixed with the repo directory name (e.g., `my-app/fix-bug`). You can override this globally in `~/.config/nexus/config.toml` or per-repo in `.nexus.toml` at the repo root:

```toml
# ~/.config/nexus/config.toml (global)
[worktree]
branch_prefix = "team"    # all repos: team/fix-bug
```

```toml
# .nexus.toml (per-repo, overrides global)
[worktree]
branch_prefix = "custom"  # this repo: custom/fix-bug
# branch_prefix = ""      # or disable prefix entirely: fix-bug
```

### Worktree Hooks

Nexus can run custom scripts when creating or tearing down worktrees. If a hook is configured, Nexus delegates the entire operation to it instead of running `git worktree add`/`remove`.

**Resolution priority** (first match wins):

1. Per-repo `.nexus.toml` — paths resolved relative to the repo root
2. Global `~/.config/nexus/config.toml` — absolute paths or `~/`-prefixed
3. Convention: `{repo_root}/.nexus/on-worktree-create` and `.nexus/on-worktree-teardown`

If a configured path is invalid (missing, not executable, symlink), Nexus does **not** fall through to the next level — the hook is skipped entirely. This prevents accidentally running a convention hook when you intended a specific one.

```toml
# ~/.config/nexus/config.toml (global)
[worktree]
on_create = "~/scripts/wt-create.sh"
on_teardown = "~/scripts/wt-teardown.sh"
```

```toml
# .nexus.toml (per-repo, overrides global)
[worktree]
on_create = "scripts/wt-create.sh"      # relative to repo root
on_teardown = "scripts/wt-teardown.sh"
```

**Environment variables** passed to hooks:

| Variable | Description |
|---|---|
| `NEXUS_WORKTREE_PATH` | Target worktree directory |
| `NEXUS_BRANCH` | Git branch name |
| `NEXUS_SESSION_NAME` | Nexus session name (empty on teardown) |
| `NEXUS_REPO_ROOT` | Repository root path |

**Constraints:**

- Hooks must be regular files (symlinks and directories are rejected)
- Hooks must have the executable bit set
- Per-repo hook paths must not contain `..` (path traversal outside the repo root is rejected)
- Hooks time out after 60 seconds (process group is killed)
- The environment is scrubbed — only `PATH`, `HOME`, `SHELL`, `USER`, `LANG`, `TERM` plus the `NEXUS_*` variables above are available
- Create hooks **must** create the directory at `$NEXUS_WORKTREE_PATH` or Nexus will report an error

**Example hook:**

```bash
#!/usr/bin/env bash
set -euo pipefail
git -C "$NEXUS_REPO_ROOT" worktree add "$NEXUS_WORKTREE_PATH" -b "$NEXUS_BRANCH"
cp "$NEXUS_REPO_ROOT/.env.example" "$NEXUS_WORKTREE_PATH/.env" 2>/dev/null || true
```

### Auto-Update

Nexus checks for updates on startup (at most once per hour) by comparing your local build against the upstream git repo. If a newer version is available, an **UPDATE** badge appears in the top bar. Run `nexus update` to pull the latest code and rebuild.

The update check uses the source directory from your original `cargo install --path .` build. If the source directory no longer exists, the check is silently skipped.

## Terminal Setup (macOS)

Nexus uses **Alt+key** shortcuts for navigation and commands. On macOS, the Option key produces special characters by default (e.g., Option+M types `µ`) instead of the Alt/Meta sequences that terminal applications expect.

You **must** configure your terminal to treat Option as Meta/Alt, or Alt keybindings will not work.

### Ghostty

Add to `~/.config/ghostty/config`:

```
macos-option-as-alt = true
```

Restart Ghostty after changing this setting.

### iTerm2

Settings > Profiles > Keys > General > Left Option key: **Esc+**

(Repeat for Right Option key if desired.)

### Terminal.app

Settings > Profiles > Keyboard > **Use Option as Meta key**

### Kitty

Add to `~/.config/kitty/kitty.conf`:

```
macos_option_as_alt yes
```

### Alacritty

No configuration needed -- Alacritty treats Option as Alt by default on macOS.

### WezTerm

Add to `~/.config/wezterm/wezterm.lua`:

```lua
config.send_composed_key_when_left_alt_is_pressed = false
config.send_composed_key_when_right_alt_is_pressed = false
```

## Platform Support

- **macOS** — primary development platform, fully supported
- **Linux** — supported (tmux + standard terminal required)
- **Windows** — not supported (tmux dependency)

## Notes

- Nexus uses a dedicated tmux socket (`tmux -L nexus`). Sessions created in your default tmux server won't appear in Nexus, and vice versa.
- Data is stored in SQLite at `~/.local/share/nexus/nexus.db` with WAL mode.
- If Nexus crashes and won't restart due to a stale lock, remove `~/.cache/nexus/nexus.lock`.

## License

[MIT](LICENSE)
