use crate::model::{Inventory, Provider, Roots};
use anyhow::Result;
mod claude;
mod codex;
mod common;
mod display;
pub use display::{terminal_safe, title as display_title};
pub fn inventory(provider: Provider, roots: &Roots) -> Result<Inventory> {
    let mut inventory = match provider {
        Provider::Claude => claude::inventory(roots.get(provider))?,
        Provider::Codex => codex::inventory(roots.get(provider))?,
    };
    let mut owners =
        std::collections::BTreeMap::<std::path::PathBuf, std::collections::BTreeSet<usize>>::new();
    for (index, session) in inventory.sessions.iter().enumerate() {
        for path in &session.files {
            let key = std::fs::canonicalize(path).unwrap_or_else(|_| path.components().collect());
            #[cfg(any(windows, target_os = "macos"))]
            let key = std::path::PathBuf::from(key.to_string_lossy().to_ascii_lowercase());
            owners.entry(key).or_default().insert(index);
        }
    }
    for indices in owners.values().filter(|owners| owners.len() > 1) {
        for index in indices {
            let session = &mut inventory.sessions[*index];
            let previous = session.skip_reason.take();
            session.skip_reason = Some(format!(
                "shared source file ownership{}",
                previous.map_or(String::new(), |reason| format!("; {reason}"))
            ));
        }
    }
    Ok(inventory)
}

/// Recheck one candidate immediately before mutation. The caller must hold its
/// maintenance lock and every associated database write reservation.
pub fn verify_candidate(session: &crate::model::Session, boundary_ms: i64) -> Result<()> {
    anyhow::ensure!(
        session.skip_reason.is_none(),
        "session was not eligible in inventory"
    );
    let original = session
        .last_message_ms
        .ok_or_else(|| anyhow::anyhow!("unknown candidate message date"))?;
    anyhow::ensure!(original < boundary_ms, "candidate is in current month");
    let fresh = match session.provider {
        Provider::Claude => claude::recheck(session)?,
        Provider::Codex => codex::recheck(session)?,
    };
    anyhow::ensure!(
        fresh.skip_reason.is_none(),
        "session is no longer eligible: {}",
        fresh.skip_reason.as_deref().unwrap_or("unknown")
    );
    anyhow::ensure!(
        fresh.last_message_ms == Some(original),
        "genuine message date changed since inventory"
    );
    let mut expected = session.files.clone();
    expected.sort();
    expected.dedup();
    let mut actual = fresh.files;
    actual.sort();
    actual.dedup();
    anyhow::ensure!(
        expected == actual,
        "associated session files changed since inventory"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;
    use std::{fs, path::Path};
    fn roots(p: &Path) -> Roots {
        let p = p.canonicalize().unwrap();
        Roots {
            codex: p.join("codex"),
            claude: p.join("claude"),
        }
    }
    fn codex_fixture(p: &Path, mode: &str) -> Roots {
        let r = roots(p);
        fs::create_dir_all(r.codex.join("sessions")).unwrap();
        let rollout = r.codex.join("sessions/rollout-a.jsonl");
        fs::write(&rollout,"{\"timestamp\":\"2026-10-01T00:00:00Z\",\"type\":\"event_msg\",\"payload\":{\"type\":\"agent_message\",\"message\":\"synthetic recovery\"}}\n").unwrap();
        let c = Connection::open(r.codex.join("state_5.sqlite")).unwrap();
        c.execute_batch("CREATE TABLE threads(id TEXT PRIMARY KEY,rollout_path TEXT,history_mode TEXT); CREATE TABLE thread_spawn_edges(parent_thread_id TEXT,child_thread_id TEXT,status TEXT);").unwrap();
        c.execute(
            "INSERT INTO threads VALUES ('a',?1,?2)",
            rusqlite::params![rollout.to_str().unwrap(), mode],
        )
        .unwrap();
        r
    }
    #[test]
    fn codex_uses_original_database_completion_and_all_associated_databases() {
        let t = tempfile::tempdir().unwrap();
        let r = codex_fixture(t.path(), "database");
        let c = Connection::open(r.codex.join("thread_history_1.sqlite")).unwrap();
        c.execute_batch("CREATE TABLE thread_items(thread_id TEXT,item_type TEXT,created_at_ms INTEGER,completed_at_ms INTEGER); INSERT INTO thread_items VALUES('a','userMessage',1700000000000,NULL),('a','agentMessage',1700000000010,1700000000100),('a','commandExecution',1900000000000,NULL);").unwrap();
        let c = Connection::open(r.codex.join("logs_2.sqlite")).unwrap();
        c.execute_batch("CREATE TABLE logs(id INTEGER PRIMARY KEY,thread_id TEXT,body TEXT); INSERT INTO logs VALUES(1,'a','private');").unwrap();
        let v = inventory(Provider::Codex, &r).unwrap();
        let s = &v.sessions[0];
        assert_eq!(s.last_message_ms, Some(1700000000100));
        assert_eq!(s.skip_reason, None);
        assert_eq!(s.databases.len(), 3);
    }
    #[test]
    fn codex_incomplete_assistant_blocks_cleanup() {
        let t = tempfile::tempdir().unwrap();
        let r = codex_fixture(t.path(), "database");
        let c = Connection::open(r.codex.join("thread_history_1.sqlite")).unwrap();
        c.execute_batch("CREATE TABLE thread_items(thread_id TEXT,item_type TEXT,created_at_ms INTEGER,completed_at_ms INTEGER); INSERT INTO thread_items VALUES('a','agentMessage',1700000000000,NULL);").unwrap();
        assert!(
            inventory(Provider::Codex, &r).unwrap().sessions[0]
                .skip_reason
                .is_some()
        );
    }
    #[test]
    fn codex_spawn_edges_block_both_relatives() {
        let t = tempfile::tempdir().unwrap();
        let r = codex_fixture(t.path(), "legacy");
        let c = Connection::open(r.codex.join("state_5.sqlite")).unwrap();
        c.execute_batch("INSERT INTO thread_spawn_edges VALUES('a','child','completed');")
            .unwrap();
        assert!(
            inventory(Provider::Codex, &r).unwrap().sessions[0]
                .skip_reason
                .as_deref()
                .unwrap()
                .contains("relation")
        );
    }
    #[test]
    fn codex_queued_work_and_active_goal_are_skipped() {
        for (name, table, reason) in [
            (
                "queue_1.sqlite",
                "CREATE TABLE queued_items(thread_id TEXT); INSERT INTO queued_items VALUES('a');",
                "queued",
            ),
            (
                "goals_1.sqlite",
                "CREATE TABLE thread_goals(thread_id TEXT,status TEXT); INSERT INTO thread_goals VALUES('a','active');",
                "goal",
            ),
        ] {
            let t = tempfile::tempdir().unwrap();
            let r = codex_fixture(t.path(), "legacy");
            Connection::open(r.codex.join(name))
                .unwrap()
                .execute_batch(table)
                .unwrap();
            assert!(
                inventory(Provider::Codex, &r).unwrap().sessions[0]
                    .skip_reason
                    .as_deref()
                    .unwrap()
                    .contains(reason)
            );
        }
    }
    #[test]
    fn codex_database_without_dates_never_uses_recovery_timestamp() {
        let t = tempfile::tempdir().unwrap();
        let r = codex_fixture(t.path(), "database");
        let v = inventory(Provider::Codex, &r).unwrap();
        assert!(v.sessions[0].last_message_ms.is_none());
        assert!(v.sessions[0].skip_reason.is_some());
    }
    #[test]
    fn codex_unregistered_rollout_is_retained() {
        let t = tempfile::tempdir().unwrap();
        let r = codex_fixture(t.path(), "legacy");
        fs::write(r.codex.join("sessions/orphan.jsonl"), "private log").unwrap();
        let v = inventory(Provider::Codex, &r).unwrap();
        let s = v
            .sessions
            .iter()
            .find(|s| s.id.starts_with("unregistered:"))
            .unwrap();
        assert!(s.skip_reason.as_deref().unwrap().contains("unregistered"));
        assert!(s.last_message_ms.is_none());
    }
    #[test]
    fn claude_child_dates_ignore_tools_and_project_index_blocks() {
        let t = tempfile::tempdir().unwrap();
        let r = roots(t.path());
        let p = r.claude.join("projects/project");
        fs::create_dir_all(p.join("abc/subagents")).unwrap();
        fs::write(p.join("abc.jsonl"),"{\"type\":\"user\",\"timestamp\":\"2023-11-14T22:13:20Z\",\"message\":{\"role\":\"user\",\"content\":\"hello\"}}\n{\"type\":\"user\",\"timestamp\":\"2026-10-01T00:00:00Z\",\"message\":{\"content\":[{\"type\":\"tool_result\",\"tool_use_id\":\"fixture-call\"}]}}\n").unwrap();
        fs::write(p.join("abc/subagents/agent.jsonl"),"{\"type\":\"assistant\",\"timestamp\":\"2023-11-14T22:13:21Z\",\"message\":{\"role\":\"assistant\",\"content\":[{\"type\":\"text\",\"text\":\"reply\"}]}}\n").unwrap();
        let v = inventory(Provider::Claude, &r).unwrap();
        assert_eq!(v.sessions[0].last_message_ms, Some(1700000001000));
        assert_eq!(v.sessions[0].files.len(), 2);
        assert!(v.sessions[0].skip_reason.is_none());
        fs::write(p.join("sessions-index.json"), "{}").unwrap();
        let v = inventory(Provider::Claude, &r).unwrap();
        assert!(
            v.sessions[0]
                .skip_reason
                .as_deref()
                .unwrap()
                .contains("index")
        );
        assert!(!v.warnings.is_empty());
    }
    #[test]
    fn claude_global_prompt_history_is_preserved_and_does_not_block() {
        let t = tempfile::tempdir().unwrap();
        let r = roots(t.path());
        let p = r.claude.join("projects/project");
        fs::create_dir_all(&p).unwrap();
        fs::write(p.join("abc.jsonl"),"{\"type\":\"user\",\"timestamp\":\"2023-11-14T22:13:20Z\",\"message\":{\"content\":\"hello\"}}\n").unwrap();
        let history = "{\"display\":\"prompt\",\"pastedContents\":{},\"timestamp\":1700000000000,\"project\":\"project\",\"sessionId\":\"abc\"}\n";
        fs::write(r.claude.join("history.jsonl"), history).unwrap();
        let v = inventory(Provider::Claude, &r).unwrap();
        assert!(v.sessions[0].skip_reason.is_none());
        assert_eq!(v.sessions[0].files.len(), 1);
        assert_eq!(
            fs::read_to_string(r.claude.join("history.jsonl")).unwrap(),
            history
        );
    }
    #[test]
    fn malformed_message_and_metadata_only_are_unknown() {
        let t = tempfile::tempdir().unwrap();
        let r = roots(t.path());
        let p = r.claude.join("projects/project");
        fs::create_dir_all(&p).unwrap();
        fs::write(
            p.join("abc.jsonl"),
            "{\"type\":\"summary\",\"timestamp\":\"2023-11-14T22:13:20Z\"}\n",
        )
        .unwrap();
        assert!(
            inventory(Provider::Claude, &r).unwrap().sessions[0]
                .skip_reason
                .is_some()
        );
        fs::write(p.join("abc.jsonl"), "not JSON\n").unwrap();
        assert!(
            inventory(Provider::Claude, &r).unwrap().sessions[0]
                .skip_reason
                .is_some()
        );
    }
    #[test]
    fn recheck_refuses_new_database_message_and_changed_rollout_reference() {
        let t = tempfile::tempdir().unwrap();
        let r = codex_fixture(t.path(), "database");
        let c = Connection::open(r.codex.join("thread_history_1.sqlite")).unwrap();
        c.execute_batch("CREATE TABLE thread_items(thread_id TEXT,item_type TEXT,created_at_ms INTEGER,completed_at_ms INTEGER); INSERT INTO thread_items VALUES('a','agentMessage',1700000000000,1700000000100);").unwrap();
        let s = inventory(Provider::Codex, &r).unwrap().sessions.remove(0);
        verify_candidate(&s, 1704067200000).unwrap();
        c.execute_batch("INSERT INTO thread_items VALUES('a','userMessage',1705000000000,NULL)")
            .unwrap();
        assert!(verify_candidate(&s, 1704067200000).is_err());
        c.execute_batch("DELETE FROM thread_items WHERE item_type='userMessage'")
            .unwrap();
        let state = Connection::open(r.codex.join("state_5.sqlite")).unwrap();
        let other = r.codex.join("sessions/other.jsonl");
        fs::write(&other, "{}").unwrap();
        state
            .execute(
                "UPDATE threads SET rollout_path=?1",
                [other.to_str().unwrap()],
            )
            .unwrap();
        assert!(verify_candidate(&s, 1704067200000).is_err());
    }
    #[test]
    fn recheck_works_under_database_write_reservations_and_refuses_queued_work() {
        let t = tempfile::tempdir().unwrap();
        let r = codex_fixture(t.path(), "database");
        let history = Connection::open(r.codex.join("thread_history_1.sqlite")).unwrap();
        history.execute_batch("PRAGMA journal_mode=WAL; CREATE TABLE thread_items(thread_id TEXT,item_type TEXT,created_at_ms INTEGER,completed_at_ms INTEGER); INSERT INTO thread_items VALUES('a','agentMessage',1700000000000,1700000000100);").unwrap();
        let queue = Connection::open(r.codex.join("queue_1.sqlite")).unwrap();
        queue
            .execute_batch("PRAGMA journal_mode=WAL;CREATE TABLE queued_items(thread_id TEXT)")
            .unwrap();
        let s = inventory(Provider::Codex, &r).unwrap().sessions.remove(0);
        let state = Connection::open(r.codex.join("state_5.sqlite")).unwrap();
        for db in [&state, &history, &queue] {
            db.execute_batch("BEGIN IMMEDIATE").unwrap();
        }
        verify_candidate(&s, 1704067200000).unwrap();
        for db in [&state, &history, &queue] {
            db.execute_batch("ROLLBACK").unwrap();
        }
        queue
            .execute("INSERT INTO queued_items VALUES('a')", [])
            .unwrap();
        assert!(verify_candidate(&s, 1704067200000).is_err());
    }
    #[test]
    fn recheck_refuses_changed_raw_date_and_new_child_group() {
        let t = tempfile::tempdir().unwrap();
        let r = roots(t.path());
        let p = r.claude.join("projects/project");
        fs::create_dir_all(&p).unwrap();
        let old = "{\"type\":\"user\",\"timestamp\":\"2023-11-14T22:13:20Z\",\"message\":{\"content\":\"hello\"}}\n";
        fs::write(p.join("abc.jsonl"), old).unwrap();
        let s = inventory(Provider::Claude, &r).unwrap().sessions.remove(0);
        verify_candidate(&s, 1704067200000).unwrap();
        fs::write(p.join("abc.jsonl"), old.replace("2023-11-14", "2024-02-14")).unwrap();
        assert!(verify_candidate(&s, 1704067200000).is_err());
        fs::write(p.join("abc.jsonl"), old).unwrap();
        fs::create_dir_all(p.join("abc/subagents")).unwrap();
        fs::write(p.join("abc/subagents/new.jsonl"), old).unwrap();
        assert!(verify_candidate(&s, 1704067200000).is_err());
    }
    #[test]
    fn all_owners_of_shared_rollout_are_skipped_including_newer_retained_thread() {
        let t = tempfile::tempdir().unwrap();
        let r = codex_fixture(t.path(), "database");
        let state = Connection::open(r.codex.join("state_5.sqlite")).unwrap();
        let history = Connection::open(r.codex.join("thread_history_1.sqlite")).unwrap();
        history.execute_batch("CREATE TABLE thread_items(thread_id TEXT,item_type TEXT,created_at_ms INTEGER,completed_at_ms INTEGER);INSERT INTO thread_items VALUES('a','agentMessage',1700000000000,1700000000100)").unwrap();
        let candidate = inventory(Provider::Codex, &r).unwrap().sessions.remove(0);
        state.execute_batch("INSERT INTO threads SELECT 'b',rollout_path,history_mode FROM threads WHERE id='a'").unwrap();
        assert!(verify_candidate(&candidate, 1704067200000).is_err());
        history
            .execute(
                "INSERT INTO thread_items VALUES('b','userMessage',?1,NULL)",
                [chrono::Utc::now().timestamp_millis()],
            )
            .unwrap();
        let v = inventory(Provider::Codex, &r).unwrap();
        assert_eq!(v.sessions.len(), 2);
        assert!(v.sessions.iter().all(|s| {
            s.skip_reason
                .as_deref()
                .is_some_and(|r| r.contains("shared source"))
        }));
        let alias = r.codex.join("sessions/../sessions/rollout-a.jsonl");
        state
            .execute(
                "UPDATE threads SET rollout_path=?1 WHERE id='b'",
                [alias.to_str().unwrap()],
            )
            .unwrap();
        assert!(verify_candidate(&candidate, 1704067200000).is_err());
        assert!(
            inventory(Provider::Codex, &r)
                .unwrap()
                .sessions
                .iter()
                .all(|s| s
                    .skip_reason
                    .as_deref()
                    .is_some_and(|s| s.contains("shared source")))
        );
    }
    #[test]
    fn image_only_user_send_counts_and_pending_claude_tool_call_blocks() {
        let t = tempfile::tempdir().unwrap();
        let r = roots(t.path());
        let p = r.claude.join("projects/project");
        fs::create_dir_all(&p).unwrap();
        let old = "{\"type\":\"user\",\"timestamp\":\"2023-11-14T22:13:20Z\",\"message\":{\"content\":\"hello\"}}\n";
        fs::write(p.join("abc.jsonl"), old).unwrap();
        let candidate = inventory(Provider::Claude, &r).unwrap().sessions.remove(0);
        let current = chrono::Utc::now().to_rfc3339();
        let image = serde_json::json!({"type":"user","timestamp":current,"message":{"content":[{"type":"image","source":{"type":"base64","data":"fixture"}}]}});
        fs::write(p.join("abc.jsonl"), format!("{old}{image}\n")).unwrap();
        let v = inventory(Provider::Claude, &r).unwrap();
        assert!(v.sessions[0].last_message_ms.unwrap() > 1704067200000);
        assert!(verify_candidate(&candidate, 1704067200000).is_err());
        let tool = serde_json::json!({"type":"assistant","timestamp":"2023-11-14T22:13:21Z","message":{"stop_reason":"tool_use","content":[{"type":"tool_use","id":"call-1","name":"read","input":{}}]}});
        fs::write(p.join("abc.jsonl"), format!("{old}{tool}\n")).unwrap();
        assert!(
            inventory(Provider::Claude, &r).unwrap().sessions[0]
                .skip_reason
                .as_deref()
                .is_some_and(|s| s.contains("tool"))
        );
        let result = serde_json::json!({"type":"user","timestamp":"2023-11-14T22:13:22Z","message":{"content":[{"type":"tool_result","tool_use_id":"call-1","content":"done"}]}});
        fs::write(p.join("abc.jsonl"), format!("{old}{tool}\n{result}\n")).unwrap();
        assert!(
            inventory(Provider::Claude, &r).unwrap().sessions[0]
                .skip_reason
                .is_some()
        );
        let final_reply = serde_json::json!({"type":"assistant","timestamp":"2023-11-14T22:13:23Z","message":{"stop_reason":"end_turn","content":[{"type":"text","text":"done"}]}});
        fs::write(
            p.join("abc.jsonl"),
            format!("{old}{tool}\n{result}\n{final_reply}\n"),
        )
        .unwrap();
        assert!(
            inventory(Provider::Claude, &r).unwrap().sessions[0]
                .skip_reason
                .is_none()
        );
        let unknown = serde_json::json!({"type":"user","timestamp":current,"message":{"content":[{"type":"unknown_new_block"}]}});
        fs::write(p.join("abc.jsonl"), format!("{old}{unknown}\n")).unwrap();
        assert!(
            inventory(Provider::Claude, &r).unwrap().sessions[0]
                .skip_reason
                .is_some()
        );
    }
    #[test]
    fn streamed_claude_tool_stop_is_resolved_by_same_message_id_and_meta_result() {
        let t = tempfile::tempdir().unwrap();
        let r = roots(t.path());
        let p = r.claude.join("projects/project");
        fs::create_dir_all(&p).unwrap();
        let rows = [
            serde_json::json!({"type":"user","timestamp":"2023-11-14T22:13:20Z","message":{"content":"hello"}}),
            serde_json::json!({"type":"assistant","timestamp":"2023-11-14T22:13:21Z","message":{"id":"m1","stop_reason":"tool_use","content":[{"type":"text","text":"working"}]}}),
            serde_json::json!({"type":"assistant","timestamp":"2023-11-14T22:13:21Z","message":{"id":"m1","stop_reason":"tool_use","content":[{"type":"tool_use","id":"call-1","name":"read","input":{}}]}}),
            serde_json::json!({"type":"user","isMeta":true,"timestamp":"2026-10-01T00:00:00Z","message":{"content":[{"type":"tool_result","tool_use_id":"call-1","content":"done"}]}}),
            serde_json::json!({"type":"assistant","timestamp":"2023-11-14T22:13:23Z","message":{"id":"m2","stop_reason":"end_turn","content":[{"type":"text","text":"finished"}]}}),
        ];
        fs::write(
            p.join("abc.jsonl"),
            rows.iter().map(|v| format!("{v}\n")).collect::<String>(),
        )
        .unwrap();
        let v = inventory(Provider::Claude, &r).unwrap();
        assert!(v.sessions[0].skip_reason.is_none());
        assert_eq!(v.sessions[0].last_message_ms, Some(1700000003000));
        fs::write(
            p.join("abc.jsonl"),
            rows[..2]
                .iter()
                .map(|v| format!("{v}\n"))
                .collect::<String>(),
        )
        .unwrap();
        assert!(
            inventory(Provider::Claude, &r).unwrap().sessions[0]
                .skip_reason
                .is_some()
        );
    }
    #[test]
    fn unsupported_inbound_foreign_keys_block_and_covered_composite_is_allowed() {
        for schema in [
            "CREATE TABLE private_history(session_key TEXT REFERENCES threads(id),payload TEXT)",
            "CREATE TABLE private_history(id TEXT PRIMARY KEY,thread_id TEXT REFERENCES threads(id)); CREATE TABLE leaf(record_id TEXT REFERENCES private_history(id))",
        ] {
            let t = tempfile::tempdir().unwrap();
            let r = codex_fixture(t.path(), "legacy");
            Connection::open(r.codex.join("state_5.sqlite"))
                .unwrap()
                .execute_batch(schema)
                .unwrap();
            assert!(
                inventory(Provider::Codex, &r).unwrap().sessions[0]
                    .skip_reason
                    .as_deref()
                    .is_some_and(|s| s.contains("foreign key"))
            );
        }
        let t = tempfile::tempdir().unwrap();
        let r = codex_fixture(t.path(), "legacy");
        Connection::open(r.codex.join("state_5.sqlite")).unwrap().execute_batch("CREATE TABLE thread_turns(thread_id TEXT,turn_id TEXT,status TEXT,PRIMARY KEY(thread_id,turn_id));CREATE TABLE known_children(thread_id TEXT,turn_id TEXT,FOREIGN KEY(thread_id,turn_id) REFERENCES thread_turns(thread_id,turn_id))").unwrap();
        let candidate = inventory(Provider::Codex, &r).unwrap().sessions.remove(0);
        assert!(candidate.skip_reason.is_none());
        verify_candidate(
            &candidate,
            chrono::Utc::now().timestamp_millis() + 86_400_000,
        )
        .unwrap();
        Connection::open(r.codex.join("state_5.sqlite"))
            .unwrap()
            .execute_batch("CREATE TABLE novel_private(session_key TEXT REFERENCES threads(id))")
            .unwrap();
        assert!(
            verify_candidate(
                &candidate,
                chrono::Utc::now().timestamp_millis() + 86_400_000
            )
            .is_err()
        );
    }
    #[test]
    fn arbitrary_provider_file_cannot_be_a_registered_rollout() {
        let t = tempfile::tempdir().unwrap();
        let r = codex_fixture(t.path(), "database");
        let protected = r.codex.join("auth.json");
        fs::write(&protected, "protected fixture").unwrap();
        Connection::open(r.codex.join("state_5.sqlite"))
            .unwrap()
            .execute(
                "UPDATE threads SET rollout_path=?1",
                [protected.to_str().unwrap()],
            )
            .unwrap();
        Connection::open(r.codex.join("thread_history_1.sqlite")).unwrap().execute_batch("CREATE TABLE thread_items(thread_id TEXT,item_type TEXT,created_at_ms INTEGER,completed_at_ms INTEGER);INSERT INTO thread_items VALUES('a','agentMessage',1700000000000,1700000000100)").unwrap();
        let v = inventory(Provider::Codex, &r).unwrap();
        let s = v.sessions.iter().find(|s| s.id == "a").unwrap();
        assert!(
            s.skip_reason
                .as_deref()
                .is_some_and(|s| s.contains("rollout location"))
        );
        assert_eq!(fs::read_to_string(protected).unwrap(), "protected fixture");
    }
    #[cfg(unix)]
    #[test]
    fn hardlinked_rollout_and_auxiliary_database_are_retained() {
        let t = tempfile::tempdir().unwrap();
        let r = codex_fixture(t.path(), "legacy");
        fs::hard_link(
            r.codex.join("sessions/rollout-a.jsonl"),
            t.path().join("outside-rollout"),
        )
        .unwrap();
        let v = inventory(Provider::Codex, &r).unwrap();
        assert!(v.sessions[0].skip_reason.is_some());
        fs::remove_file(t.path().join("outside-rollout")).unwrap();
        let db = r.codex.join("logs_2.sqlite");
        Connection::open(&db)
            .unwrap()
            .execute_batch("CREATE TABLE logs(thread_id TEXT)")
            .unwrap();
        fs::hard_link(&db, t.path().join("outside-db")).unwrap();
        let v = inventory(Provider::Codex, &r).unwrap();
        assert!(v.sessions.iter().all(|s| s.skip_reason.is_some()));
        assert!(!v.warnings.is_empty());
    }
    #[cfg(unix)]
    #[test]
    fn linked_rollout_is_skipped() {
        let t = tempfile::tempdir().unwrap();
        let r = codex_fixture(t.path(), "legacy");
        fs::remove_file(r.codex.join("sessions/rollout-a.jsonl")).unwrap();
        fs::write(t.path().join("outside"), "{}").unwrap();
        std::os::unix::fs::symlink(
            t.path().join("outside"),
            r.codex.join("sessions/rollout-a.jsonl"),
        )
        .unwrap();
        assert!(
            inventory(Provider::Codex, &r).unwrap().sessions[0]
                .skip_reason
                .is_some()
        );
    }
}
