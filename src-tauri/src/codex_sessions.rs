#[cfg(test)]
mod tests {
    use super::{scan_codex_sessions_at, SessionState};
    use std::fs;
    use std::path::PathBuf;
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    struct TempSessions {
        root: PathBuf,
    }

    impl TempSessions {
        fn new() -> Self {
            let unique = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let root = std::env::temp_dir().join(format!(
                "code-light-codex-sessions-{}-{unique}",
                std::process::id()
            ));
            fs::create_dir_all(&root).unwrap();
            Self { root }
        }

        fn write(&self, relative: &str, contents: &str) -> PathBuf {
            let path = self.root.join(relative);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(&path, contents).unwrap();
            path
        }
    }

    impl Drop for TempSessions {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    fn meta(id: &str) -> String {
        format!(r#"{{"type":"session_meta","payload":{{"id":"{id}"}}}}"#)
    }

    fn event(kind: &str, turn_id: &str) -> String {
        format!(r#"{{"type":"event_msg","payload":{{"type":"{kind}","turn_id":"{turn_id}"}}}}"#)
    }

    #[test]
    fn discovers_nested_logs_and_reports_active_task_as_working() {
        let sessions = TempSessions::new();
        sessions.write(
            "nested/session.jsonl",
            &format!("{}\n{}\n", meta("active-session"), event("task_started", "turn-1")),
        );

        let found = scan_codex_sessions_at(&sessions.root, SystemTime::now());

        assert_eq!(found.len(), 1);
        assert_eq!(found[0].session_id, "active-session");
        assert_eq!(found[0].state, SessionState::Working);
        assert_eq!(found[0].message, "Task working");
        assert!(found[0].timestamp > 0);
    }

    #[test]
    fn reports_completed_when_terminal_event_overrides_started_task() {
        let sessions = TempSessions::new();
        sessions.write(
            "completed.jsonl",
            &format!(
                "{}\n{}\n{}\n",
                meta("complete-session"),
                event("task_started", "turn-1"),
                event("task_complete", "turn-1")
            ),
        );
        sessions.write(
            "aborted.jsonl",
            &format!(
                "{}\n{}\n{{\"type\":\"turn_aborted\",\"turn_id\":\"turn-2\"}}\n",
                meta("aborted-session"),
                event("task_started", "turn-2")
            ),
        );

        let found = scan_codex_sessions_at(&sessions.root, SystemTime::now());

        assert_eq!(found.len(), 2);
        assert!(found.iter().all(|session| session.state == SessionState::Completed));
        assert!(found.iter().any(|session| session.message == "Task completed"));
        assert!(found.iter().any(|session| session.message == "Turn aborted"));
    }

    #[test]
    fn ignores_malformed_and_stale_logs() {
        let sessions = TempSessions::new();
        sessions.write("bad.jsonl", "not json\n");
        let stale = sessions.write(
            "stale.jsonl",
            &format!("{}\n{}\n", meta("stale-session"), event("task_started", "turn-1")),
        );
        let modified = fs::metadata(stale).unwrap().modified().unwrap();
        let found = scan_codex_sessions_at(&sessions.root, modified + Duration::from_secs(301));

        assert!(found.is_empty());
    }
}

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

const STALE_AFTER_SECS: u64 = 300;
const MAX_SESSION_LOGS: usize = 16;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionState {
    Idle,
    Working,
    Completed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodexSession {
    pub session_id: String,
    pub state: SessionState,
    pub message: String,
    pub timestamp: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TurnState {
    Active,
    Completed,
    Aborted,
}

pub fn scan_codex_sessions(root: &Path) -> Vec<CodexSession> {
    scan_codex_sessions_at(root, SystemTime::now())
}

fn scan_codex_sessions_at(root: &Path, now: SystemTime) -> Vec<CodexSession> {
    let now_secs = now.duration_since(UNIX_EPOCH).unwrap_or_default().as_secs();
    let mut logs = Vec::new();
    collect_jsonl_files(root, &mut logs);

    logs.sort_unstable_by(|left, right| right.1.cmp(&left.1));
    logs.into_iter()
        .take(MAX_SESSION_LOGS)
        .filter(|(_, modified)| now_secs.saturating_sub(*modified) <= STALE_AFTER_SECS)
        .filter_map(|(path, modified)| read_session_log(&path, modified))
        .collect()
}

fn collect_jsonl_files(root: &Path, logs: &mut Vec<(PathBuf, u64)>) {
    let Ok(entries) = fs::read_dir(root) else {
        return;
    };

    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_jsonl_files(&path, logs);
        } else if path.extension().and_then(|extension| extension.to_str()) == Some("jsonl") {
            if let Ok(modified) = entry.metadata().and_then(|metadata| metadata.modified()) {
                let timestamp = modified.duration_since(UNIX_EPOCH).unwrap_or_default().as_secs();
                logs.push((path, timestamp));
            }
        }
    }
}

fn read_session_log(path: &Path, timestamp: u64) -> Option<CodexSession> {
    let contents = fs::read_to_string(path).ok()?;
    let mut session_id = None;
    let mut turns = HashMap::new();
    let mut last_terminal = None;

    for line in contents.lines().filter(|line| !line.trim().is_empty()) {
        let value: serde_json::Value = serde_json::from_str(line).ok()?;
        let event_type = value.get("type").and_then(serde_json::Value::as_str)?;
        let payload = value.get("payload");

        if event_type == "session_meta" {
            session_id = payload
                .and_then(|payload| payload.get("id"))
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned);
            continue;
        }

        let kind = if event_type == "event_msg" {
            payload
                .and_then(|payload| payload.get("type"))
                .and_then(serde_json::Value::as_str)
        } else {
            Some(event_type)
        };
        let Some(kind) = kind else { continue };
        let turn_id = payload
            .and_then(|payload| payload.get("turn_id"))
            .or_else(|| value.get("turn_id"))
            .and_then(serde_json::Value::as_str);
        let Some(turn_id) = turn_id else { continue };

        match kind {
            "task_started" => {
                turns.insert(turn_id.to_owned(), TurnState::Active);
            }
            "task_complete" => {
                turns.insert(turn_id.to_owned(), TurnState::Completed);
                last_terminal = Some(TurnState::Completed);
            }
            "turn_aborted" => {
                turns.insert(turn_id.to_owned(), TurnState::Aborted);
                last_terminal = Some(TurnState::Aborted);
            }
            _ => {}
        }
    }

    let session_id = session_id?;
    let (state, message) = if turns.values().any(|state| *state == TurnState::Active) {
        (SessionState::Working, "Task working")
    } else if last_terminal == Some(TurnState::Aborted) {
        (SessionState::Completed, "Turn aborted")
    } else if last_terminal == Some(TurnState::Completed) {
        (SessionState::Completed, "Task completed")
    } else {
        (SessionState::Idle, "Idle")
    };

    Some(CodexSession {
        session_id,
        state,
        message: message.to_owned(),
        timestamp,
    })
}
