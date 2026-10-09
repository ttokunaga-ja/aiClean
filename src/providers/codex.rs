use super::common::{messages, plain, quote, root_path, skip, valid_ms};
use crate::model::{DatabaseRef, Inventory, Provider, Session, TableRef};
use anyhow::{Context, Result};
use rusqlite::{Connection, OpenFlags};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Path, PathBuf},
};
type ForeignKey = Vec<(String, String, Option<String>)>;
struct Table {
    name: String,
    columns: BTreeSet<String>,
    refs: Vec<String>,
    ordinary: bool,
    foreign_keys: Vec<ForeignKey>,
}
struct Db {
    path: PathBuf,
    connection: Connection,
    tables: Vec<Table>,
}
fn schemas(connection: &Connection) -> Result<Vec<Table>> {
    let mut stmt=connection.prepare("SELECT name,COALESCE(sql,'') FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%'")?;
    let names = stmt
        .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    names
        .into_iter()
        .map(|(name, sql)| {
            let mut stmt = connection.prepare(&format!("PRAGMA table_info({})", quote(&name)))?;
            let columns = stmt
                .query_map([], |r| r.get::<_, String>(1))?
                .collect::<rusqlite::Result<BTreeSet<_>>>()?;
            let refs = if name == "threads" && columns.contains("id") {
                vec!["id".into()]
            } else {
                ["thread_id", "parent_thread_id", "child_thread_id"]
                    .into_iter()
                    .filter(|c| columns.contains(*c))
                    .map(String::from)
                    .collect()
            };
            let mut statement =
                connection.prepare(&format!("PRAGMA foreign_key_list({})", quote(&name)))?;
            let mut groups = BTreeMap::<i64, ForeignKey>::new();
            for row in statement.query_map([], |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, String>(3)?,
                    r.get::<_, Option<String>>(4)?,
                ))
            })? {
                let (id, parent, from, to) = row?;
                groups.entry(id).or_default().push((parent, from, to));
            }
            Ok(Table {
                foreign_keys: groups.into_values().collect(),
                name,
                columns,
                refs,
                ordinary: !sql.to_uppercase().contains("VIRTUAL TABLE"),
            })
        })
        .collect()
}
// Follow inbound foreign keys from all thread-selector tables. A dependent row
// is covered only if its FK carries the same root thread selector to its parent.
fn unsupported_foreign_keys(tables: &[Table]) -> bool {
    let mut reachable = tables
        .iter()
        .filter(|t| !t.refs.is_empty())
        .map(|t| t.name.to_ascii_lowercase())
        .collect::<BTreeSet<_>>();
    loop {
        let before = reachable.len();
        for table in tables.iter().filter(|t| t.ordinary) {
            for group in &table.foreign_keys {
                let Some((parent_name, _, _)) = group.first() else {
                    continue;
                };
                if !reachable.contains(&parent_name.to_ascii_lowercase()) {
                    continue;
                }
                reachable.insert(table.name.to_ascii_lowercase());
                let Some(parent) = tables
                    .iter()
                    .find(|t| t.name.eq_ignore_ascii_case(parent_name))
                else {
                    return true;
                };
                let covered = parent.refs.len() == 1
                    && group.iter().any(|(_, from, to)| {
                        table
                            .refs
                            .iter()
                            .any(|root| root.eq_ignore_ascii_case(from))
                            && to
                                .as_ref()
                                .is_some_and(|to| parent.refs[0].eq_ignore_ascii_case(to))
                    });
                if !covered {
                    return true;
                }
            }
        }
        if reachable.len() == before {
            return false;
        }
    }
}
fn guard_ids(
    db: &Db,
    table: &Table,
    condition: &str,
    reason: &str,
    sessions: &mut BTreeMap<String, Session>,
) -> Result<()> {
    for col in &table.refs {
        let sql = format!(
            "SELECT DISTINCT {} FROM {} WHERE {}",
            quote(col),
            quote(&table.name),
            condition
        );
        let mut stmt = db.connection.prepare(&sql)?;
        for id in stmt.query_map([], |r| r.get::<_, Option<String>>(0))? {
            if let Some(s) = id?.and_then(|id| sessions.get_mut(&id)) {
                skip(s, reason);
            }
        }
    }
    Ok(())
}
fn supported_rollout(root: &Path, path: &Path) -> Result<()> {
    plain(root, path)?;
    anyhow::ensure!(
        path.extension().and_then(|s| s.to_str()) == Some("jsonl")
            && (path.starts_with(root.join("sessions"))
                || path.starts_with(root.join("archived_sessions"))),
        "unsupported registered rollout location"
    );
    anyhow::ensure!(path.is_file(), "missing rollout file");
    Ok(())
}
pub(super) fn inventory(input: &Path) -> Result<Inventory> {
    let mut output = Inventory {
        provider: Provider::Codex,
        sessions: vec![],
        warnings: vec![],
    };
    if !input.exists() {
        return Ok(output);
    }
    let root = root_path(input)?;
    let mut databases = vec![];
    let mut global_issue = None;
    let mut paths = fs::read_dir(&root)?
        .map(|e| e.map(|e| e.path()))
        .collect::<std::io::Result<Vec<_>>>()?;
    paths.sort();
    for path in paths {
        if path.extension().and_then(|x| x.to_str()) != Some("sqlite") {
            continue;
        }
        let opened = (|| -> Result<Db> {
            plain(&root, &path)?;
            for suffix in ["-wal", "-shm"] {
                let side = PathBuf::from(format!("{}{suffix}", path.display()));
                if fs::symlink_metadata(&side).is_ok() {
                    plain(&root, &side)?;
                }
            }
            let connection = Connection::open_with_flags(
                &path,
                OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
            )?;
            connection.execute_batch("PRAGMA query_only=ON; BEGIN")?;
            let tables = schemas(&connection)?;
            Ok(Db {
                path: path.clone(),
                connection,
                tables,
            })
        })();
        match opened {
            Ok(db) => databases.push(db),
            Err(e) => {
                output.warnings.push(format!("{}: {e}", path.display()));
                global_issue = Some("unsafe or unreadable provider database");
            }
        }
    }
    let mut sessions = BTreeMap::<String, Session>::new();
    let mut modes = BTreeMap::<String, String>::new();
    for db in &databases {
        if !db
            .path
            .file_name()
            .and_then(|x| x.to_str())
            .is_some_and(|x| x.starts_with("state_"))
        {
            continue;
        }
        let Some(table) = db.tables.iter().find(|t| t.name == "threads") else {
            continue;
        };
        if !table.columns.contains("rollout_path")
            || !table.columns.contains("id")
            || !table.ordinary
        {
            global_issue = Some("unsupported state threads schema");
            continue;
        }
        let mode = if table.columns.contains("history_mode") {
            quote("history_mode")
        } else {
            "'legacy'".into()
        };
        let mut stmt = db
            .connection
            .prepare(&format!("SELECT id,rollout_path,{mode} FROM threads"))?;
        for row in stmt.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
            ))
        })? {
            let (id, path, mode) = row?;
            if let Some(s) = sessions.get_mut(&id) {
                skip(s, "duplicate thread registration across state databases");
                continue;
            }
            let path = PathBuf::from(path);
            let path = if path.is_absolute() {
                path
            } else {
                root.join(path)
            };
            let mut s = Session {
                provider: Provider::Codex,
                root: root.clone(),
                id: id.clone(),
                last_message_ms: None,
                files: vec![path.clone()],
                databases: vec![],
                skip_reason: None,
            };
            if let Err(e) = supported_rollout(&root, &path) {
                skip(&mut s, e.to_string());
            }
            modes.insert(id.clone(), mode);
            sessions.insert(id, s);
        }
    }
    for db in &databases {
        if unsupported_foreign_keys(&db.tables) {
            global_issue = Some("unsupported inbound foreign key relation");
            output.warnings.push(format!(
                "{}: unsupported inbound foreign key relation; sessions retained",
                db.path.display()
            ));
        }
        let mut refs = vec![];
        for table in &db.tables {
            if table.refs.is_empty() {
                continue;
            }
            if !table.ordinary {
                global_issue = Some("unsupported virtual database relation");
                continue;
            }
            refs.push(TableRef {
                name: table.name.clone(),
                id_columns: table.refs.clone(),
            });
            if table
                .refs
                .iter()
                .any(|c| matches!(c.as_str(), "parent_thread_id" | "child_thread_id"))
            {
                guard_ids(
                    db,
                    table,
                    "1",
                    "spawn or parent/child relation has unsupported deletion scope",
                    &mut sessions,
                )?;
            }
            match table.name.as_str() {
                "thread_turns" => {
                    if !table.columns.contains("status") {
                        global_issue = Some("unsupported turn status schema");
                    } else {
                        guard_ids(
                            db,
                            table,
                            "status IS NULL OR status NOT IN ('completed','failed','interrupted')",
                            "incomplete or unknown turn status",
                            &mut sessions,
                        )?;
                    }
                }
                "queued_items" => guard_ids(db, table, "1", "queued work", &mut sessions)?,
                "thread_goals" => {
                    if !table.columns.contains("status") {
                        global_issue = Some("unsupported goal status schema");
                    } else {
                        guard_ids(
                            db,
                            table,
                            "status IS NULL OR status NOT IN ('complete','paused','blocked','usage_limited','budget_limited')",
                            "active or unknown goal",
                            &mut sessions,
                        )?;
                    }
                }
                "thread_items" => {
                    if !["thread_id", "item_type", "created_at_ms", "completed_at_ms"]
                        .iter()
                        .all(|c| table.columns.contains(*c))
                    {
                        global_issue = Some("unsupported message history schema");
                        continue;
                    }
                    // Only genuine message projections are read, once per database, without item_json bodies.
                    let mut stmt=db.connection.prepare("SELECT thread_id, MAX(CASE WHEN item_type='userMessage' THEN created_at_ms ELSE completed_at_ms END), MIN(CASE WHEN item_type='userMessage' THEN created_at_ms ELSE completed_at_ms END), SUM(CASE WHEN (item_type='userMessage' AND created_at_ms IS NULL) OR (item_type='agentMessage' AND completed_at_ms IS NULL) THEN 1 ELSE 0 END) FROM thread_items WHERE item_type IN ('userMessage','agentMessage') GROUP BY thread_id")?;
                    for row in stmt.query_map([], |r| {
                        Ok((
                            r.get::<_, String>(0)?,
                            r.get::<_, Option<i64>>(1)?,
                            r.get::<_, Option<i64>>(2)?,
                            r.get::<_, i64>(3)?,
                        ))
                    })? {
                        let (id, max, min, incomplete) = row?;
                        let Some(s) = sessions.get_mut(&id) else {
                            continue;
                        };
                        if incomplete > 0 {
                            skip(s, "incomplete message timestamp");
                        }
                        if let Some(ts) = max {
                            match valid_ms(ts) {
                                Ok(ts) => {
                                    s.last_message_ms =
                                        Some(s.last_message_ms.map_or(ts, |old| old.max(ts)))
                                }
                                Err(e) => skip(s, e.to_string()),
                            }
                        }
                        if let Some(ts) = min
                            && let Err(e) = valid_ms(ts)
                        {
                            skip(s, e.to_string());
                        }
                    }
                }
                "thread_realtime_items" => {
                    if table.columns.contains("item_type") {
                        guard_ids(
                            db,
                            table,
                            "item_type IN ('userMessage','agentMessage')",
                            "unsupported realtime message completion history",
                            &mut sessions,
                        )?;
                    } else {
                        global_issue = Some("unsupported realtime message schema");
                    }
                }
                _ => {}
            }
        }
        {
            let reference = DatabaseRef {
                path: db.path.clone(),
                tables: refs,
            };
            // References with no rows are harmless; avoid scanning very large ancillary logs for each thread.
            for s in sessions.values_mut() {
                s.databases.push(reference.clone());
            }
        }
    }
    for s in sessions.values_mut() {
        if let Some(reason) = global_issue {
            skip(s, reason);
        }
        if s.last_message_ms.is_none() && s.skip_reason.is_none() {
            if modes.get(&s.id).is_some_and(|m| m == "legacy") {
                match messages(&s.files[0], false) {
                    Ok(Some(ts)) => s.last_message_ms = Some(ts),
                    Ok(None) => skip(s, "unknown last conversation timestamp"),
                    Err(e) => skip(s, e.to_string()),
                }
            } else {
                skip(s, "unknown database message timestamp");
            }
        }
        if s.last_message_ms.is_none() {
            skip(s, "unknown last conversation timestamp");
        }
    }
    let registered = sessions
        .values()
        .flat_map(|s| s.files.iter().cloned())
        .collect::<BTreeSet<_>>();
    for directory in ["sessions", "archived_sessions"] {
        let directory = root.join(directory);
        if !directory.exists() {
            continue;
        }
        if let Err(e) = plain(&root, &directory) {
            output.warnings.push(e.to_string());
            continue;
        }
        for entry in walkdir::WalkDir::new(&directory).follow_links(false) {
            let entry = match entry {
                Ok(e) => e,
                Err(_) => {
                    output
                        .warnings
                        .push("unreadable raw rollout inventory".into());
                    continue;
                }
            };
            let path = entry.path();
            if path.extension().and_then(|s| s.to_str()) != Some("jsonl")
                || registered.contains(path)
            {
                continue;
            }
            output.sessions.push(Session {
                provider: Provider::Codex,
                root: root.clone(),
                id: format!(
                    "unregistered:{}",
                    path.strip_prefix(&root).unwrap_or(path).display()
                ),
                last_message_ms: None,
                files: vec![path.to_path_buf()],
                databases: vec![],
                skip_reason: Some("unregistered raw rollout retained".into()),
            });
        }
    }
    output.sessions.extend(sessions.into_values());
    output.sessions.sort_by(|a, b| a.id.cmp(&b.id));
    Ok(output)
}

/// Targeted eligibility queries while the caller holds every database write reservation.
pub(super) fn recheck(original: &Session) -> Result<Session> {
    let root = root_path(&original.root)?;
    let candidate_identities = original
        .files
        .iter()
        .map(|path| {
            plain(&root, path)?;
            fs::canonicalize(path).context("unknown candidate source identity")
        })
        .collect::<Result<Vec<_>>>()?;
    let mut paths = fs::read_dir(&root)?
        .map(|e| e.map(|e| e.path()))
        .collect::<std::io::Result<Vec<_>>>()?;
    paths.retain(|p| p.extension().and_then(|s| s.to_str()) == Some("sqlite"));
    paths.sort();
    let mut expected = original
        .databases
        .iter()
        .map(|d| d.path.clone())
        .collect::<Vec<_>>();
    expected.sort();
    anyhow::ensure!(
        paths == expected,
        "provider database set changed since inventory"
    );
    let mut fresh = original.clone();
    fresh.last_message_ms = None;
    fresh.files.clear();
    fresh.skip_reason = None;
    let mut registered = 0;
    let mut legacy = false;
    for path in paths {
        plain(&root, &path)?;
        for suffix in ["-wal", "-shm"] {
            let side = PathBuf::from(format!("{}{suffix}", path.display()));
            if fs::symlink_metadata(&side).is_ok() {
                plain(&root, &side)?;
            }
        }
        let connection = Connection::open_with_flags(
            &path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;
        connection.execute_batch("PRAGMA query_only=ON; BEGIN")?;
        let tables = schemas(&connection)?;
        anyhow::ensure!(
            !unsupported_foreign_keys(&tables),
            "unsupported inbound foreign key relation appeared"
        );
        let database = original
            .databases
            .iter()
            .find(|d| d.path == path)
            .ok_or_else(|| anyhow::anyhow!("unknown candidate database"))?;
        let actual = tables
            .iter()
            .filter(|t| !t.refs.is_empty())
            .map(|t| (t.name.clone(), t.refs.clone()))
            .collect::<BTreeMap<_, _>>();
        let expected = database
            .tables
            .iter()
            .map(|t| (t.name.clone(), t.id_columns.clone()))
            .collect::<BTreeMap<_, _>>();
        anyhow::ensure!(
            actual == expected,
            "associated database reference schema changed"
        );
        for table in tables {
            if table.refs.is_empty() {
                continue;
            }
            anyhow::ensure!(table.ordinary, "unsupported virtual database reference");
            let predicate = table
                .refs
                .iter()
                .map(|c| format!("{}=?1", quote(c)))
                .collect::<Vec<_>>()
                .join(" OR ");
            let exists = |condition: &str| -> Result<bool> {
                let query = format!(
                    "SELECT EXISTS(SELECT 1 FROM {} WHERE ({predicate}) AND ({condition}))",
                    quote(&table.name)
                );
                Ok(connection.query_row(&query, [&original.id], |r| r.get::<_, bool>(0))?)
            };
            if table
                .refs
                .iter()
                .any(|c| matches!(c.as_str(), "parent_thread_id" | "child_thread_id"))
            {
                anyhow::ensure!(!exists("1")?, "spawn or parent/child relation appeared");
            }
            match table.name.as_str() {
                "threads"
                    if path
                        .file_name()
                        .and_then(|s| s.to_str())
                        .is_some_and(|s| s.starts_with("state_")) =>
                {
                    anyhow::ensure!(
                        table.columns.contains("rollout_path"),
                        "unsupported registration schema"
                    );
                    let mut ownership =
                        connection.prepare("SELECT id,rollout_path FROM threads")?;
                    for row in ownership
                        .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?
                    {
                        let (owner, reference) = row?;
                        if owner == original.id {
                            continue;
                        }
                        let reference = PathBuf::from(reference);
                        let reference = if reference.is_absolute() {
                            reference
                        } else {
                            root.join(reference)
                        };
                        let reference = match fs::canonicalize(&reference) {
                            Ok(path) => path,
                            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                            Err(error) => {
                                return Err(error)
                                    .context("unknown registered source ownership identity");
                            }
                        };
                        for candidate in &candidate_identities {
                            #[cfg(any(windows, target_os = "macos"))]
                            let shared = reference
                                .to_string_lossy()
                                .eq_ignore_ascii_case(&candidate.to_string_lossy());
                            #[cfg(not(any(windows, target_os = "macos")))]
                            let shared = reference.as_path() == candidate.as_path();
                            anyhow::ensure!(
                                !shared,
                                "shared source file ownership appeared in thread registration"
                            );
                        }
                    }
                    let mode = if table.columns.contains("history_mode") {
                        quote("history_mode")
                    } else {
                        "'legacy'".into()
                    };
                    let mut statement = connection.prepare(&format!(
                        "SELECT rollout_path,{mode} FROM threads WHERE id=?1"
                    ))?;
                    for row in statement.query_map([&original.id], |r| {
                        Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
                    })? {
                        let (rollout, mode) = row?;
                        registered += 1;
                        legacy = mode == "legacy";
                        let rollout = PathBuf::from(rollout);
                        let rollout = if rollout.is_absolute() {
                            rollout
                        } else {
                            root.join(rollout)
                        };
                        supported_rollout(&root, &rollout)?;
                        fresh.files.push(rollout);
                    }
                }
                "thread_turns" => {
                    anyhow::ensure!(
                        table.columns.contains("status"),
                        "unsupported turn status schema"
                    );
                    anyhow::ensure!(
                        !exists(
                            "status IS NULL OR status NOT IN ('completed','failed','interrupted')"
                        )?,
                        "incomplete or unknown turn status appeared"
                    );
                }
                "queued_items" => anyhow::ensure!(!exists("1")?, "queued work appeared"),
                "thread_goals" => {
                    anyhow::ensure!(
                        table.columns.contains("status"),
                        "unsupported goal status schema"
                    );
                    anyhow::ensure!(
                        !exists(
                            "status IS NULL OR status NOT IN ('complete','paused','blocked','usage_limited','budget_limited')"
                        )?,
                        "active or unknown goal appeared"
                    );
                }
                "thread_items" => {
                    anyhow::ensure!(
                        ["thread_id", "item_type", "created_at_ms", "completed_at_ms"]
                            .iter()
                            .all(|c| table.columns.contains(*c)),
                        "unsupported message history schema"
                    );
                    let (max,min,incomplete):(Option<i64>,Option<i64>,i64)=connection.query_row("SELECT MAX(CASE WHEN item_type='userMessage' THEN created_at_ms ELSE completed_at_ms END), MIN(CASE WHEN item_type='userMessage' THEN created_at_ms ELSE completed_at_ms END), COALESCE(SUM(CASE WHEN (item_type='userMessage' AND created_at_ms IS NULL) OR (item_type='agentMessage' AND completed_at_ms IS NULL) THEN 1 ELSE 0 END),0) FROM thread_items WHERE thread_id=?1 AND item_type IN ('userMessage','agentMessage')",[&original.id],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?)))?;
                    anyhow::ensure!(incomplete == 0, "incomplete message timestamp appeared");
                    if let Some(date) = max {
                        let date = valid_ms(date)?;
                        fresh.last_message_ms =
                            Some(fresh.last_message_ms.map_or(date, |old| old.max(date)));
                    }
                    if let Some(date) = min {
                        valid_ms(date)?;
                    }
                }
                "thread_realtime_items" => {
                    anyhow::ensure!(
                        table.columns.contains("item_type"),
                        "unsupported realtime message schema"
                    );
                    anyhow::ensure!(
                        !exists("item_type IN ('userMessage','agentMessage')")?,
                        "unsupported realtime message history appeared"
                    );
                }
                _ => {}
            }
        }
    }
    anyhow::ensure!(registered == 1, "thread registration removed or duplicated");
    if fresh.last_message_ms.is_none() && legacy {
        fresh.last_message_ms = messages(&fresh.files[0], false)?;
    }
    anyhow::ensure!(
        fresh.last_message_ms.is_some(),
        "unknown conversation timestamp after recheck"
    );
    Ok(fresh)
}
