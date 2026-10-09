mod core;
pub use core::{maintenance_lock, record_attempt, run, status, undo};

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{DatabaseRef, Provider, Roots, Session, TableRef};
    use rusqlite::Connection;
    #[test]
    fn captures_and_undoes_exact_duplicate_rows_in_separate_wal_databases() {
        let temp = tempfile::tempdir().unwrap();
        let base = temp.path().canonicalize().unwrap();
        let root = base.join("codex");
        std::fs::create_dir(&root).unwrap();
        let roots = Roots {
            codex: root.clone(),
            claude: base.join("claude"),
        };
        let file = root.join("session.jsonl");
        std::fs::write(&file, b"private raw log\n").unwrap();
        let mut databases = Vec::new();
        for name in ["state.sqlite", "logs.sqlite"] {
            let path = root.join(name);
            let db = Connection::open(&path).unwrap();
            db.execute_batch("PRAGMA journal_mode=WAL; CREATE TABLE events(session_id TEXT, payload, other); INSERT INTO events VALUES('s',X'00ff',NULL),('s',X'00ff',NULL),('unrelated',123,2.5);").unwrap();
            databases.push(DatabaseRef {
                path,
                tables: vec![TableRef {
                    name: "events".into(),
                    id_columns: vec!["session_id".into()],
                }],
            });
        }
        let session = Session {
            provider: Provider::Codex,
            root: root.clone(),
            id: "s".into(),
            last_message_ms: Some(1),
            files: vec![file.clone()],
            databases: databases.clone(),
            skip_reason: None,
        };
        let state = base.join("state");
        let result = core::run_test(&roots, &state, Provider::Codex, &[session], "2026-10")
            .expect("cleanup must capture exact typed rows");
        assert_eq!(result["removed_sessions"], 1);
        assert!(!file.exists());
        for db in &databases {
            assert_eq!(
                Connection::open(&db.path)
                    .unwrap()
                    .query_row("SELECT count(*) FROM events", [], |r| r.get::<_, i64>(0))
                    .unwrap(),
                1
            );
        }
        core::undo_test(&roots, &state, Provider::Codex, None)
            .expect("undo must restore duplicate rows");
        assert_eq!(std::fs::read(file).unwrap(), b"private raw log\n");
        for db in &databases {
            let connection = Connection::open(&db.path).unwrap();
            assert_eq!(connection.query_row("SELECT count(*) FROM events WHERE session_id='s' AND typeof(payload)='blob' AND hex(payload)='00FF' AND other IS NULL",[],|r|r.get::<_,i64>(0)).unwrap(),2);
            assert_eq!(
                connection
                    .query_row(
                        "SELECT group_concat(rowid) FROM events WHERE session_id='s'",
                        [],
                        |r| r.get::<_, String>(0)
                    )
                    .unwrap(),
                "1,2"
            );
        }
    }
}
