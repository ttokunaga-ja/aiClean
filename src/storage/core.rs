use crate::{
    model::{Provider, Roots, Session},
    platform,
};
use anyhow::{Context, Result, bail, ensure};
use chrono::{Local, NaiveDate, TimeZone, Utc};
use fs2::FileExt;
use rusqlite::{
    Connection, OpenFlags, params_from_iter,
    types::{Value, ValueRef},
};
use serde::{Deserialize, Serialize};
use serde_json::{Value as Json, json};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeSet,
    fs::{self, File, OpenOptions},
    io::{BufRead, BufReader, BufWriter, Read, Write},
    path::{Component, Path, PathBuf},
    time::Duration,
};
use uuid::Uuid;

const FORMAT: u32 = 1;
const RESERVE: u64 = 8 * 1024 * 1024 * 1024;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
enum Cell {
    Null,
    Integer(i64),
    Real(u64),
    Text(Vec<u8>),
    Blob(Vec<u8>),
}
impl Cell {
    fn read(value: ValueRef<'_>) -> Self {
        match value {
            ValueRef::Null => Self::Null,
            ValueRef::Integer(v) => Self::Integer(v),
            ValueRef::Real(v) => Self::Real(v.to_bits()),
            ValueRef::Text(v) => Self::Text(v.to_vec()),
            ValueRef::Blob(v) => Self::Blob(v.to_vec()),
        }
    }
    fn bind(&self) -> Value {
        match self {
            Self::Null => Value::Null,
            Self::Integer(v) => Value::Integer(*v),
            Self::Real(v) => Value::Real(f64::from_bits(*v)),
            Self::Text(v) | Self::Blob(v) => Value::Blob(v.clone()),
        }
    }
    fn placeholder(&self) -> &'static str {
        if matches!(self, Self::Text(_)) {
            "CAST(? AS TEXT)"
        } else {
            "?"
        }
    }
}
#[derive(Debug, Serialize, Deserialize)]
struct ExactRow {
    rowid: Option<i64>,
    cells: Vec<Cell>,
}
#[derive(Debug, Serialize, Deserialize, PartialEq)]
struct Schema {
    sql: String,
    columns: Vec<String>,
    pk: Vec<usize>,
    rowid_alias: Option<String>,
    objects: Vec<(String, String, String)>,
}
#[derive(Debug, Serialize, Deserialize)]
struct TableSnapshot {
    name: String,
    schema: Schema,
    rows: String,
    sha256: String,
    bytes: u64,
    count: u64,
}
#[derive(Debug, Serialize, Deserialize)]
struct DatabaseSnapshot {
    path: PathBuf,
    tables: Vec<TableSnapshot>,
}
#[derive(Debug, Serialize, Deserialize)]
struct FileSnapshot {
    path: PathBuf,
    copy: String,
    sha256: String,
    bytes: u64,
    mode: u32,
    readonly: bool,
}
#[derive(Debug, Serialize, Deserialize)]
struct Manifest {
    format: u32,
    provider: Provider,
    root: PathBuf,
    session: String,
    files: Vec<FileSnapshot>,
    databases: Vec<DatabaseSnapshot>,
}
#[derive(Debug, Serialize, Deserialize)]
struct Receipt {
    format: u32,
    provider: Provider,
    batch: String,
    unit: String,
    created_ms: i64,
    root: PathBuf,
    bundle: PathBuf,
    trash: Option<PathBuf>,
    manifest_sha256: Option<String>,
    phase: String,
    intent: Option<String>,
    committed_databases: Vec<usize>,
    deleted_files: Vec<usize>,
    original_file_bytes: u64,
    snapshot_bytes: u64,
}
impl Receipt {
    fn finished(&self) -> bool {
        matches!(
            self.phase.as_str(),
            "restored" | "cancelled-before-deletion"
        )
    }
    fn pending(&self) -> bool {
        self.phase != "complete" && !self.finished()
    }
    fn originals_untouched(&self) -> bool {
        matches!(
            self.phase.as_str(),
            "capturing" | "captured" | "trash-intent"
        )
    }
}

// Only unit tests substitute this boundary. Production always uses platform guards
// and native Trash; it has no mock/unsafe-command switch.
trait Effects {
    fn quiet(&self, provider: Provider) -> Result<()>;
    fn verify_candidate(&self, session: &Session, cutoff: i64) -> Result<()>;
    fn trash(&self, path: &Path) -> Result<PathBuf>;
    fn find_trash(&self, path: &Path) -> Result<Option<PathBuf>>;
    fn headroom(&self, path: &Path, bytes: u64) -> Result<()> {
        ensure!(
            fs2::available_space(path)? >= RESERVE.saturating_add(bytes),
            "Insufficient free space: capture requires its size plus an 8 GiB reserve"
        );
        Ok(())
    }
    fn checkpoint(&self, _point: &str) -> Result<()> {
        Ok(())
    }
}
struct Native;
impl Effects for Native {
    fn quiet(&self, provider: Provider) -> Result<()> {
        platform::ensure_quiet(provider)
    }
    fn verify_candidate(&self, session: &Session, cutoff: i64) -> Result<()> {
        crate::providers::verify_candidate(session, cutoff)
    }
    fn trash(&self, path: &Path) -> Result<PathBuf> {
        platform::trash(path)
    }
    fn find_trash(&self, path: &Path) -> Result<Option<PathBuf>> {
        platform::find_trash(path)
    }
}

pub fn run(
    roots: &Roots,
    state: &Path,
    provider: Provider,
    sessions: &[Session],
    month: &str,
    scheduled: bool,
) -> Result<Json> {
    run_with(roots, state, provider, sessions, month, scheduled, &Native)
}
pub fn undo(roots: &Roots, state: &Path, provider: Provider, batch: Option<&str>) -> Result<Json> {
    undo_with(roots, state, provider, batch, &Native)
}

pub fn maintenance_lock(state: &Path) -> Result<File> {
    ensure_safe_absolute(state)?;
    private_dir(state)?;
    let path = state.join("maintenance.lock");
    no_links(&path, true)?;
    let file = private_open(&path, false)?;
    file.try_lock_exclusive()
        .context("Another aiClean maintenance operation is active")?;
    Ok(file)
}

fn run_with(
    roots: &Roots,
    state: &Path,
    provider: Provider,
    sessions: &[Session],
    month: &str,
    scheduled: bool,
    effects: &dyn Effects,
) -> Result<Json> {
    validate_state(roots, state)?;
    let _lock = maintenance_lock(state)?;
    let root = provider_root(roots, provider)?;
    let cutoff = month_cutoff(month)?;
    let mut receipts = load_receipts(state, Provider::Claude)?;
    receipts.extend(load_receipts(state, Provider::Codex)?);
    ensure!(
        !receipts.iter().any(|(_, r)| r.pending()),
        "Interrupted operation requires undo before new cleanup"
    );
    let marker = state.join(format!("month-{}-{month}.json", provider.name()));
    no_links(&marker, true)?;
    if marker.exists() {
        return Ok(
            json!({"provider":provider,"month":month,"already_completed":true,"removed_sessions":0}),
        );
    }
    let stop = state.join(format!("stop-{}", provider.name()));
    let initial_stop = stop_token(&stop)?;
    if scheduled && initial_stop.is_some() {
        let result = json!({"provider":provider,"month":month,"scheduled":true,"stopped":true,"removed_sessions":0});
        atomic_json(
            &state.join(format!("last-{}.json", provider.name())),
            &result,
        )?;
        return Ok(result);
    }
    effects.quiet(provider)?;
    let mut owned_files = BTreeSet::new();
    for session in sessions
        .iter()
        .filter(|s| s.skip_reason.is_none() && s.last_message_ms.is_some_and(|t| t < cutoff))
    {
        validate_session(&root, provider, session)?;
        for path in &session.files {
            ensure!(
                owned_files.insert(path.clone()),
                "Multiple sessions share a source file"
            );
        }
    }
    let batch = Uuid::new_v4().to_string();
    let mut removed = 0u64;
    let mut file_bytes = 0u64;
    let mut snapshot_bytes = 0u64;
    let mut skipped = 0u64;
    let mut stopped = false;
    for session in sessions {
        if new_stop(&stop, &initial_stop)? {
            stopped = true;
            break;
        }
        if session.skip_reason.is_some() || session.last_message_ms.is_none_or(|t| t >= cutoff) {
            skipped += 1;
            continue;
        }
        let receipt = match cleanup_unit(&root, state, provider, session, &batch, cutoff, effects) {
            Ok(receipt) => receipt,
            Err(error) => {
                atomic_json(
                    &state.join(format!("last-{}.json", provider.name())),
                    &json!({"provider":provider,"month":month,"batch":batch,"removed_sessions":removed,"failed":true,"requires_undo":load_receipts(state,provider)?.iter().any(|(_,r)|r.pending())}),
                )?;
                return Err(error);
            }
        };
        removed += 1;
        file_bytes += receipt.original_file_bytes;
        snapshot_bytes += receipt.snapshot_bytes;
    }
    stopped |= new_stop(&stop, &initial_stop)?;
    let result = json!({"provider":provider,"month":month,"batch":batch,"removed_sessions":removed,"skipped_sessions":skipped,"removed_source_file_bytes":file_bytes,"recoverable_bundle_bytes":snapshot_bytes,"sqlite_pages_reusable":true,"physical_reclaimed_bytes":null,"stopped":stopped});
    if !stopped {
        atomic_json(&marker, &result)?;
    }
    atomic_json(
        &state.join(format!("last-{}.json", provider.name())),
        &result,
    )?;
    Ok(result)
}

fn cleanup_unit(
    root: &Path,
    state: &Path,
    provider: Provider,
    session: &Session,
    batch: &str,
    cutoff: i64,
    effects: &dyn Effects,
) -> Result<Receipt> {
    effects.quiet(provider)?;
    validate_session(root, provider, session)?;
    let mut connections = open_connections(session.databases.iter().map(|d| d.path.as_path()))?;
    effects.verify_candidate(session, cutoff)?;
    let estimate = estimate_capture(session, &connections)?;
    effects.headroom(state, estimate)?;
    for file in &session.files {
        effects.headroom(file.parent().context("Source has no parent")?, 0)?;
    }
    for db in &session.databases {
        effects.headroom(db.path.parent().context("Database has no parent")?, 0)?;
    }
    let unit = Uuid::new_v4().to_string();
    let bundles = state.join("bundles");
    private_dir(&bundles)?;
    let bundle = bundles.join(format!("aiClean-{}-{unit}", provider.name()));
    private_dir(&bundle)?;
    let mut receipt = Receipt {
        format: FORMAT,
        provider,
        batch: batch.to_owned(),
        unit,
        created_ms: Utc::now().timestamp_millis(),
        root: root.to_owned(),
        bundle: bundle.clone(),
        trash: None,
        manifest_sha256: None,
        phase: "capturing".into(),
        intent: None,
        committed_databases: vec![],
        deleted_files: vec![],
        original_file_bytes: 0,
        snapshot_bytes: 0,
    };
    let receipt_path = receipt_path(state, &receipt)?;
    atomic_json(&receipt_path, &receipt)?;
    effects.checkpoint("after-capture-intent")?;
    let manifest = capture(root, session, &connections, &bundle, effects)?;
    let manifest_path = bundle.join("manifest.json");
    atomic_json(&manifest_path, &manifest)?;
    receipt.manifest_sha256 = Some(hash_file(&manifest_path)?.0);
    receipt.original_file_bytes = manifest.files.iter().map(|f| f.bytes).sum();
    receipt.snapshot_bytes = directory_bytes(&bundle)?;
    // Make every snapshot immutable to ordinary accidental writes. Hashes are
    // also verified on every replay; readonly is not a security trust boundary.
    freeze_bundle(&bundle)?;
    receipt.phase = "captured".into();
    atomic_json(&receipt_path, &receipt)?;
    effects.checkpoint("after-capture")?;
    effects.quiet(provider)?;
    effects.verify_candidate(session, cutoff)?;
    verify_files(&manifest.files, false)?;
    for (db, connection) in manifest.databases.iter().zip(&connections) {
        validate_database(connection, db)?;
        validate_existing_rows(connection, db, &bundle, false)?;
    }
    receipt.phase = "trash-intent".into();
    receipt.intent = Some("native-trash".into());
    atomic_json(&receipt_path, &receipt)?;
    let trash = effects.trash(&bundle)?;
    effects.checkpoint("after-trash-before-receipt")?;
    receipt.trash = Some(trash.clone());
    receipt.phase = "deleting".into();
    receipt.intent = None;
    atomic_json(&receipt_path, &receipt)?;
    verify_bundle(&receipt, &trash)?;
    effects.quiet(provider)?;
    effects.verify_candidate(session, cutoff)?;
    // Holding BEGIN IMMEDIATE on every WAL database prevents another writer
    // changing a captured row. COMMIT is still separate for every database.
    for (index, (db, connection)) in manifest.databases.iter().zip(&connections).enumerate() {
        validate_database(connection, db)?;
        suppress_triggers(connection, db)?;
        for table in &db.tables {
            stream_rows(&trash.join(&table.rows), |row| {
                ensure!(
                    row_matches(connection, table, &row)? == RowState::Exact,
                    "Captured row changed before deletion"
                );
                let (predicate, bindings) = identity(table, &row)?;
                let affected = connection.execute(
                    &format!("DELETE FROM {} WHERE {predicate}", quoted(&table.name)),
                    params_from_iter(bindings),
                )?;
                ensure!(affected == 1, "Exact deletion did not affect one row");
                Ok(())
            })?;
        }
        effects.checkpoint(&format!("after-delete-before-trigger-restore-{index}"))?;
        recreate_triggers(connection, db)?;
        validate_database(connection, db)?;
        receipt.intent = Some(format!("delete-db-{index}"));
        atomic_json(&receipt_path, &receipt)?;
        effects.quiet(provider)?;
        connection.execute_batch("COMMIT")?;
        effects.checkpoint(&format!("after-delete-db-{index}"))?;
        receipt.committed_databases.push(index);
        receipt.intent = None;
        atomic_json(&receipt_path, &receipt)?;
    }
    connections.clear();
    for (index, file) in manifest.files.iter().enumerate() {
        receipt.intent = Some(format!("delete-file-{index}"));
        atomic_json(&receipt_path, &receipt)?;
        effects.quiet(provider)?;
        validate_source(root, &file.path, false)?;
        verify_file(file)?;
        fs::remove_file(&file.path)?;
        sync_dir(file.path.parent().context("Source file has no parent")?)?;
        effects.checkpoint(&format!("after-delete-file-{index}"))?;
        receipt.deleted_files.push(index);
        receipt.intent = None;
        atomic_json(&receipt_path, &receipt)?;
    }
    receipt.phase = "complete".into();
    atomic_json(&receipt_path, &receipt)?;
    Ok(receipt)
}

fn undo_with(
    roots: &Roots,
    state: &Path,
    provider: Provider,
    batch: Option<&str>,
    effects: &dyn Effects,
) -> Result<Json> {
    validate_state(roots, state)?;
    let _lock = maintenance_lock(state)?;
    let root = provider_root(roots, provider)?;
    let mut receipts = load_receipts(state, provider)?;
    if let Some(batch) = batch {
        ensure!(Uuid::parse_str(batch).is_ok(), "Invalid batch ID");
    }
    let selected = batch.map(str::to_owned).or_else(|| {
        receipts
            .iter()
            .filter(|(_, r)| !r.finished())
            .max_by_key(|(_, r)| (r.created_ms, &r.unit))
            .map(|(_, r)| r.batch.clone())
    });
    let Some(selected) = selected else {
        return Ok(json!({"provider":provider,"restored_sessions":0,"nothing_to_restore":true}));
    };
    ensure!(
        receipts.iter().any(|(_, r)| r.batch == selected),
        "Batch not found for selected provider"
    );
    receipts.retain(|(_, r)| r.batch == selected && !r.finished());
    effects.quiet(provider)?;
    // Preflight the entire selected batch before the first restore write, then
    // repeat under each unit's locks immediately before that unit is restored.
    for (_, receipt) in &receipts {
        ensure!(
            receipt.root == root,
            "Provider root differs from captured root"
        );
        if receipt.originals_untouched() {
            continue;
        }
        let bundle = resolve_bundle(receipt, effects)?
            .context("Recoverable bundle missing; the OS may have purged Trash")?;
        let manifest = verify_bundle(receipt, &bundle)?;
        validate_manifest(&root, provider, &manifest)?;
        let connections = open_connections(manifest.databases.iter().map(|d| d.path.as_path()))?;
        preflight_restore(&manifest, &connections, &bundle)?;
    }
    let mut restored = 0;
    let mut cancelled = 0;
    let mut file_bytes = 0u64;
    for (path, mut receipt) in receipts {
        ensure!(
            receipt.root == root,
            "Provider root differs from captured root"
        );
        // The durable Trash acknowledgement changes the phase to "deleting"
        // BEFORE any source mutation. Pre-ack interruptions can be cancelled
        // even when native Trash cannot be enumerated; originals were untouched.
        if receipt.originals_untouched() {
            receipt.phase = "cancelled-before-deletion".into();
            receipt.intent = None;
            atomic_json(&path, &receipt)?;
            cancelled += 1;
            continue;
        }
        let bundle = resolve_bundle(&receipt, effects)?
            .context("Recoverable bundle missing; the OS may have purged Trash")?;
        let manifest = verify_bundle(&receipt, &bundle)?;
        validate_manifest(&root, provider, &manifest)?;
        let connections = open_connections(manifest.databases.iter().map(|d| d.path.as_path()))?;
        preflight_restore(&manifest, &connections, &bundle)?;
        effects.headroom(state, 0)?;
        for file in &manifest.files {
            let mut parent = file.path.parent().context("Source has no parent")?;
            while !parent.exists() {
                parent = parent
                    .parent()
                    .context("Source parent has no existing ancestor")?;
            }
            effects.headroom(parent, file.bytes)?;
        }
        for db in &manifest.databases {
            effects.headroom(
                db.path.parent().context("Database has no parent")?,
                receipt.original_file_bytes,
            )?;
        }
        receipt.phase = "restoring".into();
        receipt.trash = Some(bundle.clone());
        receipt.intent = None;
        atomic_json(&path, &receipt)?;
        effects.quiet(provider)?;
        for (index, (db, connection)) in manifest.databases.iter().zip(&connections).enumerate() {
            restore_database(connection, db, &bundle)?;
            receipt.intent = Some(format!("restore-db-{index}"));
            atomic_json(&path, &receipt)?;
            effects.quiet(provider)?;
            connection.execute_batch("COMMIT")?;
            effects.checkpoint(&format!("after-restore-db-{index}"))?;
            receipt.intent = None;
            atomic_json(&path, &receipt)?;
        }
        for (index, file) in manifest.files.iter().enumerate() {
            receipt.intent = Some(format!("restore-file-{index}"));
            atomic_json(&path, &receipt)?;
            effects.quiet(provider)?;
            restore_file(&root, file, &bundle, &receipt.unit, index, effects)?;
            effects.checkpoint(&format!("after-restore-file-{index}"))?;
            receipt.intent = None;
            atomic_json(&path, &receipt)?;
        }
        receipt.phase = "restored".into();
        atomic_json(&path, &receipt)?;
        restored += 1;
        file_bytes += receipt.original_file_bytes;
    }
    let result = json!({"provider":provider,"batch":selected,"restored_sessions":restored,"restored_file_bytes":file_bytes,"cancelled_before_deletion":cancelled});
    atomic_json(
        &state.join(format!("last-{}.json", provider.name())),
        &result,
    )?;
    Ok(result)
}

pub fn status(roots: &Roots, state: &Path, provider: Provider) -> Result<Json> {
    status_with(roots, state, provider, &Native)
}
pub fn record_attempt(roots: &Roots, state: &Path, provider: Provider, value: &Json) -> Result<()> {
    validate_state(roots, state)?;
    private_dir(state)?;
    // This diagnostic is independent of durable mutation receipts. It can be
    // written even when another maintenance process holds the global lock.
    atomic_json(
        &state.join(format!("attempt-{}.json", provider.name())),
        value,
    )
}
fn status_with(
    roots: &Roots,
    state: &Path,
    provider: Provider,
    effects: &dyn Effects,
) -> Result<Json> {
    // Status is read-only, does not create the state directory or take an
    // exclusive maintenance lock, and remains usable while cleanup runs.
    validate_state(roots, state)?;
    let receipts = load_receipts(state, provider)?;
    let mut recoverable_bytes = 0;
    let mut recoverable_trash_bytes = 0;
    let mut recoverable_staging_bytes = 0;
    let mut recoverable = 0;
    let mut missing = 0;
    let mut incomplete_capture_bytes = 0;
    let mut pending = Vec::new();
    let mut batches = BTreeSet::new();
    for (_, receipt) in &receipts {
        if receipt.pending() {
            pending.push(json!({"batch":receipt.batch,"unit":receipt.unit,"phase":receipt.phase,"intent":receipt.intent}));
        }
        if receipt.manifest_sha256.is_none() && receipt.bundle.exists() {
            incomplete_capture_bytes += directory_bytes(&receipt.bundle)?;
        }
        if receipt.finished() || receipt.phase == "capturing" {
            continue;
        }
        batches.insert(receipt.batch.clone());
        if let Some(bundle) = status_bundle(receipt, effects)? {
            // Status scans metadata only; multi-GB row/file hashes are checked
            // on undo, where full integrity validation is required.
            if quick_bundle(receipt, &bundle).is_ok() {
                recoverable += 1;
                recoverable_bytes += receipt.snapshot_bytes;
                if bundle == receipt.bundle {
                    recoverable_staging_bytes += receipt.snapshot_bytes;
                } else {
                    recoverable_trash_bytes += receipt.snapshot_bytes;
                }
            } else {
                missing += 1;
            }
        } else {
            missing += 1;
        }
    }
    let last = state.join(format!("last-{}.json", provider.name()));
    no_links(&last, true)?;
    let last_result: Option<Json> = if last.exists() {
        Some(serde_json::from_reader(File::open(last)?)?)
    } else {
        None
    };
    let attempt = state.join(format!("attempt-{}.json", provider.name()));
    no_links(&attempt, true)?;
    let last_attempt: Option<Json> = if attempt.exists() {
        Some(serde_json::from_reader(File::open(attempt)?)?)
    } else {
        None
    };
    Ok(
        json!({"provider":provider,"pending_operations":pending,"recoverable_batches":batches,"recoverable_sessions":recoverable,"recoverable_trash_bytes":recoverable_trash_bytes,"recoverable_staging_bytes":recoverable_staging_bytes,"available_bundle_bytes":recoverable_bytes,"incomplete_capture_bytes":incomplete_capture_bytes,"missing_or_corrupt_bundles":missing,"integrity_checked":false,"integrity_verified_on_undo":true,"location_check":"recorded paths; undo can search native Trash metadata","last_result":last_result,"last_attempt":last_attempt}),
    )
}

fn capture(
    root: &Path,
    session: &Session,
    connections: &[Connection],
    bundle: &Path,
    effects: &dyn Effects,
) -> Result<Manifest> {
    let mut files = Vec::new();
    for (index, path) in session.files.iter().enumerate() {
        validate_source(root, path, false)?;
        let metadata = fs::metadata(path)?;
        let copy = format!("file-{index}");
        let mut input = File::open(path)?;
        let mut output = private_open(&bundle.join(&copy), true)?;
        std::io::copy(&mut input, &mut output)?;
        output.sync_all()?;
        let (sha256, bytes) = hash_file(&bundle.join(&copy))?;
        let (source_hash, source_bytes) = hash_file(path)?;
        ensure!(
            source_hash == sha256 && source_bytes == bytes,
            "Source file changed during capture"
        );
        #[cfg(unix)]
        let mode = {
            use std::os::unix::fs::PermissionsExt;
            metadata.permissions().mode()
        };
        #[cfg(not(unix))]
        let mode = 0;
        files.push(FileSnapshot {
            path: path.clone(),
            copy,
            sha256,
            bytes,
            mode,
            readonly: metadata.permissions().readonly(),
        });
        effects.checkpoint(&format!("after-capture-file-{index}"))?;
    }
    let mut databases = Vec::new();
    for (db_index, (database, connection)) in session.databases.iter().zip(connections).enumerate()
    {
        let mut tables = Vec::new();
        for (table_index, target) in database.tables.iter().enumerate() {
            let schema = table_schema(connection, &target.name)?;
            ensure!(
                !target.id_columns.is_empty()
                    && target.id_columns.iter().all(|c| schema.columns.contains(c)),
                "Unsupported session selector"
            );
            let predicate = target
                .id_columns
                .iter()
                .map(|c| format!("{} = ?", quoted(c)))
                .collect::<Vec<_>>()
                .join(" OR ");
            let sql = format!(
                "SELECT {} FROM {} WHERE {predicate}",
                select_columns(&schema),
                quoted(&target.name)
            );
            let rows_name = format!("rows-{db_index}-{table_index}.jsonl");
            let mut output = BufWriter::new(private_open(&bundle.join(&rows_name), true)?);
            let mut statement = connection.prepare(&sql)?;
            let mut cursor = statement.query(params_from_iter(
                target.id_columns.iter().map(|_| &session.id),
            ))?;
            let mut count = 0;
            while let Some(row) = cursor.next()? {
                let exact = read_row(row, &schema)?;
                serde_json::to_writer(&mut output, &exact)?;
                output.write_all(b"\n")?;
                count += 1;
            }
            output.flush()?;
            output.get_ref().sync_all()?;
            let (sha256, bytes) = hash_file(&bundle.join(&rows_name))?;
            tables.push(TableSnapshot {
                name: target.name.clone(),
                schema,
                rows: rows_name,
                sha256,
                bytes,
                count,
            });
        }
        databases.push(DatabaseSnapshot {
            path: database.path.clone(),
            tables,
        });
    }
    sync_dir(bundle)?;
    Ok(Manifest {
        format: FORMAT,
        provider: session.provider,
        root: root.to_owned(),
        session: session.id.clone(),
        files,
        databases,
    })
}

fn estimate_capture(session: &Session, connections: &[Connection]) -> Result<u64> {
    let mut bytes = 1024u64 * 1024;
    for file in &session.files {
        bytes = bytes.saturating_add(fs::metadata(file)?.len());
    }
    for (database, connection) in session.databases.iter().zip(connections) {
        for table in &database.tables {
            let schema = table_schema(connection, &table.name)?;
            ensure!(
                !table.id_columns.is_empty()
                    && table.id_columns.iter().all(|c| schema.columns.contains(c)),
                "Unsupported session selector"
            );
            let size = schema
                .columns
                .iter()
                .map(|c| format!("coalesce(length(CAST({} AS BLOB)),0)", quoted(c)))
                .collect::<Vec<_>>()
                .join("+");
            let predicate = table
                .id_columns
                .iter()
                .map(|c| format!("{} = ?", quoted(c)))
                .collect::<Vec<_>>()
                .join(" OR ");
            let sql = format!(
                "SELECT coalesce(sum(({size})*5 + {}),0) FROM {} WHERE {predicate}",
                schema.columns.len() * 128 + 128,
                quoted(&table.name)
            );
            let estimate: i64 = connection.query_row(
                &sql,
                params_from_iter(table.id_columns.iter().map(|_| &session.id)),
                |r| r.get(0),
            )?;
            bytes = bytes.saturating_add(estimate.max(0) as u64);
        }
    }
    Ok(bytes)
}

fn table_schema(connection: &Connection, name: &str) -> Result<Schema> {
    let (kind, sql): (String, String) = connection
        .query_row(
            "SELECT type, sql FROM sqlite_schema WHERE name=?",
            [name],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .context("Target table unavailable")?;
    ensure!(
        kind == "table" && !sql.to_uppercase().contains("CREATE VIRTUAL TABLE"),
        "Unsupported table type"
    );
    let mut statement = connection.prepare(&format!("PRAGMA table_xinfo({})", quoted(name)))?;
    let info = statement
        .query_map([], |r| {
            Ok((
                r.get::<_, String>(1)?,
                r.get::<_, i64>(5)?,
                r.get::<_, i64>(6)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    ensure!(
        !info.is_empty() && info.iter().all(|(_, _, hidden)| *hidden == 0),
        "Generated/hidden columns are unsupported"
    );
    let columns: Vec<String> = info.iter().map(|(n, _, _)| n.clone()).collect();
    let mut ordered_pk: Vec<_> = info
        .iter()
        .enumerate()
        .filter(|(_, (_, p, _))| *p > 0)
        .map(|(i, (_, p, _))| (*p, i))
        .collect();
    ordered_pk.sort();
    let pk: Vec<usize> = ordered_pk.into_iter().map(|(_, i)| i).collect();
    let rowid_alias = if sql.to_uppercase().contains("WITHOUT ROWID") {
        None
    } else {
        ["rowid", "_rowid_", "oid"]
            .into_iter()
            .find(|alias| !columns.iter().any(|c| c.eq_ignore_ascii_case(alias)))
            .map(str::to_owned)
    };
    ensure!(
        rowid_alias.is_some() || !pk.is_empty(),
        "Table has no accessible stable row identity"
    );
    let mut statement = connection.prepare("SELECT type,name,coalesce(sql,'') FROM sqlite_schema WHERE tbl_name=? AND type IN ('index','trigger') ORDER BY type,name")?;
    let objects = statement
        .query_map([name], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
        .collect::<rusqlite::Result<Vec<(String, String, String)>>>()?;
    Ok(Schema {
        sql,
        columns,
        pk,
        rowid_alias,
        objects,
    })
}

fn open_connections<'a>(paths: impl Iterator<Item = &'a Path>) -> Result<Vec<Connection>> {
    let mut connections = Vec::new();
    let mut seen = BTreeSet::new();
    for path in paths {
        ensure!(seen.insert(path.to_owned()), "Duplicate database path");
        no_links(path, false)?;
        platform::ensure_single_link(path)?;
        for suffix in ["-wal", "-shm", "-journal"] {
            let sidecar = PathBuf::from(format!("{}{suffix}", path.to_string_lossy()));
            no_links(&sidecar, true)?;
            if sidecar.exists() {
                platform::ensure_single_link(&sidecar)?;
            }
        }
        let connection = Connection::open_with_flags(
            path,
            OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;
        connection.busy_timeout(Duration::from_millis(500))?;
        // Offline exact row edits must not activate descendant cascade paths.
        connection
            .execute_batch("PRAGMA foreign_keys=OFF; PRAGMA synchronous=FULL; BEGIN IMMEDIATE")?;
        connections.push(connection);
    }
    Ok(connections)
}
fn validate_database(connection: &Connection, snapshot: &DatabaseSnapshot) -> Result<()> {
    for table in &snapshot.tables {
        ensure!(
            table_schema(connection, &table.name)? == table.schema,
            "Database schema changed; refusing replay"
        );
    }
    Ok(())
}
fn preflight_restore(manifest: &Manifest, connections: &[Connection], bundle: &Path) -> Result<()> {
    for (database, connection) in manifest.databases.iter().zip(connections) {
        validate_database(connection, database)?;
        validate_existing_rows(connection, database, bundle, true)?;
        connection.execute_batch("SAVEPOINT restore_preflight")?;
        let check = restore_database(connection, database, bundle);
        connection.execute_batch("ROLLBACK TO restore_preflight; RELEASE restore_preflight")?;
        check?;
    }
    verify_files(&manifest.files, true)
}
fn validate_existing_rows(
    connection: &Connection,
    db: &DatabaseSnapshot,
    bundle: &Path,
    allow_missing: bool,
) -> Result<()> {
    for table in &db.tables {
        stream_rows(&bundle.join(&table.rows), |row| {
            let found = row_matches(connection, table, &row)?;
            ensure!(
                found == RowState::Exact || (allow_missing && found == RowState::Absent),
                "Existing database row conflicts with snapshot"
            );
            Ok(())
        })?;
    }
    Ok(())
}
#[derive(PartialEq)]
enum RowState {
    Absent,
    Exact,
    Conflict,
}
fn row_matches(connection: &Connection, table: &TableSnapshot, row: &ExactRow) -> Result<RowState> {
    let (predicate, bindings) = identity(table, row)?;
    let mut statement = connection.prepare(&format!(
        "SELECT {} FROM {} WHERE {predicate}",
        select_columns(&table.schema),
        quoted(&table.name)
    ))?;
    let mut cursor = statement.query(params_from_iter(bindings))?;
    let Some(found) = cursor.next()? else {
        return Ok(RowState::Absent);
    };
    let exact = read_row(found, &table.schema)?;
    if exact.rowid != row.rowid || exact.cells != row.cells || cursor.next()?.is_some() {
        Ok(RowState::Conflict)
    } else {
        Ok(RowState::Exact)
    }
}
fn identity(table: &TableSnapshot, row: &ExactRow) -> Result<(String, Vec<Value>)> {
    ensure!(
        row.cells.len() == table.schema.columns.len(),
        "Invalid snapshot row width"
    );
    if let Some(alias) = &table.schema.rowid_alias {
        return Ok((
            format!("{} = ?", quoted(alias)),
            vec![Value::Integer(
                row.rowid.context("Snapshot row identity missing")?,
            )],
        ));
    }
    ensure!(
        row.rowid.is_none() && !table.schema.pk.is_empty(),
        "Invalid primary key identity"
    );
    let predicate = table
        .schema
        .pk
        .iter()
        .map(|&i| {
            format!(
                "{} IS {}",
                quoted(&table.schema.columns[i]),
                row.cells[i].placeholder()
            )
        })
        .collect::<Vec<_>>()
        .join(" AND ");
    let values = table
        .schema
        .pk
        .iter()
        .map(|&i| row.cells[i].bind())
        .collect();
    Ok((predicate, values))
}
fn read_row(row: &rusqlite::Row<'_>, schema: &Schema) -> Result<ExactRow> {
    let offset = usize::from(schema.rowid_alias.is_some());
    let rowid = if offset == 1 { Some(row.get(0)?) } else { None };
    let mut cells = Vec::with_capacity(schema.columns.len());
    for index in 0..schema.columns.len() {
        cells.push(Cell::read(row.get_ref(index + offset)?));
    }
    Ok(ExactRow { rowid, cells })
}
fn select_columns(schema: &Schema) -> String {
    schema
        .rowid_alias
        .iter()
        .chain(schema.columns.iter())
        .map(|c| quoted(c))
        .collect::<Vec<_>>()
        .join(",")
}
fn restore_database(
    connection: &Connection,
    database: &DatabaseSnapshot,
    bundle: &Path,
) -> Result<()> {
    suppress_triggers(connection, database)?;
    for table in &database.tables {
        stream_rows(&bundle.join(&table.rows), |row| {
            match row_matches(connection, table, &row)? {
                RowState::Exact => return Ok(()),
                RowState::Conflict => bail!("Existing row conflicts with snapshot"),
                RowState::Absent => {}
            }
            let mut columns = table.schema.columns.clone();
            let mut placeholders = row.cells.iter().map(Cell::placeholder).collect::<Vec<_>>();
            let mut bindings: Vec<Value> = row.cells.iter().map(Cell::bind).collect();
            if let Some(alias) = &table.schema.rowid_alias {
                columns.insert(0, alias.clone());
                placeholders.insert(0, "?");
                bindings.insert(0, Value::Integer(row.rowid.context("Missing rowid")?));
            }
            let sql = format!(
                "INSERT INTO {} ({}) VALUES ({})",
                quoted(&table.name),
                columns
                    .iter()
                    .map(|c| quoted(c))
                    .collect::<Vec<_>>()
                    .join(","),
                placeholders.join(",")
            );
            connection
                .execute(&sql, params_from_iter(bindings))
                .context("Restore row violates current database constraints")?;
            ensure!(
                row_matches(connection, table, &row)? == RowState::Exact,
                "SQLite converted a restored value; refusing lossy replay"
            );
            Ok(())
        })?;
    }
    recreate_triggers(connection, database)?;
    validate_database(connection, database)?;
    Ok(())
}
fn suppress_triggers(connection: &Connection, database: &DatabaseSnapshot) -> Result<()> {
    for table in &database.tables {
        for (kind, name, _) in &table.schema.objects {
            if kind == "trigger" {
                connection.execute_batch(&format!("DROP TRIGGER {}", quoted(name)))?;
            }
        }
    }
    Ok(())
}
fn recreate_triggers(connection: &Connection, database: &DatabaseSnapshot) -> Result<()> {
    for table in &database.tables {
        for (kind, _, sql) in &table.schema.objects {
            if kind == "trigger" {
                ensure!(!sql.is_empty(), "Trigger definition missing");
                connection.execute_batch(sql)?;
            }
        }
    }
    Ok(())
}
fn stream_rows(path: &Path, mut apply: impl FnMut(ExactRow) -> Result<()>) -> Result<()> {
    no_links(path, false)?;
    let mut input = BufReader::new(File::open(path)?);
    let mut line = String::new();
    loop {
        line.clear();
        if input.read_line(&mut line)? == 0 {
            break;
        }
        ensure!(line.ends_with('\n'), "Truncated snapshot JSONL");
        apply(serde_json::from_str(&line)?)?;
    }
    Ok(())
}

fn restore_file(
    root: &Path,
    snapshot: &FileSnapshot,
    bundle: &Path,
    unit: &str,
    index: usize,
    effects: &dyn Effects,
) -> Result<()> {
    validate_source(root, &snapshot.path, true)?;
    let parent = snapshot.path.parent().context("Source has no parent")?;
    let temporary = parent.join(format!(".aiClean-restore-{unit}-{index}"));
    no_links(&temporary, true)?;
    if snapshot.path.exists() {
        verify_file(snapshot)?;
        if temporary.exists() {
            ensure!(
                fs::metadata(&temporary)?.is_file(),
                "Restore staging path is not a file"
            );
            discard_restore_stage(&temporary)?;
            sync_dir(parent)?;
        }
        return Ok(());
    }
    create_source_parents(root, parent)?;
    // This filename is uniquely owned by the receipt. A interrupted partial
    // temporary copy can be discarded; existing destination files never are.
    if temporary.exists() {
        ensure!(
            fs::metadata(&temporary)?.is_file(),
            "Restore staging path is not a file"
        );
        discard_restore_stage(&temporary)?;
        sync_dir(parent)?;
    }
    let mut output = private_open(&temporary, true)?;
    let mut input = File::open(bundle.join(&snapshot.copy))?;
    std::io::copy(&mut input, &mut output)?;
    output.sync_all()?;
    let (hash, bytes) = hash_file(&temporary)?;
    ensure!(
        hash == snapshot.sha256 && bytes == snapshot.bytes,
        "Restore copy checksum mismatch"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        output.set_permissions(fs::Permissions::from_mode(snapshot.mode))?;
    }
    #[cfg(windows)]
    {
        // Publish content and its readonly attribute together. Retain write
        // access through the attribute change so FlushFileBuffers can succeed.
        let mut permissions = output.metadata()?.permissions();
        permissions.set_readonly(snapshot.readonly);
        output.set_permissions(permissions)?;
    }
    output.sync_all()?;
    effects.checkpoint(&format!("after-restore-stage-{index}"))?;
    drop(output);
    // Native exclusive rename publishes only when the destination is absent;
    // it also avoids leaving temporary hardlink aliases after an interruption.
    match platform::exclusive_restore_file(&temporary, &snapshot.path) {
        Ok(()) => {}
        Err(error)
            if error
                .downcast_ref::<std::io::Error>()
                .is_some_and(|e| e.kind() == std::io::ErrorKind::AlreadyExists) =>
        {
            verify_file(snapshot)?;
        }
        Err(error) => return Err(error).context("Exclusive file restore failed"),
    }
    sync_dir(parent)?;
    effects.checkpoint(&format!("after-restore-publish-{index}"))?;
    if temporary.exists() {
        discard_restore_stage(&temporary)?;
    }
    sync_dir(parent)?;
    Ok(())
}
fn discard_restore_stage(path: &Path) -> Result<()> {
    no_links(path, false)?;
    platform::ensure_single_link(path)?;
    ensure!(
        fs::metadata(path)?.is_file(),
        "Restore staging path is not a file"
    );
    #[cfg(windows)]
    {
        let mut permissions = fs::metadata(path)?.permissions();
        if permissions.readonly() {
            // Windows-only: permit removal of this receipt's uniquely owned
            // disposable stage, never change the destination file's attributes.
            #[allow(clippy::permissions_set_readonly_false)]
            permissions.set_readonly(false);
            fs::set_permissions(path, permissions)?;
            OpenOptions::new()
                .read(true)
                .write(true)
                .open(path)?
                .sync_all()?;
        }
    }
    fs::remove_file(path)?;
    Ok(())
}
fn verify_file(snapshot: &FileSnapshot) -> Result<()> {
    no_links(&snapshot.path, false)?;
    let (hash, bytes) = hash_file(&snapshot.path)?;
    ensure!(
        hash == snapshot.sha256 && bytes == snapshot.bytes,
        "Existing source file conflicts with snapshot"
    );
    Ok(())
}
fn verify_files(files: &[FileSnapshot], allow_missing: bool) -> Result<()> {
    for file in files {
        no_links(&file.path, allow_missing)?;
        if file.path.exists() {
            verify_file(file)?;
        } else {
            ensure!(allow_missing, "Source file disappeared");
        }
    }
    Ok(())
}
fn verify_bundle(receipt: &Receipt, bundle: &Path) -> Result<Manifest> {
    no_links(bundle, false)?;
    let manifest_path = bundle.join("manifest.json");
    let (hash, _) = hash_file(&manifest_path)?;
    ensure!(
        Some(&hash) == receipt.manifest_sha256.as_ref(),
        "Immutable manifest checksum mismatch"
    );
    let manifest: Manifest = serde_json::from_reader(File::open(manifest_path)?)?;
    ensure!(
        manifest.format == FORMAT
            && manifest.provider == receipt.provider
            && manifest.root == receipt.root,
        "Snapshot does not match receipt"
    );
    for file in &manifest.files {
        safe_bundle_name(&file.copy)?;
        let (hash, bytes) = hash_file(&bundle.join(&file.copy))?;
        ensure!(
            hash == file.sha256 && bytes == file.bytes,
            "Recoverable file checksum mismatch"
        );
    }
    for database in &manifest.databases {
        for table in &database.tables {
            safe_bundle_name(&table.rows)?;
            let (hash, bytes) = hash_file(&bundle.join(&table.rows))?;
            ensure!(
                hash == table.sha256 && bytes == table.bytes,
                "Recoverable rows checksum mismatch"
            );
            let mut count = 0;
            stream_rows(&bundle.join(&table.rows), |row| {
                identity(table, &row)?;
                count += 1;
                Ok(())
            })?;
            ensure!(count == table.count, "Snapshot row count mismatch");
        }
    }
    Ok(manifest)
}
fn quick_bundle(receipt: &Receipt, bundle: &Path) -> Result<()> {
    no_links(bundle, false)?;
    let path = bundle.join("manifest.json");
    let (hash, _) = hash_file(&path)?;
    ensure!(
        Some(&hash) == receipt.manifest_sha256.as_ref(),
        "Manifest checksum mismatch"
    );
    let manifest: Manifest = serde_json::from_reader(File::open(&path)?)?;
    ensure!(
        manifest.format == FORMAT
            && manifest.provider == receipt.provider
            && manifest.root == receipt.root,
        "Manifest identity mismatch"
    );
    let check_member = |name: &str, bytes: u64| -> Result<()> {
        safe_bundle_name(name)?;
        let member = bundle.join(name);
        no_links(&member, false)?;
        platform::ensure_single_link(&member)?;
        let metadata = fs::metadata(member)?;
        ensure!(
            metadata.is_file() && metadata.len() == bytes,
            "Incomplete bundle member"
        );
        Ok(())
    };
    for file in &manifest.files {
        check_member(&file.copy, file.bytes)?;
    }
    for database in &manifest.databases {
        for table in &database.tables {
            check_member(&table.rows, table.bytes)?;
        }
    }
    Ok(())
}
fn resolve_bundle(receipt: &Receipt, effects: &dyn Effects) -> Result<Option<PathBuf>> {
    if let Some(trash) = &receipt.trash {
        no_links(trash, true)?;
        if trash.exists() {
            return Ok(Some(trash.clone()));
        }
    }
    no_links(&receipt.bundle, true)?;
    if receipt.bundle.exists() {
        return Ok(Some(receipt.bundle.clone()));
    }
    effects.find_trash(&receipt.bundle)
}
fn status_bundle(receipt: &Receipt, effects: &dyn Effects) -> Result<Option<PathBuf>> {
    if let Some(trash) = &receipt.trash {
        no_links(trash, true)?;
        // Avoid re-enumerating Trash for every historical, OS-purged receipt.
        return Ok(trash.exists().then(|| trash.clone()));
    }
    no_links(&receipt.bundle, true)?;
    if receipt.bundle.exists() {
        return Ok(Some(receipt.bundle.clone()));
    }
    // Pre-ack operations have untouched originals and may not be enumerable.
    effects.find_trash(&receipt.bundle).or(Ok(None))
}
fn safe_bundle_name(name: &str) -> Result<()> {
    ensure!(
        Path::new(name).components().count() == 1
            && matches!(
                Path::new(name).components().next(),
                Some(Component::Normal(_))
            ),
        "Unsafe snapshot member name"
    );
    Ok(())
}
fn validate_manifest(root: &Path, provider: Provider, manifest: &Manifest) -> Result<()> {
    ensure!(
        manifest.root == root && manifest.provider == provider,
        "Snapshot provider/root mismatch"
    );
    for file in &manifest.files {
        validate_source(root, &file.path, true)?;
    }
    for database in &manifest.databases {
        validate_source(root, &database.path, false)?;
    }
    Ok(())
}
fn validate_session(root: &Path, provider: Provider, session: &Session) -> Result<()> {
    ensure!(
        session.provider == provider && fs::canonicalize(&session.root)? == root,
        "Session outside selected provider root"
    );
    ensure!(!session.id.is_empty(), "Empty session identity");
    let mut seen = BTreeSet::new();
    for file in &session.files {
        validate_source(root, file, false)?;
        ensure!(seen.insert(file.clone()), "Duplicate session file");
    }
    for database in &session.databases {
        validate_source(root, &database.path, false)?;
        ensure!(
            seen.insert(database.path.clone()),
            "Duplicate or overlapping database path"
        );
        for suffix in ["-wal", "-shm", "-journal"] {
            let sidecar = PathBuf::from(format!("{}{suffix}", database.path.to_string_lossy()));
            no_links(&sidecar, true)?;
        }
        let mut tables = BTreeSet::new();
        for table in &database.tables {
            ensure!(tables.insert(&table.name), "Duplicate database table");
        }
    }
    ensure!(
        !session.files.is_empty() || !session.databases.is_empty(),
        "Empty cleanup unit"
    );
    Ok(())
}
fn provider_root(roots: &Roots, provider: Provider) -> Result<PathBuf> {
    let path = roots.get(provider);
    ensure_safe_absolute(path)?;
    no_links(path, false)?;
    ensure!(
        fs::metadata(path)?.is_dir(),
        "Provider root is not a directory"
    );
    fs::canonicalize(path).context("Provider root unavailable")
}
fn validate_source(root: &Path, path: &Path, allow_missing: bool) -> Result<()> {
    ensure_safe_absolute(path)?;
    no_links(path, allow_missing)?;
    // Canonicalize only the configured root, preserving macOS OS aliases such
    // as /var -> /private/var. All components under that root are checked.
    let source = canonical_existing_prefix(path)?;
    ensure!(
        source.starts_with(root) && source != root,
        "Source path outside provider root"
    );
    no_links_below(root, &source, allow_missing)?;
    if source.exists() {
        ensure!(
            fs::metadata(&source)?.is_file(),
            "Source is not a regular file"
        );
        platform::ensure_single_link(&source)?;
    }
    Ok(())
}
fn ensure_safe_absolute(path: &Path) -> Result<()> {
    ensure!(
        path.is_absolute()
            && !path
                .components()
                .any(|c| matches!(c, Component::ParentDir | Component::CurDir)),
        "Expected absolute path without traversal"
    );
    #[cfg(windows)]
    ensure!(
        !path
            .components()
            .any(|c| matches!(c,Component::Normal(name) if name.to_string_lossy().contains(':'))),
        "Windows alternate data streams are unsupported"
    );
    Ok(())
}
fn canonical_existing_prefix(path: &Path) -> Result<PathBuf> {
    let mut existing = path;
    let mut suffix = Vec::new();
    while !existing.exists() {
        // exists() follows links, so detect dangling links before ascending.
        if fs::symlink_metadata(existing).is_ok() {
            bail!("Dangling or inaccessible source path");
        }
        suffix.push(
            existing
                .file_name()
                .context("Path has no existing ancestor")?
                .to_owned(),
        );
        existing = existing.parent().context("Path has no existing ancestor")?;
    }
    let mut canonical = fs::canonicalize(existing)?;
    for component in suffix.into_iter().rev() {
        canonical.push(component);
    }
    Ok(canonical)
}
fn no_links(path: &Path, allow_missing: bool) -> Result<()> {
    ensure_safe_absolute(path)?;
    let mut current = PathBuf::new();
    for component in path.components() {
        current.push(component);
        // A Windows drive prefix such as \\?\C: is not an absolute filesystem
        // root until its following RootDir is pushed. Query every real root
        // and descendant as before, without probing that incomplete prefix.
        if matches!(component, Component::Prefix(_)) {
            continue;
        }
        match fs::symlink_metadata(&current) {
            Ok(metadata) => {
                // macOS's fixed system aliases are above user data roots.
                #[cfg(target_os = "macos")]
                if metadata.file_type().is_symlink()
                    && [Path::new("/var"), Path::new("/tmp"), Path::new("/etc")]
                        .contains(&current.as_path())
                {
                    ensure!(
                        fs::read_link(&current)?
                            == Path::new("private")
                                .join(current.file_name().context("System alias name")?),
                        "Unexpected system alias"
                    );
                    continue;
                }
                reject_link(&metadata)?;
            }
            Err(error) if allow_missing && error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(());
            }
            Err(error) => return Err(error.into()),
        }
    }
    Ok(())
}
fn no_links_below(root: &Path, path: &Path, allow_missing: bool) -> Result<()> {
    let relative = path.strip_prefix(root).context("Path outside safe root")?;
    reject_link(&fs::symlink_metadata(root)?)?;
    let mut current = root.to_owned();
    for component in relative.components() {
        ensure!(
            matches!(component, Component::Normal(_)),
            "Unsafe path component"
        );
        current.push(component);
        no_links(&current, allow_missing)?;
    }
    Ok(())
}
fn reject_link(metadata: &fs::Metadata) -> Result<()> {
    ensure!(
        !metadata.file_type().is_symlink(),
        "Symbolic links are unsupported"
    );
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        ensure!(
            metadata.file_attributes() & 0x400 == 0,
            "Windows reparse points are unsupported"
        );
    }
    Ok(())
}
fn validate_state(roots: &Roots, state: &Path) -> Result<()> {
    ensure_safe_absolute(state)?;
    no_links(state, true)?;
    let canonical_state = canonical_existing_prefix(state)?;
    for root in [&roots.codex, &roots.claude] {
        let canonical_root = canonical_existing_prefix(root)?;
        ensure!(
            !canonical_state.starts_with(&canonical_root)
                && !canonical_root.starts_with(&canonical_state),
            "Receipt state must be independent of provider data"
        );
    }
    Ok(())
}
fn create_source_parents(root: &Path, parent: &Path) -> Result<()> {
    let canonical = canonical_existing_prefix(parent)?;
    no_links_below(root, &canonical, true)?;
    let mut current = root.to_owned();
    for component in canonical.strip_prefix(root)?.components() {
        current.push(component);
        if !current.exists() {
            private_dir(&current)?;
        }
        no_links(&current, false)?;
    }
    Ok(())
}
fn private_dir(path: &Path) -> Result<()> {
    no_links(path, true)?;
    let mut builder = fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(path)?;
    ensure!(fs::metadata(path)?.is_dir(), "Expected directory");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let permissions = fs::metadata(path)?.permissions();
        if permissions.mode() & 0o777 != 0o700 {
            fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
        }
    }
    #[cfg(windows)]
    platform::ensure_private_directory(path)?;
    sync_dir(path)?;
    if let Some(parent) = path.parent() {
        sync_dir(parent)?;
    }
    Ok(())
}
fn private_open(path: &Path, exclusive: bool) -> Result<File> {
    no_links(path, true)?;
    let mut options = OpenOptions::new();
    options.read(true).write(true);
    if exclusive {
        options.create_new(true);
    } else {
        options.create(true).truncate(false);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path).context("Cannot open private state file")
}
fn atomic_json(path: &Path, value: &impl Serialize) -> Result<()> {
    let parent = path.parent().context("State file has no parent")?;
    no_links(path, true)?;
    let temporary = parent.join(format!(".receipt-{}.tmp", Uuid::new_v4()));
    let mut output = private_open(&temporary, true)?;
    serde_json::to_writer(&mut output, value)?;
    output.write_all(b"\n")?;
    output.sync_all()?;
    platform::durable_replace(&temporary, path)?;
    Ok(())
}
fn sync_dir(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        File::open(path)?.sync_all()?;
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        // FILE_FLAG_BACKUP_SEMANTICS permits opening a directory handle.
        let directory = OpenOptions::new()
            .read(true)
            .custom_flags(0x02000000)
            .open(path)?;
        // Windows does not support FlushFileBuffers on directory handles.
        // File data is flushed before atomic rename; native Trash provides its
        // own durable native operation and the original-path recovery metadata.
        drop(directory);
    }
    Ok(())
}
fn hash_file(path: &Path) -> Result<(String, u64)> {
    no_links(path, false)?;
    platform::ensure_single_link(path)?;
    let mut input = File::open(path)?;
    ensure!(
        input.metadata()?.is_file(),
        "Expected regular snapshot file"
    );
    let mut hasher = Sha256::new();
    let mut buffer = [0u8; 64 * 1024];
    let mut size = 0u64;
    loop {
        let count = input.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        hasher.update(&buffer[..count]);
        size += count as u64;
    }
    Ok((format!("{:x}", hasher.finalize()), size))
}
fn freeze_bundle(bundle: &Path) -> Result<()> {
    for entry in fs::read_dir(bundle)? {
        let path = entry?.path();
        no_links(&path, false)?;
        platform::ensure_single_link(&path)?;
        // Windows flush requires write access. Acquire it while the captured
        // private copy is writable, then retain it through the readonly change.
        let captured = OpenOptions::new().read(true).write(true).open(&path)?;
        ensure!(captured.metadata()?.is_file(), "Unexpected bundle member");
        let mut permissions = captured.metadata()?.permissions();
        permissions.set_readonly(true);
        captured.set_permissions(permissions)?;
        captured.sync_all()?;
    }
    sync_dir(bundle)
}
fn directory_bytes(bundle: &Path) -> Result<u64> {
    let mut size = 0u64;
    for entry in fs::read_dir(bundle)? {
        let path = entry?.path();
        no_links(&path, false)?;
        ensure!(fs::metadata(&path)?.is_file(), "Unexpected bundle member");
        size += fs::metadata(path)?.len();
    }
    Ok(size)
}
fn receipt_path(state: &Path, receipt: &Receipt) -> Result<PathBuf> {
    let directory = state.join("operations").join(receipt.provider.name());
    private_dir(&directory)?;
    Ok(directory.join(format!("{}.json", receipt.unit)))
}
fn load_receipts(state: &Path, provider: Provider) -> Result<Vec<(PathBuf, Receipt)>> {
    let directory = state.join("operations").join(provider.name());
    no_links(&state.join("operations"), true)?;
    no_links(&directory, true)?;
    if !directory.exists() {
        return Ok(vec![]);
    }
    let mut receipts = Vec::new();
    for entry in fs::read_dir(&directory)? {
        let path = entry?.path();
        if path.extension().is_none_or(|e| e != "json") {
            continue;
        }
        no_links(&path, false)?;
        let receipt: Receipt = serde_json::from_reader(File::open(&path)?)?;
        ensure!(
            receipt.format == FORMAT
                && receipt.provider == provider
                && Uuid::parse_str(&receipt.unit).is_ok()
                && Uuid::parse_str(&receipt.batch).is_ok(),
            "Invalid operation receipt"
        );
        ensure!(
            path.file_stem().is_some_and(|n| n == receipt.unit.as_str()),
            "Receipt identity mismatch"
        );
        ensure!(
            matches!(
                receipt.phase.as_str(),
                "capturing"
                    | "captured"
                    | "trash-intent"
                    | "deleting"
                    | "complete"
                    | "restoring"
                    | "restored"
                    | "cancelled-before-deletion"
            ),
            "Unknown receipt phase"
        );
        let expected =
            state
                .join("bundles")
                .join(format!("aiClean-{}-{}", provider.name(), receipt.unit));
        ensure!(
            receipt.bundle == expected,
            "Receipt bundle outside private state"
        );
        receipts.push((path, receipt));
    }
    receipts.sort_by(|(_, a), (_, b)| (a.created_ms, &a.unit).cmp(&(b.created_ms, &b.unit)));
    Ok(receipts)
}
fn month_cutoff(month: &str) -> Result<i64> {
    let date =
        NaiveDate::parse_from_str(&format!("{month}-01"), "%Y-%m-%d").context("Invalid month")?;
    ensure!(
        date.format("%Y-%m").to_string() == month,
        "Invalid month format"
    );
    Local
        .from_local_datetime(&date.and_hms_opt(0, 0, 0).context("Invalid month start")?)
        .single()
        .map(|t| t.timestamp_millis())
        .context("Month start is ambiguous in local timezone")
}
fn stop_token(path: &Path) -> Result<Option<Vec<u8>>> {
    no_links(path, true)?;
    if !path.exists() {
        return Ok(None);
    }
    let file = File::open(path)?;
    ensure!(
        file.metadata()?.is_file() && file.metadata()?.len() <= 4096,
        "Invalid stop token"
    );
    Ok(Some(fs::read(path)?))
}
fn new_stop(path: &Path, initial: &Option<Vec<u8>>) -> Result<bool> {
    let token = stop_token(path)?;
    Ok(token.is_some() && &token != initial)
}
fn quoted(identifier: &str) -> String {
    format!("\"{}\"", identifier.replace('"', "\"\""))
}

#[cfg(test)]
pub(super) use tests::{run_test, undo_test};

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    struct Fake {
        trash: PathBuf,
        fail: Option<String>,
        seen: Mutex<Vec<String>>,
    }
    impl Fake {
        fn new(state: &Path, fail: Option<&str>) -> Self {
            Self {
                trash: state.parent().unwrap().join("fake-trash"),
                fail: fail.map(str::to_owned),
                seen: Mutex::new(vec![]),
            }
        }
    }
    impl Effects for Fake {
        fn quiet(&self, _: Provider) -> Result<()> {
            Ok(())
        }
        fn verify_candidate(&self, _: &Session, _: i64) -> Result<()> {
            Ok(())
        }
        fn headroom(&self, _: &Path, _: u64) -> Result<()> {
            Ok(())
        }
        fn trash(&self, path: &Path) -> Result<PathBuf> {
            private_dir(&self.trash)?;
            let destination = self.trash.join(path.file_name().unwrap());
            fs::rename(path, &destination)?;
            sync_dir(&self.trash)?;
            Ok(destination)
        }
        fn find_trash(&self, path: &Path) -> Result<Option<PathBuf>> {
            let path = self.trash.join(path.file_name().unwrap());
            Ok(path.exists().then_some(path))
        }
        fn checkpoint(&self, point: &str) -> Result<()> {
            self.seen.lock().unwrap().push(point.into());
            ensure!(
                self.fail.as_deref() != Some(point),
                "Injected interruption at {point}"
            );
            Ok(())
        }
    }
    pub(crate) fn run_test(
        roots: &Roots,
        state: &Path,
        provider: Provider,
        sessions: &[Session],
        month: &str,
    ) -> Result<Json> {
        run_with(
            roots,
            state,
            provider,
            sessions,
            month,
            false,
            &Fake::new(state, None),
        )
    }
    pub(crate) fn undo_test(
        roots: &Roots,
        state: &Path,
        provider: Provider,
        batch: Option<&str>,
    ) -> Result<Json> {
        undo_with(roots, state, provider, batch, &Fake::new(state, None))
    }

    struct Fixture {
        _temp: tempfile::TempDir,
        roots: Roots,
        state: PathBuf,
        session: Session,
    }
    impl Fixture {
        fn new() -> Self {
            use crate::model::{DatabaseRef, TableRef};
            let temp = tempfile::tempdir().unwrap();
            let base = temp.path().canonicalize().unwrap();
            let root = base.join("codex");
            fs::create_dir(&root).unwrap();
            let roots = Roots {
                codex: root.clone(),
                claude: base.join("claude"),
            };
            let files = vec![root.join("one.jsonl"), root.join("two.jsonl")];
            for file in &files {
                fs::write(file, b"private original\n").unwrap();
            }
            let mut databases = vec![];
            for name in ["state.sqlite", "logs.sqlite"] {
                let path = root.join(name);
                let connection = Connection::open(&path).unwrap();
                connection.execute_batch("PRAGMA journal_mode=WAL; CREATE TABLE events(session_id TEXT,payload,number); INSERT INTO events VALUES('s',X'00ff',NULL),('s',X'00ff',NULL),('s',CAST(X'ff00' AS TEXT),-4),('unrelated',123,2.5);").unwrap();
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
                root,
                id: "s".into(),
                last_message_ms: Some(1),
                files,
                databases,
                skip_reason: None,
            };
            Self {
                _temp: temp,
                roots,
                state: base.join("state"),
                session,
            }
        }
        fn run(&self, fail: Option<&str>) -> Result<Json> {
            run_with(
                &self.roots,
                &self.state,
                Provider::Codex,
                std::slice::from_ref(&self.session),
                "2026-10",
                false,
                &Fake::new(&self.state, fail),
            )
        }
        fn undo(&self, fail: Option<&str>) -> Result<Json> {
            undo_with(
                &self.roots,
                &self.state,
                Provider::Codex,
                None,
                &Fake::new(&self.state, fail),
            )
        }
        fn count(&self, index: usize) -> i64 {
            Connection::open(&self.session.databases[index].path)
                .unwrap()
                .query_row(
                    "SELECT count(*) FROM events WHERE session_id='s'",
                    [],
                    |r| r.get(0),
                )
                .unwrap()
        }
        fn assert_original(&self) {
            for file in &self.session.files {
                assert_eq!(fs::read(file).unwrap(), b"private original\n");
            }
            for database in &self.session.databases {
                let connection = Connection::open(&database.path).unwrap();
                let actual:Vec<(i64,String,String,String)> = connection.prepare("SELECT rowid,typeof(payload),hex(payload),coalesce(CAST(number AS TEXT),'NULL') FROM events ORDER BY rowid").unwrap().query_map([],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?))).unwrap().collect::<rusqlite::Result<_>>().unwrap();
                assert_eq!(
                    actual,
                    vec![
                        (1, "blob".into(), "00FF".into(), "NULL".into()),
                        (2, "blob".into(), "00FF".into(), "NULL".into()),
                        (3, "text".into(), "FF00".into(), "-4".into()),
                        (4, "integer".into(), "313233".into(), "2.5".into())
                    ]
                );
            }
        }
    }

    #[test]
    fn every_cleanup_crash_window_is_recoverable_and_blocks_fresh_run() {
        for point in [
            "after-capture",
            "after-trash-before-receipt",
            "after-delete-db-0",
            "after-delete-db-1",
            "after-delete-file-0",
            "after-delete-file-1",
        ] {
            let fixture = Fixture::new();
            assert!(fixture.run(Some(point)).is_err(), "{point}");
            assert!(!fixture.state.join("month-codex-2026-10.json").exists());
            let receipts = load_receipts(&fixture.state, Provider::Codex).unwrap();
            assert_eq!(receipts.len(), 1);
            assert!(receipts[0].1.pending());
            assert!(
                fixture
                    .run(None)
                    .unwrap_err()
                    .to_string()
                    .contains("requires undo")
            );
            fixture
                .undo(None)
                .unwrap_or_else(|e| panic!("{point}: {e:#}"));
            fixture.assert_original();
            assert_eq!(fixture.undo(None).unwrap()["restored_sessions"], 0);
        }
    }
    #[test]
    fn every_restore_crash_window_replays_exactly_once() {
        for point in [
            "after-restore-db-0",
            "after-restore-db-1",
            "after-restore-stage-0",
            "after-restore-publish-0",
            "after-restore-file-0",
            "after-restore-file-1",
        ] {
            let fixture = Fixture::new();
            fixture.run(None).unwrap();
            assert!(fixture.undo(Some(point)).is_err(), "{point}");
            fixture
                .undo(None)
                .unwrap_or_else(|e| panic!("{point}: {e:#}"));
            fixture.assert_original();
            assert!(fs::read_dir(&fixture.session.root).unwrap().all(|e| {
                !e.unwrap()
                    .file_name()
                    .to_string_lossy()
                    .starts_with(".aiClean-restore-")
            }));
        }
    }
    #[test]
    fn one_committed_wal_database_does_not_claim_cross_database_atomicity() {
        let fixture = Fixture::new();
        assert!(fixture.run(Some("after-delete-db-0")).is_err());
        assert_eq!(fixture.count(0), 0);
        assert_eq!(fixture.count(1), 3);
        let receipt = load_receipts(&fixture.state, Provider::Codex)
            .unwrap()
            .remove(0)
            .1;
        assert_eq!(receipt.intent.as_deref(), Some("delete-db-0"));
        assert!(receipt.committed_databases.is_empty());
        fixture.undo(None).unwrap();
        fixture.assert_original();
    }
    #[test]
    fn file_conflict_refuses_before_any_database_restore() {
        let fixture = Fixture::new();
        fixture.run(None).unwrap();
        fs::write(&fixture.session.files[1], b"new unrelated content").unwrap();
        assert!(fixture.undo(None).is_err());
        assert_eq!(fixture.count(0), 0);
        assert_eq!(fixture.count(1), 0);
        assert!(!fixture.session.files[0].exists());
        assert_eq!(
            fs::read(&fixture.session.files[1]).unwrap(),
            b"new unrelated content"
        );
    }
    #[test]
    fn rowid_conflict_refuses_before_any_restore_write() {
        let fixture = Fixture::new();
        fixture.run(None).unwrap();
        Connection::open(&fixture.session.databases[1].path)
            .unwrap()
            .execute(
                "INSERT INTO events(rowid,session_id,payload) VALUES(1,'new',99)",
                [],
            )
            .unwrap();
        assert!(fixture.undo(None).is_err());
        assert_eq!(fixture.count(0), 0);
        assert_eq!(fixture.count(1), 0);
        assert!(!fixture.session.files[0].exists());
    }
    #[test]
    fn unique_conflict_is_detected_by_savepoint_before_another_database_commit() {
        let fixture = Fixture::new();
        for database in &fixture.session.databases {
            Connection::open(&database.path)
                .unwrap()
                .execute_batch(
                    "CREATE UNIQUE INDEX unique_event ON events(number) WHERE number IS NOT NULL;",
                )
                .unwrap();
        }
        fixture.run(None).unwrap();
        Connection::open(&fixture.session.databases[1].path)
            .unwrap()
            .execute(
                "INSERT INTO events(rowid,session_id,number) VALUES(100,'new',-4)",
                [],
            )
            .unwrap();
        assert!(fixture.undo(None).is_err());
        assert_eq!(fixture.count(0), 0);
        assert!(!fixture.session.files[0].exists());
    }
    #[test]
    fn schema_change_and_snapshot_tampering_refuse_restore() {
        let fixture = Fixture::new();
        fixture.run(None).unwrap();
        Connection::open(&fixture.session.databases[1].path)
            .unwrap()
            .execute_batch("CREATE INDEX new_index ON events(payload)")
            .unwrap();
        assert!(fixture.undo(None).is_err());
        assert_eq!(fixture.count(0), 0);
        let receipt = load_receipts(&fixture.state, Provider::Codex)
            .unwrap()
            .remove(0)
            .1;
        let bundle = receipt.trash.unwrap();
        let file = bundle.join("file-0");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&file, fs::Permissions::from_mode(0o600)).unwrap();
        }
        #[cfg(windows)]
        {
            let mut permissions = fs::metadata(&file).unwrap().permissions();
            // Windows-only disposable fixture: clear its readonly attribute to test corruption.
            #[allow(clippy::permissions_set_readonly_false)]
            permissions.set_readonly(false);
            fs::set_permissions(&file, permissions).unwrap();
        }
        fs::write(&file, b"tampered").unwrap();
        assert!(
            fixture
                .undo(None)
                .unwrap_err()
                .to_string()
                .contains("checksum")
        );
    }
    #[test]
    fn without_rowid_composite_primary_keys_and_or_selectors_restore() {
        use crate::model::{DatabaseRef, TableRef};
        let mut fixture = Fixture::new();
        let path = fixture.session.root.join("composite.sqlite");
        Connection::open(&path).unwrap().execute_batch("CREATE TABLE keyed(first TEXT,second BLOB,source TEXT,target TEXT,payload,PRIMARY KEY(first,second)) WITHOUT ROWID; INSERT INTO keyed VALUES('a',X'00','s','x',7),('b',X'ff','x','s',NULL),('c',X'10','unrelated','unrelated',2);").unwrap();
        fixture.session.databases = vec![DatabaseRef {
            path: path.clone(),
            tables: vec![TableRef {
                name: "keyed".into(),
                id_columns: vec!["source".into(), "target".into()],
            }],
        }];
        fixture.run(None).unwrap();
        assert_eq!(
            Connection::open(&path)
                .unwrap()
                .query_row("SELECT count(*) FROM keyed", [], |r| r.get::<_, i64>(0))
                .unwrap(),
            1
        );
        fixture.undo(None).unwrap();
        assert_eq!(
            Connection::open(path)
                .unwrap()
                .query_row("SELECT count(*) FROM keyed", [], |r| r.get::<_, i64>(0))
                .unwrap(),
            3
        );
    }
    #[test]
    fn stop_finishes_current_unit_and_does_not_mark_month_complete() {
        struct Stopper {
            fake: Fake,
            state: PathBuf,
        }
        impl Effects for Stopper {
            fn quiet(&self, p: Provider) -> Result<()> {
                self.fake.quiet(p)
            }
            fn verify_candidate(&self, s: &Session, c: i64) -> Result<()> {
                self.fake.verify_candidate(s, c)
            }
            fn trash(&self, p: &Path) -> Result<PathBuf> {
                self.fake.trash(p)
            }
            fn find_trash(&self, p: &Path) -> Result<Option<PathBuf>> {
                self.fake.find_trash(p)
            }
            fn headroom(&self, p: &Path, b: u64) -> Result<()> {
                self.fake.headroom(p, b)
            }
            fn checkpoint(&self, p: &str) -> Result<()> {
                if p == "after-delete-db-0" {
                    fs::write(self.state.join("stop-codex"), b"stop")?;
                }
                Ok(())
            }
        }
        let fixture = Fixture::new();
        let result = run_with(
            &fixture.roots,
            &fixture.state,
            Provider::Codex,
            std::slice::from_ref(&fixture.session),
            "2026-10",
            false,
            &Stopper {
                fake: Fake::new(&fixture.state, None),
                state: fixture.state.clone(),
            },
        )
        .unwrap();
        assert_eq!(result["stopped"], true);
        assert_eq!(result["removed_sessions"], 1);
        assert!(!fixture.session.files[0].exists());
        assert_eq!(fixture.count(0), 0);
        assert_eq!(fixture.count(1), 0);
        assert!(!fixture.state.join("month-codex-2026-10.json").exists());
        fixture.undo(None).unwrap();
        fixture.assert_original();
    }
    #[test]
    fn native_trash_receipt_window_is_found_and_reported_missing_after_purge() {
        let fixture = Fixture::new();
        assert!(fixture.run(Some("after-trash-before-receipt")).is_err());
        let status = status_with(
            &fixture.roots,
            &fixture.state,
            Provider::Codex,
            &Fake::new(&fixture.state, None),
        )
        .unwrap();
        assert_eq!(status["recoverable_sessions"], 1);
        assert!(status["recoverable_trash_bytes"].as_u64().unwrap() > 0);
        let receipt = load_receipts(&fixture.state, Provider::Codex)
            .unwrap()
            .remove(0)
            .1;
        let bundle = Fake::new(&fixture.state, None)
            .find_trash(&receipt.bundle)
            .unwrap()
            .unwrap();
        fs::rename(&bundle, bundle.with_extension("purged")).unwrap();
        let status = status_with(
            &fixture.roots,
            &fixture.state,
            Provider::Codex,
            &Fake::new(&fixture.state, None),
        )
        .unwrap();
        assert_eq!(status["missing_or_corrupt_bundles"], 1);
        assert_eq!(fixture.undo(None).unwrap()["cancelled_before_deletion"], 1);
        fixture.assert_original();
    }
    #[test]
    fn triggers_are_suppressed_transactionally_and_recreated_before_commit() {
        for point in [
            None,
            Some("after-delete-before-trigger-restore-0"),
            Some("after-delete-db-0"),
        ] {
            let fixture = Fixture::new();
            for database in &fixture.session.databases {
                Connection::open(&database.path).unwrap().execute_batch("CREATE TRIGGER unsafe_delete AFTER DELETE ON events BEGIN DELETE FROM events WHERE session_id='unrelated'; END;CREATE TRIGGER unsafe_insert AFTER INSERT ON events BEGIN UPDATE events SET number=999 WHERE rowid=NEW.rowid;END").unwrap();
            }
            if point.is_some() {
                assert!(fixture.run(point).is_err());
            } else {
                fixture.run(None).unwrap();
            }
            for database in &fixture.session.databases {
                let connection = Connection::open(&database.path).unwrap();
                assert_eq!(
                    connection
                        .query_row(
                            "SELECT count(*) FROM events WHERE session_id='unrelated'",
                            [],
                            |r| r.get::<_, i64>(0)
                        )
                        .unwrap(),
                    1
                );
                assert_eq!(
                    connection
                        .query_row(
                            "SELECT count(*) FROM sqlite_schema WHERE type='trigger'",
                            [],
                            |r| r.get::<_, i64>(0)
                        )
                        .unwrap(),
                    2
                );
            }
            fixture.undo(None).unwrap();
            fixture.assert_original();
            for database in &fixture.session.databases {
                assert_eq!(
                    Connection::open(&database.path)
                        .unwrap()
                        .query_row(
                            "SELECT count(*) FROM sqlite_schema WHERE type='trigger'",
                            [],
                            |r| r.get::<_, i64>(0)
                        )
                        .unwrap(),
                    2
                );
            }
        }
    }
    #[cfg(unix)]
    #[test]
    fn symlink_leaf_parent_and_traversal_paths_are_refused() {
        use std::os::unix::fs::symlink;
        let mut fixture = Fixture::new();
        let link = fixture.session.root.join("linked");
        symlink(&fixture.session.files[0], &link).unwrap();
        fixture.session.files = vec![link];
        assert!(fixture.run(None).is_err());
        let mut fixture = Fixture::new();
        let child = fixture.session.root.join("real");
        fs::create_dir(&child).unwrap();
        fs::write(child.join("log"), b"data").unwrap();
        let link = fixture.session.root.join("linked");
        symlink(&child, &link).unwrap();
        fixture.session.files = vec![link.join("log")];
        assert!(fixture.run(None).is_err());
        let mut fixture = Fixture::new();
        fixture.session.files = vec![fixture.session.root.join("../codex/one.jsonl")];
        assert!(fixture.run(None).is_err());
    }
    #[test]
    fn foreign_key_cascade_cannot_delete_unsnapshotted_descendants() {
        use crate::model::{DatabaseRef, TableRef};
        let mut fixture = Fixture::new();
        let path = fixture.session.root.join("foreign.sqlite");
        Connection::open(&path).unwrap().execute_batch("CREATE TABLE parents(id TEXT PRIMARY KEY);CREATE TABLE children(id TEXT,parent TEXT REFERENCES parents(id) ON DELETE CASCADE);INSERT INTO parents VALUES('s');INSERT INTO children VALUES('child','s');").unwrap();
        fixture.session.databases = vec![DatabaseRef {
            path: path.clone(),
            tables: vec![TableRef {
                name: "parents".into(),
                id_columns: vec!["id".into()],
            }],
        }];
        fixture.run(None).unwrap();
        assert_eq!(
            Connection::open(&path)
                .unwrap()
                .query_row("SELECT count(*) FROM children", [], |r| r.get::<_, i64>(0))
                .unwrap(),
            1
        );
        fixture.undo(None).unwrap();
        assert_eq!(
            Connection::open(&path)
                .unwrap()
                .query_row("SELECT count(*) FROM parents", [], |r| r.get::<_, i64>(0))
                .unwrap(),
            1
        );
    }
    #[test]
    fn partial_capture_is_durable_pending_and_cancelled_without_source_mutation() {
        for point in ["after-capture-intent", "after-capture-file-0"] {
            let fixture = Fixture::new();
            assert!(fixture.run(Some(point)).is_err());
            fixture.assert_original();
            let receipt = load_receipts(&fixture.state, Provider::Codex)
                .unwrap()
                .remove(0)
                .1;
            assert_eq!(receipt.phase, "capturing");
            assert!(receipt.manifest_sha256.is_none());
            assert!(fixture.run(None).is_err());
            fixture.undo(None).unwrap();
            fixture.assert_original();
            assert!(!fixture.state.join("month-codex-2026-10.json").exists());
            fixture.run(None).unwrap();
            fixture.undo(None).unwrap();
            fixture.assert_original();
        }
    }
    #[test]
    fn writer_locks_cover_all_databases_during_candidate_revalidation() {
        struct Locked {
            fake: Fake,
            calls: Mutex<usize>,
        }
        impl Effects for Locked {
            fn quiet(&self, p: Provider) -> Result<()> {
                self.fake.quiet(p)
            }
            fn verify_candidate(&self, s: &Session, _: i64) -> Result<()> {
                *self.calls.lock().unwrap() += 1;
                for db in &s.databases {
                    let connection = Connection::open(&db.path)?;
                    connection.busy_timeout(Duration::ZERO)?;
                    assert!(
                        connection
                            .execute("UPDATE events SET number=77 WHERE session_id='s'", [])
                            .is_err()
                    );
                    assert_eq!(
                        connection.query_row(
                            "SELECT count(*) FROM events WHERE session_id='s'",
                            [],
                            |r| r.get::<_, i64>(0)
                        )?,
                        3
                    );
                }
                Ok(())
            }
            fn trash(&self, p: &Path) -> Result<PathBuf> {
                self.fake.trash(p)
            }
            fn find_trash(&self, p: &Path) -> Result<Option<PathBuf>> {
                self.fake.find_trash(p)
            }
            fn headroom(&self, p: &Path, b: u64) -> Result<()> {
                self.fake.headroom(p, b)
            }
        }
        let fixture = Fixture::new();
        let effects = Locked {
            fake: Fake::new(&fixture.state, None),
            calls: Mutex::new(0),
        };
        run_with(
            &fixture.roots,
            &fixture.state,
            Provider::Codex,
            std::slice::from_ref(&fixture.session),
            "2026-10",
            false,
            &effects,
        )
        .unwrap();
        assert_eq!(*effects.calls.lock().unwrap(), 3);
        fixture.undo(None).unwrap();
        fixture.assert_original();
    }
    #[test]
    fn low_headroom_refuses_before_capture_or_original_mutation() {
        struct Low(Fake);
        impl Effects for Low {
            fn quiet(&self, p: Provider) -> Result<()> {
                self.0.quiet(p)
            }
            fn verify_candidate(&self, s: &Session, c: i64) -> Result<()> {
                self.0.verify_candidate(s, c)
            }
            fn trash(&self, p: &Path) -> Result<PathBuf> {
                self.0.trash(p)
            }
            fn find_trash(&self, p: &Path) -> Result<Option<PathBuf>> {
                self.0.find_trash(p)
            }
            fn headroom(&self, _: &Path, _: u64) -> Result<()> {
                bail!("low headroom")
            }
        }
        let fixture = Fixture::new();
        assert!(
            run_with(
                &fixture.roots,
                &fixture.state,
                Provider::Codex,
                std::slice::from_ref(&fixture.session),
                "2026-10",
                false,
                &Low(Fake::new(&fixture.state, None))
            )
            .is_err()
        );
        fixture.assert_original();
        assert!(
            load_receipts(&fixture.state, Provider::Codex)
                .unwrap()
                .is_empty()
        );
    }
    #[test]
    fn status_uses_sizes_and_defers_full_integrity_check_until_undo() {
        let fixture = Fixture::new();
        fixture.run(None).unwrap();
        let receipt = load_receipts(&fixture.state, Provider::Codex)
            .unwrap()
            .remove(0)
            .1;
        let path = receipt.trash.unwrap().join("file-0");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        }
        #[cfg(windows)]
        {
            let mut p = fs::metadata(&path).unwrap().permissions();
            // Windows-only disposable fixture: clear its readonly attribute to test corruption.
            #[allow(clippy::permissions_set_readonly_false)]
            p.set_readonly(false);
            fs::set_permissions(&path, p).unwrap();
        }
        fs::write(&path, b"tampered exactly\n").unwrap();
        assert_eq!(fs::metadata(&path).unwrap().len(), 17);
        let result = status_with(
            &fixture.roots,
            &fixture.state,
            Provider::Codex,
            &Fake::new(&fixture.state, None),
        )
        .unwrap();
        assert_eq!(result["integrity_checked"], false);
        assert_eq!(result["recoverable_sessions"], 1);
        assert!(fixture.undo(None).is_err());
    }
    #[test]
    fn invalid_nested_state_is_rejected_before_creating_any_owned_file() {
        let fixture = Fixture::new();
        let state = fixture.session.root.join("state");
        assert!(
            run_with(
                &fixture.roots,
                &state,
                Provider::Codex,
                std::slice::from_ref(&fixture.session),
                "2026-10",
                false,
                &Fake::new(&state, None)
            )
            .is_err()
        );
        assert!(!state.exists());
        assert!(
            undo_with(
                &fixture.roots,
                &state,
                Provider::Codex,
                None,
                &Fake::new(&state, None)
            )
            .is_err()
        );
        assert!(!state.exists());
    }
    #[test]
    fn global_lock_and_pending_other_provider_block_fresh_cleanup() {
        let fixture = Fixture::new();
        private_dir(&fixture.state).unwrap();
        let lock = maintenance_lock(&fixture.state).unwrap();
        assert!(
            fixture
                .run(None)
                .unwrap_err()
                .to_string()
                .contains("maintenance")
        );
        drop(lock);
        let unit = Uuid::new_v4().to_string();
        let receipt = Receipt {
            format: FORMAT,
            provider: Provider::Claude,
            batch: Uuid::new_v4().to_string(),
            unit: unit.clone(),
            created_ms: 1,
            root: fixture.roots.claude.clone(),
            bundle: fixture
                .state
                .join("bundles")
                .join(format!("aiClean-claude-{unit}")),
            trash: None,
            manifest_sha256: None,
            phase: "capturing".into(),
            intent: None,
            committed_databases: vec![],
            deleted_files: vec![],
            original_file_bytes: 0,
            snapshot_bytes: 0,
        };
        let path = receipt_path(&fixture.state, &receipt).unwrap();
        atomic_json(&path, &receipt).unwrap();
        assert!(
            fixture
                .run(None)
                .unwrap_err()
                .to_string()
                .contains("requires undo")
        );
        fixture.assert_original();
    }
    #[test]
    fn unacknowledged_trash_can_cancel_even_when_lookup_is_denied() {
        struct Denied(Fake);
        impl Effects for Denied {
            fn quiet(&self, p: Provider) -> Result<()> {
                self.0.quiet(p)
            }
            fn verify_candidate(&self, s: &Session, c: i64) -> Result<()> {
                self.0.verify_candidate(s, c)
            }
            fn trash(&self, p: &Path) -> Result<PathBuf> {
                self.0.trash(p)
            }
            fn find_trash(&self, _: &Path) -> Result<Option<PathBuf>> {
                bail!("Trash enumeration denied")
            }
            fn headroom(&self, p: &Path, b: u64) -> Result<()> {
                self.0.headroom(p, b)
            }
        }
        let fixture = Fixture::new();
        assert!(fixture.run(Some("after-trash-before-receipt")).is_err());
        fs::write(
            &fixture.session.files[0],
            b"new user content after interruption",
        )
        .unwrap();
        let result = undo_with(
            &fixture.roots,
            &fixture.state,
            Provider::Codex,
            None,
            &Denied(Fake::new(&fixture.state, None)),
        )
        .unwrap();
        assert_eq!(result["restored_sessions"], 0);
        assert_eq!(result["cancelled_before_deletion"], 1);
        assert_eq!(
            fs::read(&fixture.session.files[0]).unwrap(),
            b"new user content after interruption"
        );
        assert_eq!(fixture.count(0), 3);
    }
    #[test]
    fn acknowledged_deletion_requires_bundle_even_when_only_one_database_committed() {
        let fixture = Fixture::new();
        assert!(fixture.run(Some("after-delete-db-0")).is_err());
        let receipt = load_receipts(&fixture.state, Provider::Codex)
            .unwrap()
            .remove(0)
            .1;
        let bundle = receipt.trash.unwrap();
        fs::rename(&bundle, bundle.with_extension("purged")).unwrap();
        assert!(fixture.undo(None).is_err());
        assert_eq!(fixture.count(0), 0);
        assert_eq!(fixture.count(1), 3);
        assert_eq!(
            load_receipts(&fixture.state, Provider::Codex).unwrap()[0]
                .1
                .phase,
            "deleting"
        );
    }
    #[test]
    fn persistent_stop_disables_scheduled_run_but_allows_manual_run() {
        let fixture = Fixture::new();
        private_dir(&fixture.state).unwrap();
        fs::write(fixture.state.join("stop-codex"), b"prior-stop-token").unwrap();
        let result = run_with(
            &fixture.roots,
            &fixture.state,
            Provider::Codex,
            std::slice::from_ref(&fixture.session),
            "2026-10",
            true,
            &Fake::new(&fixture.state, None),
        )
        .unwrap();
        assert_eq!(result["stopped"], true);
        assert!(!fixture.state.join("month-codex-2026-10.json").exists());
        fixture.assert_original();
        let result = fixture.run(None).unwrap();
        assert_eq!(result["removed_sessions"], 1);
        assert_eq!(result["stopped"], false);
        fixture.undo(None).unwrap();
        fixture.assert_original();
    }
    #[test]
    fn entire_batch_is_preflighted_before_first_session_restore() {
        let fixture = Fixture::new();
        let file = fixture.session.root.join("second-session.jsonl");
        fs::write(&file, b"second session").unwrap();
        let mut second = fixture.session.clone();
        second.id = "second".into();
        second.files = vec![file.clone()];
        second.databases.clear();
        run_with(
            &fixture.roots,
            &fixture.state,
            Provider::Codex,
            &[fixture.session.clone(), second],
            "2026-10",
            false,
            &Fake::new(&fixture.state, None),
        )
        .unwrap();
        fs::write(file, b"new unrelated data").unwrap();
        assert!(fixture.undo(None).is_err());
        assert_eq!(fixture.count(0), 0);
        assert_eq!(fixture.count(1), 0);
        assert!(!fixture.session.files[0].exists());
    }
    #[test]
    fn failed_attempt_is_persisted_independently_of_maintenance_lock_and_status_is_read_only() {
        let fixture = Fixture::new();
        private_dir(&fixture.state).unwrap();
        let lock = maintenance_lock(&fixture.state).unwrap();
        let diagnostic = json!({"provider":"codex","command":"run","at":"2026-10-10T03:30:00Z","ok":false,"error":"Application is open"});
        record_attempt(&fixture.roots, &fixture.state, Provider::Codex, &diagnostic).unwrap();
        let before = fs::read(fixture.state.join("attempt-codex.json")).unwrap();
        let result = status_with(
            &fixture.roots,
            &fixture.state,
            Provider::Codex,
            &Fake::new(&fixture.state, None),
        )
        .unwrap();
        assert_eq!(result["last_attempt"], diagnostic);
        assert_eq!(
            before,
            fs::read(fixture.state.join("attempt-codex.json")).unwrap()
        );
        assert!(
            load_receipts(&fixture.state, Provider::Codex)
                .unwrap()
                .is_empty()
        );
        assert!(
            !String::from_utf8(before)
                .unwrap()
                .contains("private original")
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(fixture.state.join("attempt-codex.json"))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o077,
                0
            );
        }
        drop(lock);
        fixture.assert_original();
        let invalid = fixture.session.root.join("invalid-state");
        assert!(record_attempt(&fixture.roots, &invalid, Provider::Codex, &diagnostic).is_err());
        assert!(!invalid.exists());
    }
    #[test]
    fn hardlinked_database_source_log_and_sidecar_are_refused() {
        for target in ["database", "log", "wal"] {
            let fixture = Fixture::new();
            let path = match target {
                "database" => fixture.session.databases[0].path.clone(),
                "log" => fixture.session.files[0].clone(),
                _ => {
                    let path = PathBuf::from(format!(
                        "{}-wal",
                        fixture.session.databases[0].path.display()
                    ));
                    fs::write(&path, b"unsafe sidecar").unwrap();
                    path
                }
            };
            let outside = fixture
                .session
                .root
                .parent()
                .unwrap()
                .join(format!("outside-{target}"));
            fs::hard_link(&path, &outside).unwrap();
            let before = fs::read(&outside).unwrap();
            assert!(fixture.run(None).is_err());
            assert_eq!(fs::read(outside).unwrap(), before);
            assert!(fixture.session.files[0].exists());
        }
    }
    #[cfg(unix)]
    #[test]
    fn owned_existing_state_is_private_before_sensitive_capture_and_status_does_not_chmod() {
        use std::os::unix::fs::PermissionsExt;
        let fixture = Fixture::new();
        fs::create_dir(&fixture.state).unwrap();
        fs::set_permissions(&fixture.state, fs::Permissions::from_mode(0o755)).unwrap();
        status_with(
            &fixture.roots,
            &fixture.state,
            Provider::Codex,
            &Fake::new(&fixture.state, None),
        )
        .unwrap();
        assert_eq!(
            fs::metadata(&fixture.state).unwrap().permissions().mode() & 0o777,
            0o755
        );
        fixture.run(None).unwrap();
        assert_eq!(
            fs::metadata(&fixture.state).unwrap().permissions().mode() & 0o777,
            0o700
        );
        let receipt = load_receipts(&fixture.state, Provider::Codex)
            .unwrap()
            .remove(0)
            .1;
        for directory in [
            fixture.state.clone(),
            fixture.state.join("operations"),
            fixture.state.join("operations/codex"),
            fixture.state.join("bundles"),
            receipt.trash.clone().unwrap(),
        ] {
            assert_eq!(
                fs::metadata(directory).unwrap().permissions().mode() & 0o077,
                0
            );
        }
        for entry in fs::read_dir(receipt.trash.unwrap()).unwrap() {
            assert_eq!(
                entry.unwrap().metadata().unwrap().permissions().mode() & 0o077,
                0
            );
        }
        fixture.undo(None).unwrap();
        fixture.assert_original();
    }
    #[cfg(windows)]
    #[test]
    fn windows_canonical_verbatim_paths_validate_without_querying_drive_prefix() {
        let temp = tempfile::tempdir().unwrap();
        let root = fs::canonicalize(temp.path()).unwrap();
        assert!(
            matches!(root.components().next(),Some(Component::Prefix(prefix)) if matches!(prefix.kind(),std::path::Prefix::VerbatimDisk(_)))
        );
        let file = root.join("fixture.jsonl");
        fs::write(&file, b"disposable fixture").unwrap();
        no_links(&root, false).unwrap();
        no_links(&file, false).unwrap();
        validate_source(&root, &file, false).unwrap();
        let missing = root.join("not-created").join("missing.jsonl");
        no_links(&missing, true).unwrap();
        assert!(no_links(&missing, false).is_err());
        assert!(!missing.exists());
        let traversal = PathBuf::from(format!("{}\\child\\..\\fixture.jsonl", root.display()));
        assert!(no_links(&traversal, true).is_err());
        assert!(no_links(&root.join("fixture.jsonl:alternate-stream"), true).is_err());
    }
    #[cfg(windows)]
    #[test]
    fn windows_readonly_snapshot_is_flushed_before_durable_cleanup_ack() {
        let fixture = Fixture::new();
        fixture.run(None).unwrap();
        let receipt = load_receipts(&fixture.state, Provider::Codex)
            .unwrap()
            .remove(0)
            .1;
        assert_eq!(receipt.phase, "complete");
        assert!(receipt.manifest_sha256.is_some());
        let bundle = receipt.trash.clone().unwrap();
        verify_bundle(&receipt, &bundle).unwrap();
        for entry in fs::read_dir(&bundle).unwrap() {
            let entry = entry.unwrap();
            assert!(entry.metadata().unwrap().permissions().readonly());
            assert!(!fs::read(entry.path()).unwrap().is_empty());
        }
        fixture.undo(None).unwrap();
        fixture.assert_original();
    }
    #[cfg(windows)]
    #[test]
    fn windows_restored_readonly_attribute_is_flushed_with_retained_write_handle() {
        for interruption in [
            None,
            Some("after-restore-stage-0"),
            Some("after-restore-publish-0"),
        ] {
            let fixture = Fixture::new();
            private_dir(&fixture.state).unwrap();
            let bundle = fixture.state.join("readonly-restore-fixture");
            private_dir(&bundle).unwrap();
            let copy = bundle.join("file-0");
            fs::write(&copy, b"readonly restored fixture").unwrap();
            let (sha256, bytes) = hash_file(&copy).unwrap();
            let path = fixture.session.files[0].clone();
            fs::remove_file(&path).unwrap();
            let snapshot = FileSnapshot {
                path: path.clone(),
                copy: "file-0".into(),
                sha256,
                bytes,
                mode: 0,
                readonly: true,
            };
            let unit = Uuid::new_v4().to_string();
            let first = restore_file(
                &fixture.session.root,
                &snapshot,
                &bundle,
                &unit,
                0,
                &Fake::new(&fixture.state, interruption),
            );
            if interruption.is_some() {
                assert!(first.is_err());
                restore_file(
                    &fixture.session.root,
                    &snapshot,
                    &bundle,
                    &unit,
                    0,
                    &Fake::new(&fixture.state, None),
                )
                .unwrap();
            } else {
                first.unwrap();
            }
            assert!(
                fs::metadata(&path).unwrap().permissions().readonly(),
                "{interruption:?}"
            );
            verify_file(&snapshot).unwrap();
            assert_eq!(fs::read(&path).unwrap(), b"readonly restored fixture");
            assert!(
                !fixture
                    .session
                    .root
                    .join(format!(".aiClean-restore-{unit}-0"))
                    .exists()
            );
            let mut permissions = fs::metadata(&path).unwrap().permissions();
            // Windows-only cleanup of this test's disposable readonly restored file.
            #[allow(clippy::permissions_set_readonly_false)]
            permissions.set_readonly(false);
            fs::set_permissions(&path, permissions).unwrap();
        }
    }
    #[cfg(target_os = "macos")]
    #[test]
    #[ignore = "Moves only a disposable UUID fixture bundle through native macOS Trash"]
    fn native_macos_storage_cleanup_and_undo_disposable_fixture() {
        struct NativeFixture {
            root: PathBuf,
            state: PathBuf,
        }
        impl Effects for NativeFixture {
            fn quiet(&self, _: Provider) -> Result<()> {
                Ok(())
            }
            fn verify_candidate(&self, session: &Session, _: i64) -> Result<()> {
                ensure!(
                    session.root == self.root
                        && session.files.iter().all(|p| p.starts_with(&self.root))
                        && session
                            .databases
                            .iter()
                            .all(|d| d.path.starts_with(&self.root)),
                    "Native test only permits its disposable fixture"
                );
                Ok(())
            }
            fn headroom(&self, _: &Path, _: u64) -> Result<()> {
                Ok(())
            }
            fn trash(&self, path: &Path) -> Result<PathBuf> {
                ensure!(
                    path.parent() == Some(self.state.join("bundles").as_path()),
                    "Native test only permits its owned bundle"
                );
                platform::trash(path)
            }
            fn find_trash(&self, path: &Path) -> Result<Option<PathBuf>> {
                platform::find_trash(path)
            }
        }
        let fixture = Fixture::new();
        let effects = NativeFixture {
            root: fixture.session.root.clone(),
            state: fixture.state.clone(),
        };
        let result = run_with(
            &fixture.roots,
            &fixture.state,
            Provider::Codex,
            std::slice::from_ref(&fixture.session),
            "2026-10",
            false,
            &effects,
        )
        .unwrap();
        assert_eq!(result["removed_sessions"], 1);
        let receipt = load_receipts(&fixture.state, Provider::Codex)
            .unwrap()
            .remove(0)
            .1;
        assert_eq!(receipt.phase, "complete");
        assert!(receipt.intent.is_none());
        let actual_trash = receipt
            .trash
            .clone()
            .expect("Native actual destination must be durably acknowledged");
        assert!(!receipt.bundle.exists());
        assert!(actual_trash.join("manifest.json").is_file());
        for file in &fixture.session.files {
            assert!(!file.exists());
        }
        assert_eq!(fixture.count(0), 0);
        assert_eq!(fixture.count(1), 0);
        let result = undo_with(
            &fixture.roots,
            &fixture.state,
            Provider::Codex,
            None,
            &effects,
        )
        .unwrap();
        assert_eq!(result["restored_sessions"], 1);
        fixture.assert_original();
        assert_eq!(
            load_receipts(&fixture.state, Provider::Codex).unwrap()[0]
                .1
                .phase,
            "restored"
        );
        // Only this test's acknowledged UUID bundle is removed, after exact
        // restoration succeeds. The user's Trash is never enumerated or purged.
        assert!(
            actual_trash
                .file_name()
                .is_some_and(|n| n.to_string_lossy().contains(&receipt.unit)),
            "Native fixture bundle identity mismatch"
        );
        verify_bundle(&receipt, &actual_trash).unwrap();
        fs::remove_dir_all(&actual_trash).unwrap();
        assert!(!actual_trash.exists());
    }
}
