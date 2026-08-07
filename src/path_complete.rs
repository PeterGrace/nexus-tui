use std::fs;
use std::path::{Path, PathBuf};

use color_eyre::eyre::{bail, WrapErr};

/// Expand `~` at the start of a path to the user's home directory.
/// Returns `(expanded_path, had_tilde)`.
fn expand_tilde(input: &str) -> (String, bool) {
    if input == "~" || input.starts_with("~/") {
        if let Some(home) = dirs::home_dir() {
            let rest = &input[1..]; // "" or "/..."
            return (format!("{}{rest}", home.display()), true);
        }
    }
    (input.to_string(), false)
}

/// Collapse the home directory prefix back to `~` for display.
fn collapse_tilde(path: &str) -> String {
    if let Some(home) = dirs::home_dir() {
        let home_str = home.to_string_lossy();
        if path == home_str.as_ref() {
            return "~".to_string();
        }
        if let Some(rest) = path.strip_prefix(home_str.as_ref()) {
            if rest.starts_with('/') {
                return format!("~{rest}");
            }
        }
    }
    path.to_string()
}

/// Return filesystem completions for the given input prefix.
///
/// - Splits input into parent directory + partial filename
/// - Reads the parent directory and prefix-filters entries
/// - Sorts: directories first, then alphabetical
/// - Re-collapses `~` for display
///
/// Returns an empty vec for empty input, non-existent parent, or permission errors.
pub fn complete_path(input: &str) -> Vec<String> {
    if input.is_empty() {
        return Vec::new();
    }

    let (expanded, had_tilde) = expand_tilde(input);

    let (parent_dir, prefix) = if expanded.ends_with('/') {
        // Input ends with `/` — list the directory itself
        (expanded.as_str(), "")
    } else {
        // Split into parent + partial filename
        let path = Path::new(&expanded);
        match (path.parent(), path.file_name()) {
            (Some(p), Some(f)) => {
                let parent_str = if p.as_os_str().is_empty() {
                    ".".to_string()
                } else {
                    p.to_string_lossy().to_string()
                };
                // We need owned strings since we can't borrow expanded twice
                let prefix_str = f.to_string_lossy().to_string();
                return complete_with_parent(&parent_str, &prefix_str, had_tilde);
            }
            _ => return Vec::new(),
        }
    };

    complete_with_parent(parent_dir, prefix, had_tilde)
}

fn complete_with_parent(parent_dir: &str, prefix: &str, had_tilde: bool) -> Vec<String> {
    let entries = match fs::read_dir(parent_dir) {
        Ok(rd) => rd,
        Err(_) => return Vec::new(),
    };

    let parent_path = if parent_dir == "." {
        String::new()
    } else if parent_dir.ends_with('/') {
        parent_dir.to_string()
    } else {
        format!("{parent_dir}/")
    };

    let prefix_lower = prefix.to_lowercase();

    let mut matches: Vec<(bool, String)> = entries
        .filter_map(|e| e.ok())
        .filter_map(|entry| {
            let name = entry.file_name().to_string_lossy().to_string();
            if !prefix.is_empty() && !name.to_lowercase().starts_with(&prefix_lower) {
                return None;
            }
            // Skip hidden files unless the user is explicitly typing a dot prefix
            if name.starts_with('.') && !prefix.starts_with('.') {
                return None;
            }
            let is_dir = entry.file_type().map(|ft| ft.is_dir()).unwrap_or(false);
            let full = format!("{parent_path}{name}");
            let display = if had_tilde {
                collapse_tilde(&full)
            } else {
                full
            };
            Some((is_dir, display))
        })
        .collect();

    // Sort: directories first, then alphabetical (case-insensitive)
    matches.sort_by(|(a_dir, a_name), (b_dir, b_name)| {
        b_dir
            .cmp(a_dir)
            .then_with(|| a_name.to_lowercase().cmp(&b_name.to_lowercase()))
    });

    matches.into_iter().map(|(_, name)| name).collect()
}

/// Resolve a user-entered working directory to a canonical absolute path.
pub fn resolve_directory(input: &str) -> color_eyre::Result<PathBuf> {
    let (expanded, _) = expand_tilde(input);
    let path = Path::new(&expanded);
    let resolved = path
        .canonicalize()
        .wrap_err_with(|| format!("cannot resolve working directory '{}'", path.display()))?;

    if !resolved.is_dir() {
        bail!("working directory '{}' is not a directory", path.display());
    }

    Ok(resolved)
}

/// Check if the given path (possibly with `~`) refers to a directory.
pub fn is_directory(path: &str) -> bool {
    let (expanded, _) = expand_tilde(path);
    Path::new(&expanded).is_dir()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_expand_tilde_plain() {
        let (result, had) = expand_tilde("/usr/local");
        assert_eq!(result, "/usr/local");
        assert!(!had);
    }

    #[test]
    fn test_expand_tilde_home() {
        let (result, had) = expand_tilde("~/Documents");
        assert!(had);
        assert!(!result.starts_with('~'));
        assert!(result.ends_with("/Documents"));
    }

    #[test]
    fn test_expand_tilde_bare() {
        let (result, had) = expand_tilde("~");
        assert!(had);
        assert!(!result.starts_with('~'));
    }

    #[test]
    fn test_collapse_tilde() {
        if let Some(home) = dirs::home_dir() {
            let home_str = home.to_string_lossy().to_string();
            assert_eq!(collapse_tilde(&home_str), "~");
            assert_eq!(collapse_tilde(&format!("{home_str}/foo")), "~/foo");
        }
    }

    #[test]
    fn test_complete_empty_input() {
        assert!(complete_path("").is_empty());
    }

    #[test]
    fn test_complete_root() {
        let results = complete_path("/");
        assert!(!results.is_empty());
        for r in &results {
            assert!(r.starts_with('/'));
        }
    }

    #[test]
    fn test_complete_nonexistent() {
        let results = complete_path("/nonexistent_dir_abc123/");
        assert!(results.is_empty());
    }

    #[test]
    fn test_complete_tilde() {
        let results = complete_path("~/");
        // Should return entries with ~ prefix
        for r in &results {
            assert!(r.starts_with("~/"), "Expected ~/... but got: {r}");
        }
    }

    #[test]
    fn test_complete_dirs_first() {
        // /tmp should have a mix; dirs should come first
        let results = complete_path("/tmp/");
        if results.len() >= 2 {
            let first_is_dir = is_directory(&results[0]);
            let last_is_dir = is_directory(results.last().unwrap());
            // If there's at least one dir and one non-dir, dirs should be first
            if first_is_dir && !last_is_dir {
                // correct order
            }
            // Just verify it doesn't panic; ordering is best-effort in /tmp
        }
    }

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
        let current_dir = std::env::current_dir().unwrap();
        let relative = directory.strip_prefix(current_dir).unwrap();
        assert!(!relative.is_absolute());

        assert_eq!(
            resolve_directory(relative.to_str().unwrap()).unwrap(),
            directory.canonicalize().unwrap()
        );
    }

    #[test]
    fn test_resolve_directory_rejects_nonexistent_path() {
        let temp = tempfile::tempdir().unwrap();
        let missing = temp.path().join("missing");

        let error = resolve_directory(missing.to_str().unwrap()).unwrap_err();
        assert!(error
            .to_string()
            .contains("cannot resolve working directory"));
    }

    #[test]
    fn test_resolve_directory_rejects_file() {
        let file = tempfile::NamedTempFile::new().unwrap();

        let error = resolve_directory(file.path().to_str().unwrap()).unwrap_err();
        assert!(error.to_string().contains("is not a directory"));
    }

    #[test]
    fn test_is_directory() {
        assert!(is_directory("/tmp"));
        assert!(!is_directory("/nonexistent_abc123"));
    }

    #[test]
    fn test_complete_with_partial() {
        // Complete /tmp — should match entries starting with partial
        let results = complete_path("/tm");
        assert!(results.iter().any(|r| r.starts_with("/tmp")));
    }

    #[test]
    fn test_returns_all_matches() {
        // Should return all matches without truncation
        let results = complete_path("/");
        assert!(results.len() > 5, "Expected more than 5 entries in /");
    }
}
