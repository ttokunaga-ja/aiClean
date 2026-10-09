//! Read-only display metadata for candidates already accepted by inventory.
use super::common::plain;
use crate::model::{Provider, Session};
use rusqlite::{Connection, OpenFlags};
use serde_json::Value;
use std::io::{BufRead, BufReader};

fn text(value: &Value) -> Option<String> {
    match value {
        Value::String(s) if !s.trim().is_empty() => Some(s.clone()),
        Value::Array(items) => {
            let s = items
                .iter()
                .filter(|v| matches!(v["type"].as_str(), Some("text" | "input_text")))
                .filter_map(|v| v["text"].as_str())
                .collect::<Vec<_>>()
                .join(" ");
            (!s.trim().is_empty()).then_some(s)
        }
        _ => None,
    }
}
fn sidecar(path: &std::path::Path, suffix: &str) -> std::path::PathBuf {
    let mut name = path.as_os_str().to_os_string();
    name.push(suffix);
    name.into()
}
pub fn title(session: &Session) -> (String, Vec<String>) {
    let mut diagnostics = Vec::new();
    if session.provider == Provider::Codex {
        for db in &session.databases {
            if let Err(e) = plain(&session.root, &db.path) {
                diagnostics.push(format!("タイトルDBを安全に確認できません: {e}"));
                continue;
            }
            for suffix in ["-wal", "-shm"] {
                let side = sidecar(&db.path, suffix);
                if std::fs::symlink_metadata(&side).is_ok() && plain(&session.root, &side).is_err()
                {
                    return (
                        session.id.clone(),
                        vec!["タイトルDB補助ファイルの安全性を確認できません".into()],
                    );
                }
            }
            let wal = sidecar(&db.path, "-wal");
            let shm = sidecar(&db.path, "-shm");
            if wal.exists() && !shm.exists() {
                diagnostics.push("タイトルDBのWALに共有メモリがありません。読取によるファイル作成を避けてタイトルを省略しました".into());
                continue;
            }
            let c = match Connection::open_with_flags(
                &db.path,
                OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
            ) {
                Ok(c) => c,
                Err(e) => {
                    diagnostics.push(format!("タイトルDBを読めません: {e}"));
                    continue;
                }
            };
            {
                let title = c.query_row(
                    "SELECT title FROM threads WHERE id=?1",
                    [&session.id],
                    |r| r.get::<_, Option<String>>(0),
                );
                match title {
                    Ok(Some(t)) if !t.trim().is_empty() => return (t, diagnostics),
                    Err(e)
                        if !e.to_string().contains("no such table")
                            && !e.to_string().contains("no such column") =>
                    {
                        diagnostics.push(format!("保存済みタイトルを読めません: {e}"))
                    }
                    _ => {}
                }
            }
        }
    }
    let mut first = None;
    let mut saved = None;
    let mut summary = None;
    for file in session.files.iter().take(1) {
        if file.extension().is_none_or(|e| e != "jsonl") {
            continue;
        }
        if let Err(e) = plain(&session.root, file) {
            diagnostics.push(format!("タイトル元ファイルの安全性を確認できません: {e}"));
            continue;
        }
        let f = match std::fs::File::open(file) {
            Ok(f) => f,
            Err(e) => {
                diagnostics.push(format!("タイトル元ファイルを読めません: {e}"));
                continue;
            }
        };
        for line in BufReader::new(f).lines() {
            let line = match line {
                Ok(line) => line,
                Err(e) => {
                    diagnostics.push(format!("タイトル本文を読めません: {e}"));
                    break;
                }
            };
            let Ok(v) = serde_json::from_str::<Value>(&line) else {
                continue;
            };
            if session.provider == Provider::Claude {
                match v["type"].as_str() {
                    Some("custom-title")
                        if v["sessionId"].is_null()
                            || v["sessionId"].as_str() == session.id.rsplit('/').next() =>
                    {
                        if let Some(t) = text(&v["customTitle"]) {
                            saved = Some(t);
                        }
                    }
                    Some("summary") => {
                        if let Some(t) = text(&v["summary"]) {
                            summary = Some(t);
                        }
                    }
                    Some("user")
                        if v["isMeta"] != true
                            && v["isCompactSummary"] != true
                            && first.is_none() =>
                    {
                        first = text(&v["message"]["content"]);
                    }
                    _ => {}
                }
            } else if first.is_none() {
                if v["type"] == "event_msg" && v["payload"]["type"] == "user_message" {
                    first = text(&v["payload"]["message"]);
                } else if v["type"] == "response_item"
                    && v["payload"]["type"] == "message"
                    && v["payload"]["role"] == "user"
                {
                    first = text(&v["payload"]["content"]);
                }
            }
        }
    }
    (
        saved
            .or(summary)
            .or(first)
            .unwrap_or_else(|| session.id.clone()),
        diagnostics,
    )
}

/// Strip complete ANSI CSI/OSC sequences, controls and bidirectional formatting;
/// count Unicode scalar values, never truncate inside a UTF-8 character.
pub fn terminal_safe(s: &str, limit: usize) -> String {
    let mut out = String::new();
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' {
            match chars.next() {
                Some('[') => {
                    for c in chars.by_ref() {
                        if ('@'..='~').contains(&c) {
                            break;
                        }
                    }
                }
                Some(']') => {
                    while let Some(c) = chars.next() {
                        if c == '\u{7}' || (c == '\u{1b}' && chars.peek() == Some(&'\\')) {
                            if c == '\u{1b}' {
                                chars.next();
                            }
                            break;
                        }
                    }
                }
                _ => {}
            }
            continue;
        }
        if c.is_whitespace() {
            out.push(' ');
        } else if !c.is_control()
            && !matches!(c, '\u{200b}'..='\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2060}'..='\u{206f}' | '\u{feff}')
        {
            out.push(c);
        }
    }
    let flat = out.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut short: String = flat.chars().take(limit).collect();
    if flat.chars().count() > limit {
        short.push('…');
    }
    short
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::DatabaseRef;
    fn fixture(root: &std::path::Path, p: Provider) -> Session {
        Session {
            provider: p,
            root: root.to_path_buf(),
            id: "p/a".into(),
            last_message_ms: Some(1),
            files: vec![root.join("a.jsonl"), root.join("child.jsonl")],
            databases: vec![],
            skip_reason: None,
        }
    }
    #[test]
    fn main_file_only_summary_and_title_precedence() {
        let t = tempfile::tempdir().unwrap();
        let root = t.path().canonicalize().unwrap();
        let s = fixture(&root, Provider::Claude);
        std::fs::write(&s.files[0],"{\"type\":\"summary\",\"summary\":\"保存済み要約\"}\n{\"type\":\"user\",\"message\":{\"content\":\"質問\"}}\n").unwrap();
        std::fs::write(
            &s.files[1],
            "{\"type\":\"custom-title\",\"sessionId\":\"a\",\"customTitle\":\"子のタイトル\"}\n",
        )
        .unwrap();
        assert_eq!(title(&s).0, "保存済み要約");
        std::fs::write(&s.files[0],"{\"type\":\"user\",\"isMeta\":true,\"message\":{\"content\":\"メタ\"}}\n{\"type\":\"user\",\"message\":{\"content\":[{\"type\":\"tool_result\",\"content\":\"ツール\"}]}}\n").unwrap();
        assert_eq!(title(&s).0, s.id);
    }
    #[test]
    fn missing_sqlite_title_schema_uses_user_text_without_writes() {
        let t = tempfile::tempdir().unwrap();
        let root = t.path().canonicalize().unwrap();
        let mut s = fixture(&root, Provider::Codex);
        s.id = "a".into();
        let path = root.join("state.sqlite");
        let c = Connection::open(&path).unwrap();
        c.execute_batch("CREATE TABLE threads(id TEXT); INSERT INTO threads VALUES('a');")
            .unwrap();
        drop(c);
        s.databases.push(DatabaseRef {
            path: path.clone(),
            tables: vec![],
        });
        std::fs::write(&s.files[0],"{\"type\":\"event_msg\",\"payload\":{\"type\":\"agent_message\",\"message\":\"助手\"}}\n{\"type\":\"response_item\",\"payload\":{\"type\":\"message\",\"role\":\"user\",\"content\":[{\"type\":\"input_text\",\"text\":\"質問😀\"}]}}\n").unwrap();
        let before = std::fs::read(&path).unwrap();
        assert_eq!(title(&s).0, "質問😀");
        assert_eq!(std::fs::read(&path).unwrap(), before);
        assert_eq!(std::fs::read_dir(&root).unwrap().count(), 2);
    }
    #[test]
    fn wal_without_shm_does_not_create_state_and_reports_diagnostic() {
        let t = tempfile::tempdir().unwrap();
        let root = t.path().canonicalize().unwrap();
        let mut s = fixture(&root, Provider::Codex);
        let path = root.join("state.sqlite");
        std::fs::write(&path, b"not a database").unwrap();
        std::fs::write(root.join("state.sqlite-wal"), b"wal").unwrap();
        s.databases.push(DatabaseRef {
            path,
            tables: vec![],
        });
        let (name, diagnostics) = title(&s);
        assert_eq!(name, s.id);
        assert!(!diagnostics.is_empty());
        assert!(!root.join("state.sqlite-shm").exists());
    }
}
