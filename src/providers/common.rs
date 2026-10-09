use anyhow::{Context, Result, bail};
use chrono::{DateTime, Utc};
use serde_json::Value;
use std::{
    fs,
    io::{BufRead, BufReader},
    path::{Component, Path, PathBuf},
};

pub(super) fn plain(root: &Path, path: &Path) -> Result<()> {
    if !path.is_absolute()
        || !path.starts_with(root)
        || path.components().any(|c| matches!(c, Component::ParentDir))
    {
        bail!("unsafe path outside provider root");
    }
    for p in path.ancestors() {
        let m = fs::symlink_metadata(p).context("missing source path")?;
        if m.file_type().is_symlink() {
            bail!("unsafe symlink path");
        }
        #[cfg(windows)]
        {
            use std::os::windows::fs::MetadataExt;
            if m.file_attributes() & 0x400 != 0 {
                bail!("unsafe reparse path");
            }
        }
    }
    if fs::symlink_metadata(path)?.is_file() {
        crate::platform::ensure_single_link(path)?;
    }
    Ok(())
}
pub(super) fn root_path(root: &Path) -> Result<PathBuf> {
    let p = if root.is_absolute() {
        root.to_path_buf()
    } else {
        std::env::current_dir()?.join(root)
    };
    plain(&p, &p)?;
    Ok(p)
}
pub(super) fn valid_ms(value: i64) -> Result<i64> {
    if value <= 0 || value > Utc::now().timestamp_millis() + 86_400_000 {
        bail!("unknown or future message timestamp");
    }
    Ok(value)
}
fn timestamp(v: &Value) -> Result<i64> {
    let s = v
        .get("timestamp")
        .and_then(Value::as_str)
        .context("unknown message timestamp")?;
    valid_ms(
        DateTime::parse_from_rfc3339(s)
            .context("unknown message timestamp")?
            .timestamp_millis(),
    )
}
fn text_content(v: &Value) -> bool {
    match v {
        Value::String(s) => !s.is_empty(),
        Value::Array(items) => items.iter().any(|x| {
            x.get("type")
                .and_then(Value::as_str)
                .is_some_and(|t| matches!(t, "text" | "input_text" | "output_text"))
                && x.get("text")
                    .and_then(Value::as_str)
                    .is_some_and(|s| !s.is_empty())
        }),
        _ => false,
    }
}
fn user_content(v: &Value) -> Result<bool> {
    match v {
        Value::String(s) => Ok(!s.is_empty()),
        Value::Array(items) => {
            let mut genuine = false;
            for item in items {
                match item.get("type").and_then(Value::as_str) {
                    Some("text" | "input_text" | "output_text") => {
                        let text = item
                            .get("text")
                            .and_then(Value::as_str)
                            .context("unknown user text content")?;
                        genuine |= !text.is_empty();
                    }
                    Some(
                        "image" | "document" | "audio" | "input_image" | "input_file"
                        | "input_audio",
                    ) => genuine = true,
                    Some("tool_result") => {}
                    _ => bail!("unknown user content type"),
                }
            }
            Ok(genuine)
        }
        _ => bail!("unknown user message content"),
    }
}
pub(super) fn messages(path: &Path, claude: bool) -> Result<Option<i64>> {
    let before = fs::metadata(path)?;
    let mut latest = None;
    let mut open_turns = std::collections::BTreeSet::new();
    let mut pending_tools = std::collections::BTreeSet::new();
    let mut tool_turn_open = false;
    let mut tool_messages = std::collections::BTreeSet::new();
    let mut unmatched_tool_stops = std::collections::BTreeSet::new();
    let mut completed_tools = std::collections::BTreeSet::new();
    for line in BufReader::new(fs::File::open(path)?).lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let v: Value = serde_json::from_str(&line).context("malformed conversation JSONL")?;
        if !v.is_object() {
            bail!("malformed conversation JSONL");
        }
        if !claude && v.get("type").and_then(Value::as_str) == Some("event_msg") {
            let payload = &v["payload"];
            let turn = payload
                .get("turn_id")
                .and_then(Value::as_str)
                .unwrap_or("unknown")
                .to_string();
            match payload.get("type").and_then(Value::as_str) {
                Some("task_started") => {
                    open_turns.insert(turn);
                }
                Some("task_complete" | "turn_aborted") => {
                    open_turns.remove(&turn);
                }
                _ => {}
            }
        }
        let genuine = if claude {
            let meta = v.get("isMeta").and_then(Value::as_bool).unwrap_or(false)
                || v.get("isCompactSummary")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
            match v.get("type").and_then(Value::as_str) {
                Some("user" | "assistant") => {
                    let message = v
                        .get("message")
                        .context("unknown conversation message content")?;
                    let content = message
                        .get("content")
                        .context("unknown conversation message content")?;
                    let assistant = v.get("type").and_then(Value::as_str) == Some("assistant");
                    let message_id = message.get("id").and_then(Value::as_str);
                    let mut tool_calls = false;
                    if let Some(blocks) = content.as_array() {
                        for block in blocks {
                            match block.get("type").and_then(Value::as_str) {
                                Some("tool_use") if assistant => {
                                    tool_calls = true;
                                    tool_turn_open = true;
                                    let id = block
                                        .get("id")
                                        .and_then(Value::as_str)
                                        .context("unknown pending tool call")?
                                        .to_string();
                                    if !completed_tools.contains(&id) {
                                        pending_tools.insert(id);
                                    }
                                }
                                Some("tool_result") if !assistant => {
                                    let id = block
                                        .get("tool_use_id")
                                        .and_then(Value::as_str)
                                        .context("unknown tool result identity")?
                                        .to_string();
                                    pending_tools.remove(&id);
                                    completed_tools.insert(id);
                                }
                                _ => {}
                            }
                        }
                    }
                    if assistant {
                        if tool_calls && let Some(id) = message_id {
                            tool_messages.insert(id.to_string());
                            unmatched_tool_stops.remove(id);
                        }
                        let tool_stop =
                            message.get("stop_reason").and_then(Value::as_str) == Some("tool_use");
                        if tool_stop
                            && !tool_calls
                            && !message_id.is_some_and(|id| tool_messages.contains(id))
                        {
                            unmatched_tool_stops
                                .insert(message_id.unwrap_or("unknown-message-id").to_string());
                            tool_turn_open = true;
                        }
                        if !meta && !tool_calls && !tool_stop && text_content(content) {
                            tool_turn_open = false;
                        }
                        !meta && text_content(content)
                    } else {
                        !meta && user_content(content)?
                    }
                }
                _ => false,
            }
        } else {
            let payload = &v["payload"];
            match v.get("type").and_then(Value::as_str) {
                Some("event_msg") => matches!(
                    payload.get("type").and_then(Value::as_str),
                    Some("user_message" | "agent_message")
                ),
                Some("response_item")
                    if payload.get("type").and_then(Value::as_str) == Some("message") =>
                {
                    match payload.get("role").and_then(Value::as_str) {
                        Some("user") => user_content(
                            payload
                                .get("content")
                                .context("unknown user message content")?,
                        )?,
                        Some("assistant") => payload.get("content").is_some_and(text_content),
                        _ => false,
                    }
                }
                _ => false,
            }
        };
        if genuine {
            let ts = timestamp(&v)?;
            latest = Some(latest.map_or(ts, |x: i64| x.max(ts)));
        }
    }
    if !unmatched_tool_stops.is_empty() {
        bail!("unknown incomplete Claude tool call");
    }
    if !pending_tools.is_empty() || tool_turn_open {
        bail!("incomplete pending Claude tool call");
    }
    if !open_turns.is_empty() {
        bail!("incomplete legacy turn");
    }
    let after = fs::metadata(path)?;
    if before.len() != after.len() || before.modified()? != after.modified()? {
        bail!("conversation changed during read");
    }
    Ok(latest)
}
pub(super) fn skip(s: &mut crate::model::Session, reason: impl Into<String>) {
    if s.skip_reason.is_none() {
        s.skip_reason = Some(reason.into());
    }
}
pub(super) fn quote(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}
