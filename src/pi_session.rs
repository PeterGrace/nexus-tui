use std::path::{Path, PathBuf};
use std::time::SystemTime;

use color_eyre::Result;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PiSessionFile {
    pub(crate) id: String,
    pub(crate) modified: SystemTime,
    pub(crate) path: PathBuf,
}

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
            if path
                .extension()
                .is_none_or(|extension| extension != "jsonl")
            {
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

#[allow(dead_code)]
pub(crate) fn find_path(cwd: &str, id: &str) -> Option<PathBuf> {
    find_path_in(&session_root()?, cwd, id)
}

#[allow(dead_code)]
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
        write_session(&nested.join("other.jsonl"), "other-id", "/tmp/other", "");
        std::fs::write(nested.join("malformed.jsonl"), "not json").unwrap();

        let found = sessions_in(temp.path(), "/tmp/project");

        assert_eq!(found.len(), 1);
        assert_eq!(found[0].id, "pi-session-id");
        assert_eq!(found[0].path, nested.join("matching.jsonl"));
    }

    #[test]
    fn find_path_in_returns_the_matching_id() {
        let temp = tempfile::tempdir().unwrap();
        write_session(&temp.path().join("one.jsonl"), "one", "/tmp/project", "");
        let expected = temp.path().join("two.jsonl");
        write_session(&expected, "two", "/tmp/project", "");

        assert_eq!(
            find_path_in(temp.path(), "/tmp/project", "two"),
            Some(expected)
        );
    }

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
}
