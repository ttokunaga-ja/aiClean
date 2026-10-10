mod human;
use ai_clean::{
    model::{Inventory, Provider, Roots, Session, validate_providers},
    platform, providers, schedule, storage,
};
use anyhow::{Context, Result, bail};
use chrono::{Datelike, Local, TimeZone};
use clap::{Args, Parser, Subcommand};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

#[derive(Debug, Parser)]
#[command(
    name = "aiClean",
    version,
    help_template = "{about}\n\n使い方: {usage}\n\n{all-args}",
    about = "Claude / Codex の会話ログを復元可能な形で整理します"
)]
struct Cli {
    #[arg(
        long,
        global = true,
        env = "AICLEAN_STATE_DIR",
        help = "復元記録・自動実行設定の保存先（既定 ~/.aiClean）"
    )]
    state_dir: Option<PathBuf>,
    #[command(subcommand)]
    command: Commands,
}
#[derive(Debug, Args)]
struct Targets {
    #[arg(value_enum, required=true, num_args=1..)]
    providers: Vec<Provider>,
}
#[derive(Debug, Subcommand)]
enum Commands {
    /// 候補と容量を読み取り専用で確認します。
    Check(Targets),
    /// 10日以降、月1回の整理を実行します。
    Run {
        #[arg(value_enum, required=true, num_args=1..)]
        providers: Vec<Provider>,
        #[arg(long, hide = true)]
        scheduled: bool,
    },
    /// 毎月10日03:30の自動実行を有効にします。
    Enable(Targets),
    /// 自動実行を無効にし、進行中の整理に停止を要求します。
    Stop(Targets),
    /// 自動実行・前回結果・復元状態を表示します。
    Status(Targets),
    /// 最新の未復元バッチを復元します。
    Undo {
        #[arg(value_enum, required=true, num_args=1..)]
        providers: Vec<Provider>,
        #[arg(long)]
        batch: Option<String>,
    },
    /// 公式リリースを検証して更新します。
    Update,
    /// CLIと自動実行を削除します（会話と復元記録は保持）。
    Uninstall,
}
fn state_path(cli: &Cli) -> Result<PathBuf> {
    let path = cli.state_dir.clone().unwrap_or(
        dirs::home_dir()
            .context("Home directory unavailable")?
            .join(".aiClean"),
    );
    Ok(if path.is_absolute() {
        path
    } else {
        std::env::current_dir()?.join(path)
    })
}
fn cutoff() -> Result<i64> {
    let now = Local::now();
    let midnight = chrono::NaiveDate::from_ymd_opt(now.year(), now.month(), 1)
        .context("Invalid month")?
        .and_hms_opt(0, 0, 0)
        .context("Invalid midnight")?;
    Ok(Local
        .from_local_datetime(&midnight)
        .earliest()
        .context("Month boundary unavailable in OS timezone")?
        .timestamp_millis())
}
fn select(inventory: &Inventory, boundary: i64) -> (Vec<Session>, BTreeMap<String, usize>) {
    let mut candidates = Vec::new();
    let mut reasons = BTreeMap::new();
    for session in &inventory.sessions {
        let reason = session
            .skip_reason
            .clone()
            .or_else(|| match session.last_message_ms {
                None => Some("unknown last conversation timestamp".into()),
                Some(stamp) if stamp >= boundary => Some("current month or newer".into()),
                Some(_) => None,
            });
        if let Some(reason) = reason {
            *reasons.entry(reason).or_insert(0) += 1;
        } else {
            candidates.push(session.clone());
        }
    }
    (candidates, reasons)
}
fn log_bytes(sessions: &[Session]) -> Result<u64> {
    let mut seen = std::collections::BTreeSet::new();
    let mut size = 0u64;
    for session in sessions {
        for file in &session.files {
            if !seen.insert(file) {
                continue;
            }
            let meta = std::fs::symlink_metadata(file)?;
            if !meta.is_file() {
                bail!("Inventory contains a non-regular file: {}", file.display());
            }
            size = size.checked_add(meta.len()).context("Size overflow")?;
        }
    }
    Ok(size)
}
fn database_capacity(root: &Path) -> Result<Value> {
    let mut bytes = 0u64;
    let mut reusable = 0u64;
    if root.is_dir() {
        for entry in std::fs::read_dir(root)? {
            let path = entry?.path();
            if path.extension().is_none_or(|e| e != "sqlite") {
                continue;
            }
            let meta = std::fs::symlink_metadata(&path)?;
            if !meta.is_file() || meta.file_type().is_symlink() {
                continue;
            }
            bytes = bytes.saturating_add(meta.len());
            let c = rusqlite::Connection::open_with_flags(
                &path,
                rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
            )?;
            let pages: i64 = c.query_row("PRAGMA freelist_count", [], |r| r.get(0))?;
            let page_size: i64 = c.query_row("PRAGMA page_size", [], |r| r.get(0))?;
            let pages = u64::try_from(pages).context("Invalid freelist size")?;
            let page_size = u64::try_from(page_size).context("Invalid page size")?;
            reusable = reusable.saturating_add(pages.saturating_mul(page_size));
        }
    }
    Ok(
        json!({"file_bytes":bytes,"reusable_page_bytes":reusable,"physical_reclaim_guaranteed":false}),
    )
}
fn execute(cli: &Cli) -> Result<(Value, bool)> {
    let roots = Roots::from_env()?;
    let state = state_path(cli)?;
    ai_clean::model::validate_state_location(&roots, &state)?;
    let (targets, batch) = match &cli.command {
        Commands::Check(t) | Commands::Enable(t) | Commands::Stop(t) | Commands::Status(t) => {
            (&t.providers, None)
        }
        Commands::Run { providers, .. } => (providers, None),
        Commands::Undo { providers, batch } => (providers, batch.as_deref()),
        Commands::Update => {
            ai_clean::update::run()?;
            return Ok((json!({"results":[]}), false));
        }
        Commands::Uninstall => {
            ai_clean::uninstall::run(&state)?;
            return Ok((json!({"results":[]}), false));
        }
    };
    validate_providers(targets)?;
    let now = Local::now();
    let boundary = cutoff()?;
    let mut results = Vec::new();
    let mut failed = false;
    for &p in targets {
        let result = (|| -> Result<Value> {
            match &cli.command {
                Commands::Check(_) => {
                    let inventory = providers::inventory(p, &roots)?;
                    let (sessions, skipped) = select(&inventory, boundary);
                    let candidates = sessions.iter().map(|s| { let (title, diagnostics) = providers::display_title(s); Ok(json!({"id":s.id,"title":title,"title_diagnostic":if diagnostics.is_empty(){None}else{Some(diagnostics.join("; "))},"last_message_ms":s.last_message_ms,"bytes":log_bytes(std::slice::from_ref(s))?})) }).collect::<Result<Vec<Value>>>()?;
                    Ok(
                        json!({"provider":p,"command":"check","read_only":true,"candidates":candidates,"candidate_count":sessions.len(),"candidate_log_bytes":log_bytes(&sessions)?,"skipped_by_reason":skipped,"warnings":inventory.warnings,"database":database_capacity(roots.get(p))?,"recovery":storage::status(&roots,&state,p)?,"mutation_block_reason":platform::ensure_quiet(p).err().map(|e|format!("{e:#}"))}),
                    )
                }
                Commands::Run { scheduled, .. } => {
                    if now.day() < 10 {
                        bail!(
                            "Cleanup starts on the 10th; use check to inspect without changing data"
                        );
                    }
                    platform::ensure_quiet(p)?;
                    let inventory = providers::inventory(p, &roots)?;
                    let (sessions, skipped) = select(&inventory, boundary);
                    let mut v = storage::run(
                        &roots,
                        &state,
                        p,
                        &sessions,
                        &now.format("%Y-%m").to_string(),
                        *scheduled,
                    )?;
                    v["skipped_by_reason"] = json!(skipped);
                    v["warnings"] = json!(inventory.warnings);
                    Ok(v)
                }
                Commands::Enable(_) => schedule::enable(&state, p),
                Commands::Stop(_) => schedule::stop(&state, p),
                Commands::Status(_) => Ok(
                    json!({"provider":p,"command":"status","schedule":schedule::status(&state,p)?,"storage":storage::status(&roots,&state,p)?,"mutation_block_reason":platform::ensure_quiet(p).err().map(|e|format!("{e:#}")),"state_dir":state}),
                ),
                Commands::Undo { .. } => storage::undo(&roots, &state, p, batch),
                _ => unreachable!(),
            }
        })();
        let mut value = match result {
            Ok(v) => v,
            Err(e) => {
                let mut error = json!({"provider":p,"success":false,"error":format!("{e:#}")});
                if matches!(cli.command, Commands::Run { .. } | Commands::Undo { .. }) {
                    match storage::status(&roots, &state, p) {
                        Ok(s) => error["recovery_after_failure"] = s,
                        Err(e) => {
                            error["recovery_diagnostic"] =
                                json!(format!("復元状態を確認できません: {e:#}"))
                        }
                    }
                }
                error
            }
        };
        if matches!(cli.command, Commands::Run { .. } | Commands::Undo { .. }) {
            value["command"] = json!(if matches!(cli.command, Commands::Run { .. }) {
                "run"
            } else {
                "undo"
            });
            value["attempted_at"] = json!(Local::now().to_rfc3339());
            if let Err(e) = storage::record_attempt(&roots, &state, p, &value) {
                value["attempt_record_error"] = json!(format!("{e:#}"));
                failed = true;
            }
        }
        if value["success"] == false || value["stopped"] == true {
            failed = true;
        }
        if value["command"].is_null() {
            value["command"] = json!(match &cli.command {
                Commands::Enable(_) => "enable",
                Commands::Stop(_) => "stop",
                Commands::Status(_) => "status",
                _ => "check",
            });
        }
        results.push(value);
    }
    Ok((
        json!({"timezone_offset":now.format("%:z").to_string(),"cutoff_ms":boundary,"results":results,"success":!failed}),
        failed,
    ))
}
fn main() {
    let cli = match Cli::try_parse() {
        Ok(cli) => cli,
        Err(e) => {
            if matches!(
                e.kind(),
                clap::error::ErrorKind::DisplayHelp | clap::error::ErrorKind::DisplayVersion
            ) {
                print!("{e}");
                std::process::exit(0);
            }
            eprintln!(
                "引数が不正です。aiClean --help で使い方を確認してください。\n診断: {}",
                human::safe(&e.to_string(), 4000)
            );
            std::process::exit(2);
        }
    };
    match execute(&cli) {
        Ok((result, failed)) => {
            print!("{}", human::render(&result));
            if failed {
                std::process::exit(1);
            }
        }
        Err(e) => {
            eprintln!(
                "処理に失敗しました。\n診断: {}",
                human::safe(&format!("{e:#}"), 4000)
            );
            std::process::exit(2);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn check_explicit_plan_removed() {
        assert!(Cli::try_parse_from(["aiClean", "check", "claude", "codex"]).is_ok());
        assert!(Cli::try_parse_from(["aiClean", "plan", "claude"]).is_err());
        assert!(Cli::try_parse_from(["aiClean", "check"]).is_err());
    }
    #[test]
    fn duplicates_refused_both_orders_supported() {
        assert!(validate_providers(&[Provider::Claude, Provider::Claude]).is_err());
        assert!(
            Cli::try_parse_from(["aiClean", "undo", "codex", "claude", "--batch", "abc"]).is_ok()
        );
        assert!(Cli::try_parse_from(["aiClean", "status", "claude", "codex"]).is_ok());
    }
    #[test]
    fn month_boundary_exclusive() {
        let base = Session {
            provider: Provider::Codex,
            root: PathBuf::from("/tmp"),
            id: "fixture".into(),
            last_message_ms: Some(99),
            files: vec![],
            databases: vec![],
            skip_reason: None,
        };
        let mut equal = base.clone();
        equal.last_message_ms = Some(100);
        let i = Inventory {
            provider: Provider::Codex,
            sessions: vec![base, equal],
            warnings: vec![],
        };
        let (selected, skipped) = select(&i, 100);
        assert_eq!(selected.len(), 1);
        assert_eq!(skipped["current month or newer"], 1);
    }
}
