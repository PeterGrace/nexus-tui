# Session Working Directory Resolution Design

## Problem

Nexus currently passes the working-directory text entered during session creation directly to git detection, database creation, and `tmux new-session -c`. If the text does not resolve to an existing directory, tmux accepts it but silently starts the session in the tmux server's current working directory. This can make an unaccepted or incomplete path completion appear to work while launching the coding agent in the wrong repository.

## Desired behavior

Before Nexus proceeds beyond the working-directory step, it must:

1. Expand a leading `~`.
2. Require the path to exist and be a directory.
3. Canonicalize it to an absolute filesystem path.
4. Use only that resolved path for subsequent git detection, database storage, worktree handling, and tmux launch.

A valid directory must resolve whether or not the input ends with `/`.

## Design

Add a centralized path-resolution helper alongside the existing path-completion utilities. The helper accepts user input and returns either a canonical directory path or a descriptive error.

The TUI session-creation flow will call the helper when the working-directory prompt is submitted. On success, it will replace the raw input with the canonical path and continue to git/worktree, group, and agent selection. On failure, it will remain in text-input mode at the working-directory prompt, preserve the entered text so the user can correct it, and display the error in the existing status-message area. No session record or tmux process will be created.

The `nexus new` CLI flow will use the same helper for both an explicit `--cwd` and the default current directory. Resolution failure will terminate the command with a nonzero error before database creation or tmux launch.

Validation will occur at session-creation entry points rather than inside `TmuxManager`: tmux also launches previously stored sessions, and late validation could leave a newly created database record behind. Centralizing the helper keeps TUI and CLI behavior consistent while ensuring validation occurs before side effects.

## Error handling

Errors will distinguish these user-actionable cases where practical:

- the path does not exist or cannot be resolved;
- the resolved path is not a directory.

Underlying filesystem error context should be retained. The TUI presents the error without leaving the prompt; the CLI propagates it through its existing error-reporting path.

## Testing

Add unit tests for the resolver covering:

- an existing directory without a trailing slash resolves successfully;
- an existing directory with a trailing slash resolves successfully;
- a relative directory resolves to its canonical absolute path;
- a nonexistent path is rejected;
- an existing file is rejected as not being a directory.

Add a TUI-flow regression test at the narrowest practical boundary to verify invalid CWD input does not advance the creation context and valid input advances using the canonical path. If constructing `App` makes that test disproportionately coupled, resolver tests plus direct use at the input transition are sufficient, since the transition contains no other path transformation.

## Documentation

Update the CLI reference in `README.md` to state that session working directories must exist and are resolved to canonical absolute paths before creation.

## Non-goals

- Automatically accepting a highlighted or unique completion when Enter is pressed.
- Creating missing directories.
- Changing completion matching or keyboard navigation.
- Repairing invalid working directories already stored in the database.
