//! Stop only aiClean-owned tasks and remove only the running executable.
use anyhow::{Context, Result, bail};
use sha2::{Digest, Sha256};
use std::{
    env, fs,
    io::{self, Write},
    path::Path,
};

fn fingerprint(path: &Path) -> Result<Vec<u8>> {
    let meta = fs::symlink_metadata(path)?;
    if !meta.file_type().is_file() {
        bail!("削除対象が通常ファイルではありません: {}", path.display());
    }
    Ok(Sha256::digest(fs::read(path)?).to_vec())
}

#[cfg(unix)]
fn identity(path: &Path) -> Result<(u64, u64)> {
    use std::os::unix::fs::MetadataExt;
    let metadata = fs::symlink_metadata(path)?;
    Ok((metadata.dev(), metadata.ino()))
}

/// Prompt for the canonical running executable and remove only that executable.
pub fn run(state: &Path) -> Result<()> {
    let executable =
        fs::canonicalize(env::current_exe()?).context("実行中の実行ファイルを特定できません")?;
    let original = fingerprint(&executable)?;
    #[cfg(unix)]
    let original_identity = identity(&executable)?;
    eprintln!("aiClean executable: {}", executable.display());
    eprintln!(
        "aiClean tasks for state {} will be stopped and unregistered. All sessions, cleanup receipts, and restore bundles are preserved.",
        state.display()
    );
    eprint!("この実行ファイルを削除しますか？ [y/N]: ");
    io::stderr().flush()?;
    let mut answer = String::new();
    io::stdin().read_line(&mut answer)?;
    if !matches!(answer.trim().to_lowercase().as_str(), "y" | "yes" | "はい") {
        eprintln!("キャンセルしました。変更していません。");
        return Ok(());
    }
    if fingerprint(&executable)? != original {
        bail!("確認中に実行ファイルが変更されました。削除を中止しました");
    }
    #[cfg(unix)]
    if identity(&executable)? != original_identity {
        bail!("確認中に実行ファイルが置き換えられました。削除を中止しました");
    }
    finish_uninstall(
        state,
        crate::schedule::stop,
        crate::storage::maintenance_lock,
        crate::schedule::remove,
        || {
            // Task removal can take time; refuse to remove a newly replaced binary.
            if fingerprint(&executable)? != original {
                bail!("Executable changed while removing tasks; uninstall cancelled");
            }
            #[cfg(unix)]
            if identity(&executable)? != original_identity {
                bail!("Executable replaced while removing tasks; uninstall cancelled");
            }
            #[cfg(windows)]
            {
                windows::schedule(&executable, &original)
            }
            #[cfg(not(windows))]
            {
                fs::remove_file(&executable).context("実行ファイルを削除できません")?;
                eprintln!("実行ファイルを削除しました: {}", executable.display());
                Ok(())
            }
        },
    )
}

fn stop_owned_tasks(
    state: &Path,
    mut stop: impl FnMut(&Path, crate::model::Provider) -> Result<serde_json::Value>,
) -> Result<()> {
    for provider in [
        crate::model::Provider::Claude,
        crate::model::Provider::Codex,
    ] {
        stop(state, provider).with_context(|| format!("Could not stop aiClean-owned {} task. Executable retained; sessions and cleanup state preserved", provider.name()))?;
    }
    Ok(())
}

fn finish_uninstall<G>(
    state: &Path,
    stop: impl FnMut(&Path, crate::model::Provider) -> Result<serde_json::Value>,
    acquire: impl FnOnce(&Path) -> Result<G>,
    mut remove: impl FnMut(&Path, crate::model::Provider) -> Result<serde_json::Value>,
    delete_executable: impl FnOnce() -> Result<()>,
) -> Result<()> {
    stop_owned_tasks(state, stop)?;
    let _lock = acquire(state).context("Could not acquire maintenance lock: cleanup or restore may still be running. Stop requests and executable are retained; wait and retry uninstall")?;
    for provider in [
        crate::model::Provider::Claude,
        crate::model::Provider::Codex,
    ] {
        remove(state, provider).with_context(|| format!("Could not unregister aiClean-owned {} task. Executable retained; sessions and cleanup state preserved", provider.name()))?;
    }
    delete_executable()
}

// A failed restore must not drop the helper's persistent diagnostic receipt.
#[cfg(any(windows, test))]
fn rollback_with_receipt(
    receipt_directory: tempfile::TempDir,
    restore: impl FnOnce() -> Result<()>,
) -> Result<()> {
    if let Err(error) = restore() {
        let kept = receipt_directory.keep();
        let receipt = kept.join("receipt.json");
        if !receipt.exists() {
            // Launch failures have no helper receipt yet. Preserve a static receipt
            // and put the detailed restore error in the returned diagnostic.
            if let Err(write_error) = fs::write(
                &receipt,
                br#"{"status":"failed","detail":"Helper setup failed and executable restore failed"}"#,
            ) {
                bail!("{error:#}。記録を書き込めません: {write_error}。記録の保存先: {}", receipt.display());
            }
        }
        bail!("{error:#}。結果の記録: {}", receipt.display());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn owned_task_stop_preserves_sessions_receipts_and_bundles() {
        let dir = tempfile::tempdir().unwrap();
        for name in ["session.jsonl", "receipt.json", "bundle.zip"] {
            fs::write(dir.path().join(name), b"preserved").unwrap();
        }
        let mut seen = Vec::new();
        stop_owned_tasks(dir.path(), |state, provider| {
            assert_eq!(state, dir.path());
            seen.push(provider);
            Ok(serde_json::json!({"stopped": true}))
        })
        .unwrap();
        assert_eq!(
            seen,
            vec![
                crate::model::Provider::Claude,
                crate::model::Provider::Codex
            ]
        );
        for name in ["session.jsonl", "receipt.json", "bundle.zip"] {
            assert_eq!(fs::read(dir.path().join(name)).unwrap(), b"preserved");
        }
        let mut calls = 0;
        assert!(
            stop_owned_tasks(dir.path(), |_, _| {
                calls += 1;
                bail!("injected stop failure")
            })
            .is_err()
        );
        assert_eq!(calls, 1);
    }

    #[test]
    fn uninstall_orders_stop_lock_remove_before_executable_deletion() {
        use std::cell::RefCell;
        let dir = tempfile::tempdir().unwrap();
        let executable = dir.path().join("aiClean");
        fs::write(&executable, b"executable").unwrap();
        let calls = RefCell::new(Vec::new());
        finish_uninstall(
            dir.path(),
            |_, p| {
                calls.borrow_mut().push(format!("stop {}", p.name()));
                Ok(serde_json::json!({}))
            },
            |_| {
                calls.borrow_mut().push("lock".into());
                Ok(())
            },
            |_, p| {
                calls.borrow_mut().push(format!("remove {}", p.name()));
                Ok(serde_json::json!({}))
            },
            || {
                calls.borrow_mut().push("delete".into());
                fs::remove_file(&executable)?;
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(
            *calls.borrow(),
            [
                "stop claude",
                "stop codex",
                "lock",
                "remove claude",
                "remove codex",
                "delete"
            ]
        );
        assert!(!executable.exists());
    }

    #[test]
    fn stop_lock_or_removal_failure_never_deletes_executable_or_cleanup_state() {
        use std::cell::Cell;
        for fail_at in 1..=5 {
            let dir = tempfile::tempdir().unwrap();
            for name in [
                "aiClean",
                "operations.json",
                "bundle.zip",
                "receipt.json",
                "stop-marker",
            ] {
                fs::write(dir.path().join(name), b"preserved").unwrap();
            }
            let step = Cell::new(0);
            let advance = || -> Result<()> {
                step.set(step.get() + 1);
                if step.get() == fail_at {
                    bail!("injected failure")
                }
                Ok(())
            };
            let result = finish_uninstall(
                dir.path(),
                |_, _| {
                    advance()?;
                    Ok(serde_json::json!({}))
                },
                |_| advance(),
                |_, _| {
                    advance()?;
                    Ok(serde_json::json!({}))
                },
                || {
                    fs::remove_file(dir.path().join("aiClean"))?;
                    Ok(())
                },
            );
            assert!(result.is_err());
            assert_eq!(step.get(), fail_at);
            for name in [
                "aiClean",
                "operations.json",
                "bundle.zip",
                "receipt.json",
                "stop-marker",
            ] {
                assert_eq!(fs::read(dir.path().join(name)).unwrap(), b"preserved");
            }
        }
    }

    #[test]
    fn maintenance_guard_is_held_through_task_and_executable_removal() {
        use std::cell::Cell;
        struct Guard<'a>(&'a Cell<bool>);
        impl Drop for Guard<'_> {
            fn drop(&mut self) {
                self.0.set(false);
            }
        }
        let active = Cell::new(false);
        let dir = tempfile::tempdir().unwrap();
        finish_uninstall(
            dir.path(),
            |_, _| Ok(serde_json::json!({})),
            |_| {
                active.set(true);
                Ok(Guard(&active))
            },
            |_, _| {
                assert!(active.get());
                Ok(serde_json::json!({}))
            },
            || {
                assert!(active.get());
                Ok(())
            },
        )
        .unwrap();
        assert!(!active.get());
    }

    #[test]
    fn failed_restore_preserves_existing_helper_receipt() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("receipt.json");
        let original = br#"{"status":"failed","detail":"helper failure"}"#;
        fs::write(&path, original).unwrap();
        let error = rollback_with_receipt(directory, || {
            bail!("元の場所に別のファイルがあるため上書きしません")
        })
        .unwrap_err();
        assert!(error.to_string().contains(&path.display().to_string()));
        assert!(error.to_string().contains("上書きしません"));
        assert_eq!(fs::read(&path).unwrap(), original);
        fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[test]
    fn failed_launch_and_restore_create_a_persistent_failure_receipt() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("receipt.json");
        assert!(rollback_with_receipt(directory, || bail!("staged bytes changed")).is_err());
        assert!(fs::read_to_string(&path).unwrap().contains("failed"));
        fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[test]
    fn successful_restore_discards_cancelled_helper_receipt() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().to_path_buf();
        fs::write(path.join("receipt.json"), b"cancelled").unwrap();
        rollback_with_receipt(directory, || Ok(())).unwrap();
        assert!(!path.exists());
    }
}

#[cfg(windows)]
mod windows {
    use super::*;
    use std::{
        os::windows::process::CommandExt,
        process::{Child, Command, Stdio},
        thread,
        time::{Duration, Instant},
    };

    // All variable data travels through child-only environment variables, never source.
    const SCRIPT: &str = r#"
$ErrorActionPreference = 'Stop'
$staged = $env:EXEC_UNINSTALL_STAGED
$receipt = $env:EXEC_UNINSTALL_RECEIPT
$ready = $env:EXEC_UNINSTALL_READY
$expected = $env:EXEC_UNINSTALL_HASH
$parentId = [int]$env:EXEC_UNINSTALL_PID
function Write-Receipt([string]$status, [string]$detail) {
    $record = @{ status = $status; detail = $detail; staged_path = $staged; original_path = $env:EXEC_UNINSTALL_ORIGINAL; parent_pid = $parentId; expected_sha256 = $expected; utc = [DateTime]::UtcNow.ToString('o') }
    $nextReceipt = $receipt + '.next'
    [IO.File]::WriteAllText($nextReceipt, ($record | ConvertTo-Json -Compress), [Text.UTF8Encoding]::new($false))
    if ([IO.File]::Exists($receipt)) {
        # Windows PowerShell 5.1 coerces a null string argument into an invalid
        # empty backup path. An explicit private backup keeps replacement atomic.
        $previousReceipt = $receipt + '.previous'
        [IO.File]::Replace($nextReceipt, $receipt, $previousReceipt)
        [IO.File]::Delete($previousReceipt)
    }
    else { [IO.File]::Move($nextReceipt, $receipt) }
}
try {
    $parent = [Diagnostics.Process]::GetProcessById($parentId)
    $null = $parent.Handle
    if ((Get-FileHash -LiteralPath $staged -Algorithm SHA256).Hash -ne $expected) { throw 'Staged executable fingerprint changed before scheduling' }
    Write-Receipt 'scheduled' 'Waiting for the uninstall process to exit'
    [IO.File]::WriteAllText($ready, 'ready')
    if (-not $parent.WaitForExit(120000)) { throw 'Parent did not exit within 120 seconds; staged executable retained' }
    $deadline = [DateTime]::UtcNow.AddSeconds(30)
    while ($true) {
        if ((Get-FileHash -LiteralPath $staged -Algorithm SHA256).Hash -ne $expected) { throw 'Staged executable fingerprint changed; retained' }
        try { Remove-Item -LiteralPath $staged -Force -ErrorAction Stop; break }
        catch { if ([DateTime]::UtcNow -ge $deadline) { throw }; Start-Sleep -Milliseconds 100 }
    }
    Write-Receipt 'deleted' 'Staged executable deleted after parent exit'
} catch {
    $failure = $_.Exception.ToString()
    # This diagnostic uses only .NET so module-import failures remain observable.
    [IO.File]::WriteAllText($receipt + '.error.txt', $failure, [Text.UTF8Encoding]::new($false))
    try { Write-Receipt 'failed' $failure }
    catch { [IO.File]::AppendAllText($receipt + '.error.txt', [Environment]::NewLine + $_.Exception.ToString()) }
    exit 1
}
"#;

    fn rollback(executable: &Path, staged: &Path, hash: &[u8]) -> Result<()> {
        if fingerprint(staged).with_context(|| {
            format!(
                "退避ファイルを確認できません。残存ファイル: {}",
                staged.display()
            )
        })? != hash
        {
            bail!(
                "退避ファイルが変更されたため復元を中止しました。残存ファイル: {}",
                staged.display()
            );
        }
        match fs::symlink_metadata(executable) {
            Err(e) if e.kind() == io::ErrorKind::NotFound => (),
            _ => bail!(
                "元の場所に別のファイルがあるため上書きしません。残存ファイル: {}",
                staged.display()
            ),
        }
        fs::rename(staged, executable)
            .with_context(|| format!("復元できません。残存ファイル: {}", staged.display()))
    }

    fn stop(child: &mut Child) -> Result<()> {
        if child.try_wait()?.is_none() {
            child.kill()?;
        }
        child.wait()?;
        Ok(())
    }

    pub fn schedule(executable: &Path, hash: &[u8]) -> Result<()> {
        let directory = executable
            .parent()
            .context("実行ファイルの親フォルダがありません")?;
        let reserved = tempfile::Builder::new()
            .prefix(".uninstall-")
            .suffix(".pending-delete.exe")
            .tempfile_in(directory)?;
        let staged = reserved.path().to_path_buf();
        reserved.close()?;
        let receipt_directory = tempfile::Builder::new()
            .prefix("executable-uninstall-")
            .tempdir()?;
        let receipt = receipt_directory.path().join("receipt.json");
        let ready = receipt_directory.path().join("ready");
        if fingerprint(executable)? != hash {
            bail!("退避前に実行ファイルが変更されました");
        }
        fs::rename(executable, &staged).context("実行中の実行ファイルを退避できません")?;
        let system_root = env::var_os("SystemRoot").context("SystemRootがありません");
        let launch = system_root.and_then(|root| {
            let powershell_home =
                std::path::PathBuf::from(root).join("System32/WindowsPowerShell/v1.0");
            Command::new(powershell_home.join("powershell.exe"))
                .args(["-NoProfile", "-NonInteractive", "-Command", SCRIPT])
                // Parent pwsh sessions may expose incompatible PowerShell 7 modules.
                // Restrict only this native Windows PowerShell helper's discovery.
                .env("PSModulePath", powershell_home.join("Modules"))
                .env("EXEC_UNINSTALL_STAGED", &staged)
                .env("EXEC_UNINSTALL_ORIGINAL", executable)
                .env("EXEC_UNINSTALL_RECEIPT", &receipt)
                .env("EXEC_UNINSTALL_READY", &ready)
                .env(
                    "EXEC_UNINSTALL_HASH",
                    hash.iter().map(|b| format!("{b:02x}")).collect::<String>(),
                )
                .env("EXEC_UNINSTALL_PID", std::process::id().to_string())
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .creation_flags(0x08000000)
                .spawn()
                .map_err(Into::into)
        });
        let mut child = match launch {
            Ok(child) => child,
            Err(error) => {
                rollback_with_receipt(receipt_directory, || rollback(executable, &staged, hash))
                    .with_context(|| format!("削除ヘルパーを起動できません: {error:#}"))?;
                return Err(error)
                    .context("削除ヘルパーを起動できません。実行ファイルは復元しました");
            }
        };
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if fs::read_to_string(&ready).ok().as_deref() == Some("ready") {
                break;
            }
            let helper_running = matches!(child.try_wait(), Ok(None));
            if !helper_running || Instant::now() >= deadline {
                if let Err(error) = stop(&mut child) {
                    let kept = receipt_directory.keep();
                    bail!(
                        "ヘルパーを停止確認できません: {error}。退避ファイル: {}。記録: {}",
                        staged.display(),
                        kept.join("receipt.json").display()
                    );
                }
                let diagnostic = receipt.with_extension("json.error.txt");
                let detail = fs::read_to_string(&diagnostic).unwrap_or_else(|_| {
                    "ヘルパーのエラー記録はありません（準備待機の期限超過または起動直後の失敗）"
                        .into()
                });
                if let Err(error) = rollback(executable, &staged, hash) {
                    return rollback_with_receipt(receipt_directory, || Err(error))
                        .with_context(|| format!("削除ヘルパーの準備を確認できません: {detail}。実行ファイルの復元にも失敗しました"));
                }
                let kept = receipt_directory.keep();
                bail!(
                    "削除ヘルパーの準備を確認できません: {detail}。実行ファイルは復元しました。記録フォルダ: {}",
                    kept.display()
                );
            }
            thread::sleep(Duration::from_millis(50));
        }
        let kept = receipt_directory.keep();
        eprintln!(
            "削除を予約しました。このプロセス終了後に退避ファイルを削除します（まだ削除完了ではありません）。"
        );
        eprintln!(
            "結果の記録: {}（status: scheduled / deleted / failed）",
            kept.join("receipt.json").display()
        );
        eprintln!("失敗時に手動削除する退避ファイル: {}", staged.display());
        Ok(())
    }
}
