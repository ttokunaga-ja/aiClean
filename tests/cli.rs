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
        .args(["check", "claude", "codex", "--json"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let result: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(result["results"].as_array().unwrap().len(), 2);
    assert!(
        result["results"]
            .as_array()
            .unwrap()
            .iter()
            .all(|r| r["read_only"] == true)
    );
    assert!(!state.exists());
    assert_eq!(fs::read_dir(&claude).unwrap().count(), 0);
    assert_eq!(fs::read_dir(&codex).unwrap().count(), 0);
    for args in [
        vec!["check"],
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
