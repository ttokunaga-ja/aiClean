use chrono::{Local, TimeZone};
use serde_json::Value;
use std::fmt::Write;

pub fn safe(s: &str, limit: usize) -> String {
    ai_clean::providers::terminal_safe(s, limit)
}
fn diagnostic(s: &str) -> String {
    if s.contains("Selected provider application or CLI is running") {
        "選択したプロバイダーの会話アプリ・CLIが起動中です。終了してから整理・復元を再実行してください。".into()
    } else {
        safe(s, 4000)
    }
}
fn bytes(n: u64) -> String {
    let units = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut size = n as f64;
    let mut unit = 0;
    while size >= 1024.0 && unit < units.len() - 1 {
        size /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{n} B")
    } else {
        format!("{size:.1} {}", units[unit])
    }
}
fn state(v: &Value) -> &'static str {
    match v.as_bool() {
        Some(true) => "有効",
        Some(false) => "無効",
        None => "不明",
    }
}
fn string(v: &Value) -> String {
    v.as_str().map(|s| safe(s, 4000)).unwrap_or_else(|| {
        if v.is_null() {
            "記録なし".into()
        } else {
            v.as_u64()
                .map(|n| n.to_string())
                .unwrap_or_else(|| "不明".into())
        }
    })
}
fn number(v: &Value, key: &str) -> u64 {
    v[key].as_u64().unwrap_or(0)
}
fn outcome(v: &Value) -> String {
    if v.is_null() {
        return "記録なし".into();
    }
    if let Some(e) = v["error"].as_str() {
        return format!("失敗 — 診断: {}", diagnostic(e));
    }
    if v["failed"] == true || v["success"] == false || v["ok"] == false {
        return format!(
            "失敗（移動済み {} 件。statusで復元状態を確認してください）",
            number(v, "removed_sessions")
        );
    }
    if v["already_completed"] == true {
        return "今月はすでに整理済みです。".into();
    }
    if v["nothing_to_restore"] == true {
        return "復元する会話はありません。".into();
    }
    if v["stopped"] == true {
        return format!(
            "停止しました。移動済み {} 件 / 実行時に保持 {} 件。未完了部分はstatusで確認してください。",
            number(v, "removed_sessions"),
            number(v, "skipped_sessions")
        );
    }
    if !v["restored_sessions"].is_null() {
        return format!(
            "復元 {} 件 / 元ファイル {} / 削除前に取り消した操作 {} 件",
            number(v, "restored_sessions"),
            bytes(number(v, "restored_file_bytes")),
            number(v, "cancelled_before_deletion")
        );
    }
    if !v["removed_sessions"].is_null() {
        if number(v, "removed_sessions") == 0 {
            return format!(
                "移動する候補はありませんでした。実行時に保持 {} 件",
                number(v, "skipped_sessions")
            );
        }
        return format!(
            "ゴミ箱へ移動 {} 件 / 元ファイル {} / 復元資材 {} / 実行時に保持 {} 件",
            number(v, "removed_sessions"),
            bytes(number(v, "removed_source_file_bytes")),
            bytes(number(v, "recoverable_bundle_bytes")),
            number(v, "skipped_sessions")
        );
    }
    "結果の詳細を確認できません。statusで状態を確認してください。".into()
}
fn reason(s: &str) -> String {
    let japanese = match s {
        "current month or newer" => "今月以降の会話",
        "unknown last conversation timestamp" => "最終会話日時を確認できない",
        "active or unknown goal" => "実行中または状態不明のゴール",
        "incomplete message timestamp" => "未完了のメッセージ",
        _ if s.contains("spawn") => "関連する親子セッションを保持",
        _ if s.contains("unsafe") || s.contains("symlink") => "安全を確認できないパスを保持",
        _ if s.contains("unsupported") => "未対応の構造のため保持",
        _ if s.contains("incomplete") => "処理が未完了のため保持",
        _ if s.contains("unknown") => "状態を確認できないため保持",
        _ => "安全条件を満たさないため保持",
    };
    format!("{japanese}（診断: {}）", safe(s, 1000))
}
fn recovery(out: &mut String, v: &Value, history: bool) {
    let _ = writeln!(
        out,
        "  復元可能: {} 件 / ゴミ箱 {} / 作業領域 {}（ハッシュはundo時に検証）",
        number(v, "recoverable_sessions"),
        bytes(number(v, "recoverable_trash_bytes")),
        bytes(number(v, "recoverable_staging_bytes"))
    );
    for (key, label) in [
        ("incomplete_capture_bytes", "未完了の退避容量"),
        ("missing_or_corrupt_bundles", "復元資材の欠落・破損数"),
    ] {
        if number(v, key) > 0 {
            let _ = writeln!(
                out,
                "  警告: {label}: {}",
                if key.ends_with("bytes") {
                    bytes(number(v, key))
                } else {
                    number(v, key).to_string()
                }
            );
        }
    }
    let pending = v["pending_operations"].as_array();
    let _ = writeln!(
        out,
        "  復元が必要な中断処理: {} 件",
        pending.map_or(0, Vec::len)
    );
    for item in pending.into_iter().flatten() {
        let _ = writeln!(
            out,
            "    バッチ {} / セッション {}（undoコマンドで復元）",
            string(&item["batch"]),
            string(&item["unit"])
        );
    }
    if !history {
        return;
    }
    for (key, label) in [("last_attempt", "最終試行"), ("last_result", "前回結果")] {
        let r = &v[key];
        let when = r
            .get("attempted_at")
            .or_else(|| r.get("at"))
            .map(string)
            .unwrap_or_else(|| "日時の記録なし".into());
        let _ = writeln!(
            out,
            "  {label}: {}{}",
            outcome(r),
            if r.is_null() {
                String::new()
            } else {
                format!(" / {when}")
            }
        );
    }
}
fn skips(out: &mut String, r: &Value) {
    if let Some(map) = r["skipped_by_reason"].as_object() {
        for (k, v) in map {
            let _ = writeln!(out, "  保持: {v} 件 — {}", reason(k));
        }
    }
    for w in r["warnings"].as_array().into_iter().flatten() {
        let _ = writeln!(out, "  警告: {}", string(w));
    }
}
fn schedule(out: &mut String, s: &Value) {
    let actual = &s["actual"];
    let _ = writeln!(
        out,
        "  自動実行（OSの実状態）: {} / 設定上: {}",
        state(&actual["enabled"]),
        state(&s["desired_enabled"])
    );
    if !actual["registered"].is_null() {
        let _ = writeln!(
            out,
            "  OSタスク登録: {}",
            if actual["registered"] == true {
                "登録済み"
            } else {
                "未登録"
            }
        );
    } else if !actual["loaded"].is_null() {
        let _ = writeln!(
            out,
            "  OSタスク読込: {}",
            if actual["loaded"] == true {
                "読込済み"
            } else {
                "未読込"
            }
        );
    }
    if !actual["running"].is_null() {
        let _ = writeln!(
            out,
            "  現在の実行: {}",
            if actual["running"] == true {
                "実行中"
            } else {
                "停止中"
            }
        );
    }
    if let Some(os_state) = actual["state"].as_str() {
        let _ = writeln!(
            out,
            "  現在の実行: {}",
            match os_state {
                "Running" => "実行中",
                "Ready" => "待機中",
                "Disabled" => "無効",
                _ => "不明",
            }
        );
    }
    if actual["loaded"] == false || actual["registered"] == false {
        let _ = writeln!(
            out,
            "  自動実行は稼働していません。有効化する場合はaiClean enableで登録してください。"
        );
    }
    let next = actual
        .get("next_run")
        .filter(|v| !v.is_null())
        .unwrap_or(&s["schedule"]["next_calendar_occurrence"]);
    let _ = writeln!(
        out,
        "  次回予定: {}（毎月10日03:30。無効・不明の場合は実行を保証できません）",
        string(next)
    );
    if s["stop_requested"] == true {
        let _ = writeln!(out, "  停止要求が保存されています。");
    }
}
pub fn render(result: &Value) -> String {
    let mut out = String::new();
    for r in result["results"].as_array().into_iter().flatten() {
        let provider = match r["provider"].as_str() {
            Some("claude") => "Claude",
            Some("codex") => "Codex",
            _ => "aiClean",
        };
        let command = r["command"].as_str().unwrap_or("");
        let _ = writeln!(
            out,
            "{provider} — {}",
            match command {
                "check" => "候補確認（読み取り専用）",
                "status" => "状態確認（読み取り専用）",
                "run" => "整理結果",
                "undo" => "復元結果",
                "enable" => "自動実行の有効化結果",
                "stop" => "自動実行の停止結果",
                _ => "処理結果",
            }
        );
        if let Some(e) = r["error"].as_str() {
            let _ = writeln!(
                out,
                "  処理に失敗しました。変更済みの部分がある場合はstatusで復元状態を確認してください。\n  診断: {}",
                diagnostic(e)
            );
            if !r["attempt_record_error"].is_null() {
                let _ = writeln!(
                    out,
                    "  実行記録の保存エラー: {}",
                    string(&r["attempt_record_error"])
                );
            }
            if !r["recovery_after_failure"].is_null() {
                recovery(&mut out, &r["recovery_after_failure"], true);
            }
            if !r["recovery_diagnostic"].is_null() {
                let _ = writeln!(out, "  {}", string(&r["recovery_diagnostic"]));
            }
            continue;
        }
        match command {
            "check" => {
                let _ = writeln!(
                    out,
                    "  候補: {} 件 / 元ファイル合計: {}",
                    r["candidate_count"],
                    bytes(r["candidate_log_bytes"].as_u64().unwrap_or(0))
                );
                for c in r["candidates"].as_array().into_iter().flatten() {
                    let date = c["last_message_ms"]
                        .as_i64()
                        .and_then(|n| Local.timestamp_millis_opt(n).single())
                        .map(|d| d.format("%Y-%m-%d %H:%M:%S %:z").to_string())
                        .unwrap_or_else(|| "不明".into());
                    let _ = writeln!(
                        out,
                        "  • {}\n    最終会話: {date} / 元ファイル: {} / ID: {}",
                        {
                            let title = safe(c["title"].as_str().unwrap_or(""), 80);
                            if title.is_empty() {
                                safe(c["id"].as_str().unwrap_or(""), 80)
                            } else {
                                title
                            }
                        },
                        bytes(c["bytes"].as_u64().unwrap_or(0)),
                        safe(c["id"].as_str().unwrap_or(""), 4000)
                    );
                    if !c["title_diagnostic"].is_null() {
                        let _ =
                            writeln!(out, "    タイトル診断: {}", string(&c["title_diagnostic"]));
                    }
                }
                if let Some(map) = r["skipped_by_reason"].as_object() {
                    for (k, v) in map {
                        let _ = writeln!(out, "  保持: {v} 件 — {}", reason(k));
                    }
                }
                for w in r["warnings"].as_array().into_iter().flatten() {
                    let _ = writeln!(out, "  警告: {}", string(w));
                }
                let _ = writeln!(
                    out,
                    "  元ファイル容量は解放容量の保証ではありません。ゴミ箱はOSが削除するまで容量を使用し、SQLiteの空きページは再利用されます。"
                );
                let _ = writeln!(
                    out,
                    "  SQLite全体: {} / 再利用可能ページ: {}（物理解放量ではありません）",
                    bytes(number(&r["database"], "file_bytes")),
                    bytes(number(&r["database"], "reusable_page_bytes"))
                );
                recovery(&mut out, &r["recovery"], false);
            }
            "status" => {
                schedule(&mut out, &r["schedule"]);
                recovery(&mut out, &r["storage"], true);
            }
            "enable" | "stop" => {
                schedule(&mut out, r);
                let _ = writeln!(
                    out,
                    "  {}",
                    if command == "enable" {
                        "登録処理を終了しました。今すぐの整理は実行していません。"
                    } else {
                        "停止要求を保存しました。進行中の処理はセッション間で停止します。"
                    }
                );
            }
            "run" | "undo" => {
                let _ = writeln!(out, "  {}", outcome(r));
                if !r["batch"].is_null() {
                    let _ = writeln!(out, "  バッチID: {}", string(&r["batch"]));
                }
                skips(&mut out, r);
                if !r["attempt_record_error"].is_null() {
                    let _ = writeln!(
                        out,
                        "  実行記録の保存エラー: {}",
                        string(&r["attempt_record_error"])
                    );
                }
                let _ = writeln!(
                    out,
                    "  元ファイル容量は物理解放容量の保証ではありません。ゴミ箱はOSが削除するまで容量を使用します。"
                );
            }
            _ => {
                let _ = writeln!(out, "  {}", outcome(r));
            }
        }
        if matches!(command, "check" | "status") {
            if let Some(e) = r["mutation_block_reason"].as_str() {
                let _ = writeln!(out, "  整理・復元を実行できない理由: {}", diagnostic(e));
            } else {
                let _ = writeln!(
                    out,
                    "  アプリ稼働チェック: 実行を妨げるプロセスは検出されませんでした（整理可能の保証ではありません）。"
                );
            }
            let _ = writeln!(
                out,
                "  整理は毎月10日以降に月1回。今月初日00:00より前の実際の会話のみが対象です。タイトル・作成日時・更新日時は判定に使いません。"
            );
        }
        out.push('\n');
    }
    out
}
#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn unicode_and_terminal_controls() {
        assert_eq!(
            safe(
                "日本\n語\u{1b}[31m赤\u{1b}[0m\u{202e}安全\u{1b}]0;evil\u{7}",
                80
            ),
            "日本 語赤安全"
        );
        assert_eq!(safe("あいうえ", 2), "あい…");
    }
    #[test]
    fn process_refusal_names_only_selected_provider_applications() {
        let text =
            diagnostic("Selected provider application or CLI is running; close it before mutation");
        assert!(text.contains("選択したプロバイダーの会話アプリ・CLI"));
        assert!(!text.contains("エディタ"));
    }
    #[test]
    fn important_outcomes() {
        for (fields, wanted) in [
            (json!({"already_completed":true}), "すでに整理済み"),
            (
                json!({"nothing_to_restore":true}),
                "復元する会話はありません",
            ),
            (json!({"stopped":true}), "停止しました"),
            (json!({"removed_sessions":0}), "候補はありません"),
        ] {
            let mut r = fields;
            r["command"] = json!("run");
            assert!(render(&json!({"results":[r]})).contains(wanted));
        }
        let s = render(
            &json!({"results":[{"command":"status","provider":"codex","schedule":{"desired_enabled":true,"actual":{"enabled":null}},"storage":{"pending_operations":[{"batch":"abc","phase":"captured"}],"last_attempt":{"error":"process open"}}}]}),
        );
        assert!(
            s.contains("不明 / 設定上: 有効") && s.contains("abc") && s.contains("process open")
        );
    }
}
