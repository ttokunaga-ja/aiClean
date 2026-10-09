use std::{fs, process::Command};

#[test]
fn check_both_providers_is_read_only_and_requires_explicit_targets() {
    let temp = tempfile::tempdir().unwrap();
    let base = temp.path().canonicalize().unwrap();
    let claude = base.join("claude");
    let codex = base.join("codex");
    let state = base.join("state");
    fs::create_dir(&claude).unwrap();
    fs::create_dir(&codex).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_aiClean"))
        .env("CLAUDE_CONFIG_DIR", &claude)
        .env("CODEX_HOME", &codex)
        .env("AICLEAN_STATE_DIR", &state)
        .args(["check", "claude", "codex"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let result = String::from_utf8(output.stdout).unwrap();
    assert!(result.contains("Claude") && result.contains("Codex"));
    assert!(result.contains("候補: 0 件"));
    assert!(result.contains("読み取り専用"));
    assert!(!result.trim_start().starts_with('{'));
    assert!(!state.exists());
    assert_eq!(fs::read_dir(&claude).unwrap().count(), 0);
    assert_eq!(fs::read_dir(&codex).unwrap().count(), 0);
    for args in [
        vec!["check"],
        vec!["check", "claude", "--json"],
        vec!["plan", "claude"],
        vec!["check", "codex", "codex"],
    ] {
        let failure = Command::new(env!("CARGO_BIN_EXE_aiClean"))
            .env("CLAUDE_CONFIG_DIR", &claude)
            .env("CODEX_HOME", &codex)
            .env("AICLEAN_STATE_DIR", &state)
            .args(args)
            .output()
            .unwrap();
        assert!(!failure.status.success());
        assert!(!state.exists());
    }
}

#[test]
fn overlapping_state_is_rejected_before_any_write() {
    let temp = tempfile::tempdir().unwrap();
    let base = temp.path().canonicalize().unwrap();
    let codex = base.join("codex");
    fs::create_dir(&codex).unwrap();
    let state = codex.join("state");
    let output = Command::new(env!("CARGO_BIN_EXE_aiClean"))
        .env("CLAUDE_CONFIG_DIR", base.join("claude"))
        .env("CODEX_HOME", &codex)
        .env("AICLEAN_STATE_DIR", &state)
        .args(["stop", "codex"])
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(!state.exists());
    assert_eq!(fs::read_dir(&codex).unwrap().count(), 0);
}

#[test]
fn candidate_titles_dates_and_readonly_metadata() {
    let temp = tempfile::tempdir().unwrap();
    let base = temp.path().canonicalize().unwrap();
    let claude = base.join("claude");
    let codex = base.join("codex");
    let project = claude.join("projects/p");
    fs::create_dir_all(&project).unwrap();
    fs::create_dir_all(codex.join("sessions")).unwrap();
    let user = r#"{"type":"user","timestamp":"2020-01-01T00:00:00Z","message":{"role":"user","content":"最初の質問"}}"#;
    fs::write(project.join("a.jsonl"),format!("{user}\n{{\"type\":\"custom-title\",\"sessionId\":\"a\",\"customTitle\":\"前のタイトル\"}}\n{{\"type\":\"custom-title\",\"sessionId\":\"a\",\"customTitle\":\"日本語\\n題名\\u001b[31m赤\\u001b[0m\\u202e安全\"}}\n{{\"type\":\"custom-title\",\"sessionId\":\"other\",\"customTitle\":\"別の会話\"}}\n")).unwrap();
    fs::write(project.join("b.jsonl"), format!("{user}\n")).unwrap();
    fs::write(project.join("c.jsonl"),"{\"type\":\"assistant\",\"timestamp\":\"2020-01-01T00:00:00Z\",\"message\":{\"role\":\"assistant\",\"content\":\"助手の文章をタイトルにしない\"}}\n").unwrap();
    let rollout = codex.join("sessions/rollout-x.jsonl");
    fs::write(&rollout,"{\"timestamp\":\"2020-01-01T00:00:00Z\",\"type\":\"event_msg\",\"payload\":{\"type\":\"user_message\",\"message\":\"Codex最初\"}}\n").unwrap();
    let db = codex.join("state_5.sqlite");
    {
        let c = rusqlite::Connection::open(&db).unwrap();
        c.execute_batch("CREATE TABLE threads(id TEXT PRIMARY KEY,rollout_path TEXT,history_mode TEXT,title TEXT); CREATE TABLE thread_spawn_edges(parent_thread_id TEXT,child_thread_id TEXT,status TEXT);").unwrap();
        c.execute(
            "INSERT INTO threads VALUES ('x',?1,'legacy','保存済みCodex名')",
            [rollout.to_str().unwrap()],
        )
        .unwrap();
    }
    let originals = [
        project.join("a.jsonl"),
        project.join("b.jsonl"),
        project.join("c.jsonl"),
        rollout.clone(),
        db.clone(),
    ]
    .map(|p| {
        let b = fs::read(&p).unwrap();
        (p, b)
    });
    let output = Command::new(env!("CARGO_BIN_EXE_aiClean"))
        .env("CLAUDE_CONFIG_DIR", &claude)
        .env("CODEX_HOME", &codex)
        .env("AICLEAN_STATE_DIR", base.join("state"))
        .args(["check", "claude", "codex"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let text = String::from_utf8(output.stdout).unwrap();
    assert!(text.contains("日本語 題名赤安全"), "{text}");
    assert!(!text.contains("別の会話"));
    assert!(text.contains("最初の質問"));
    assert!(text.contains("• p/c"));
    assert!(!text.contains("助手の文章をタイトル"));
    assert!(text.contains("保存済みCodex名"));
    for id in ["p/a", "p/b", "p/c", "x"] {
        assert!(text.contains(&format!("ID: {id}")));
    }
    assert!(text.contains("最終会話: 2020-01-01"));
    assert!(!text.contains('\u{1b}') && !text.contains('\u{202e}'));
    assert!(!base.join("state").exists());
    for (p, b) in originals {
        assert_eq!(fs::read(p).unwrap(), b);
    }
    assert_eq!(fs::read_dir(&codex).unwrap().count(), 2);
}

#[test]
fn cancelled_uninstall_never_claims_completion_or_creates_state() {
    let temp = tempfile::tempdir().unwrap();
    let base = temp.path().canonicalize().unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_aiClean"))
        .env("CLAUDE_CONFIG_DIR", base.join("claude"))
        .env("CODEX_HOME", base.join("codex"))
        .env("AICLEAN_STATE_DIR", base.join("state"))
        .stdin(std::process::Stdio::null())
        .arg("uninstall")
        .output()
        .unwrap();
    assert!(output.status.success());
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(text.contains("キャンセルしました"));
    assert!(!text.contains("削除しました"));
    assert!(!text.contains("success"));
    assert!(!base.join("state").exists());
}
