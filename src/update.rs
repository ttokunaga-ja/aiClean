//! Explicit, checksum-verified updates from the official GitHub releases.
use anyhow::{Context, Result, bail};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::HashSet;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};
#[cfg(any(windows, test))]
use tempfile::NamedTempFile;

const REPO: &str = "ttokunaga-ja/aiClean";
const CURRENT: &str = env!("CARGO_PKG_VERSION");
const ASSET: &str = if cfg!(windows) {
    "aiClean-windows-x64.exe"
} else {
    "aiClean-macos"
};

pub fn run() -> Result<()> {
    if !cfg!(target_os = "macos") && !cfg!(all(windows, target_arch = "x86_64")) {
        bail!("自動更新は macOS と Windows x64 のみに対応しています");
    }
    let exe = std::env::current_exe()?.canonicalize()?;
    let _lock = UpdateLock::acquire(&exe)?;
    update(&exe, CURRENT, get, check_version)?;
    Ok(())
}

fn update(
    exe: &Path,
    current: &str,
    mut fetch: impl FnMut(&str) -> Result<Vec<u8>>,
    verify_version: impl FnOnce(&Path, &str) -> Result<()>,
) -> Result<bool> {
    let installed = fs::read(exe).context("Cannot fingerprint the installed executable")?;
    let release: Value = serde_json::from_slice(&fetch(&format!(
        "https://api.github.com/repos/{REPO}/releases/latest"
    ))?)
    .context("リリース情報を読めません")?;
    if release.get("draft").and_then(Value::as_bool) == Some(true)
        || release.get("prerelease").and_then(Value::as_bool) == Some(true)
    {
        bail!("Only published stable releases are eligible for updates");
    }
    let tag = release["tag_name"]
        .as_str()
        .context("リリースタグがありません")?;
    let latest = tag
        .strip_prefix('v')
        .context("リリースタグは vX.Y.Z が必要です")?;
    let latest_parts = parse_version(latest).context("リリースタグの版が不正です")?;
    let current_parts = parse_version(current).context("現在の版が不正です")?;
    if latest_parts <= current_parts {
        println!("最新です（v{current}）");
        return Ok(false);
    }
    println!("v{current} → {tag} に更新します");
    let base = format!("https://github.com/{REPO}/releases/download/{tag}");
    let sums = String::from_utf8(fetch(&format!("{base}/SHA256SUMS"))?)?;
    let expected = checksum(&sums, ASSET)?;
    let bytes = fetch(&format!("{base}/{ASSET}"))?;
    if hex(&Sha256::digest(&bytes)) != expected {
        bail!("ダウンロードした {ASSET} のハッシュが一致しません。現在の版はそのままです");
    }
    let parent = exe
        .parent()
        .context("実行ファイルのディレクトリがありません")?;
    let mut candidate = tempfile::Builder::new()
        .prefix(".aiClean-update-")
        .suffix(std::env::consts::EXE_SUFFIX)
        .tempfile_in(parent)?;
    candidate.write_all(&bytes)?;
    candidate.as_file().sync_all()?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(candidate.path(), fs::Permissions::from_mode(0o755))?;
    }
    // Close the write handle before execution (ETXTBSY on Unix).
    let staged = candidate.into_temp_path();
    verify_version(&staged, latest)?;
    if fs::read(exe)? != installed {
        bail!("Installed executable changed during update; replacement cancelled");
    }
    replace(exe, &staged)?;
    println!(
        "{tag} に更新しました: {}",
        crate::providers::terminal_safe(&exe.to_string_lossy(), 4000)
    );
    Ok(true)
}

struct UpdateLock {
    path: PathBuf,
    file: Option<File>,
}
impl UpdateLock {
    fn acquire(exe: &Path) -> Result<Self> {
        let mut name = exe
            .file_name()
            .context("実行ファイル名がありません")?
            .to_os_string();
        name.push(".update.lock");
        let path = exe.with_file_name(name);
        let file = OpenOptions::new().write(true).create_new(true).open(&path)
            .with_context(|| format!("更新ロックを取得できません: {}（別の更新が実行中か、中断後のロックが残っています）", path.display()))?;
        // Once created, guard ownership even if writing the diagnostic PID fails.
        let mut lock = Self {
            path,
            file: Some(file),
        };
        writeln!(lock.file.as_mut().unwrap(), "{}", std::process::id())?;
        Ok(lock)
    }
}
impl Drop for UpdateLock {
    fn drop(&mut self) {
        drop(self.file.take());
        let _ = fs::remove_file(&self.path);
    }
}

fn get(url: &str) -> Result<Vec<u8>> {
    let out = Command::new("curl")
        .args([
            "--disable",
            "--fail",
            "--silent",
            "--show-error",
            "--location",
            "--proto",
            "=https",
            "--proto-redir",
            "=https",
            "--connect-timeout",
            "15",
            "--max-time",
            "120",
            "--max-filesize",
            "134217728",
            "--user-agent",
            "aiClean-updater",
            url,
        ])
        .output()
        .context("curl を実行できません")?;
    if !out.status.success() {
        if url.ends_with("/releases/latest") {
            bail!(
                "No published aiClean release could be retrieved. The repository/release may not be published yet; keep this installation or build the local source with cargo build --release --locked. curl: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        bail!(
            "ダウンロードに失敗しました ({url}): {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(out.stdout)
}

fn parse_version(version: &str) -> Option<(u64, u64, u64)> {
    let mut parts = version.split('.');
    let mut next = || {
        let part = parts.next()?;
        if part.is_empty()
            || !part.bytes().all(|b| b.is_ascii_digit())
            || (part.len() > 1 && part.starts_with('0'))
        {
            return None;
        }
        part.parse().ok()
    };
    let result = (next()?, next()?, next()?);
    parts.next().is_none().then_some(result)
}

fn checksum(sums: &str, wanted: &str) -> Result<String> {
    let mut seen = HashSet::new();
    let mut result = None;
    for line in sums.lines() {
        // GNU sha256sum: 64 hex digits, a space, text/binary marker, filename.
        let b = line.as_bytes();
        if b.len() < 67
            || !b[..64].iter().all(u8::is_ascii_hexdigit)
            || b[64] != b' '
            || !matches!(b[65], b' ' | b'*')
        {
            bail!("SHA256SUMS の形式が不正です");
        }
        let name = &line[66..];
        if name.is_empty()
            || name.chars().any(char::is_whitespace)
            || name.contains('/')
            || name.contains('\\')
            || !seen.insert(name)
        {
            bail!("SHA256SUMS に不正または重複したファイル名があります");
        }
        if name == wanted {
            result = Some(line[..64].to_ascii_lowercase());
        }
    }
    result.with_context(|| format!("SHA256SUMS に {wanted} がありません"))
}
fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn check_version(bin: &Path, version: &str) -> Result<()> {
    check_version_with_timeout(bin, version, Duration::from_secs(10))
}

fn check_version_with_timeout(bin: &Path, version: &str, timeout: Duration) -> Result<()> {
    let stdout = tempfile::tempfile()?;
    let stderr = tempfile::tempfile()?;
    let mut child = Command::new(bin)
        .arg("--version")
        .stdin(Stdio::null())
        .stdout(stdout.try_clone()?)
        .stderr(stderr)
        .spawn()
        .context("新しい版を実行できません。現在の版はそのままです")?;
    let start = Instant::now();
    while child.try_wait()?.is_none() {
        if start.elapsed() >= timeout {
            let _ = child.kill();
            let _ = child.wait();
            bail!("新しい版の確認がタイムアウトしました。現在の版はそのままです");
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    let status = child.wait()?;
    use std::io::{Seek, SeekFrom};
    let mut stdout = stdout;
    stdout.seek(SeekFrom::Start(0))?;
    let mut bytes = Vec::new();
    stdout.take(1024).read_to_end(&mut bytes)?;
    let shown = std::str::from_utf8(&bytes).context("新しい版の表示が UTF-8 ではありません")?;
    if !status.success() || shown.trim_end_matches(['\r', '\n']) != format!("aiClean {version}") {
        bail!("新しい版の版確認に失敗しました。現在の版はそのままです");
    }
    Ok(())
}

fn replace(exe: &Path, candidate: &Path) -> Result<()> {
    #[cfg(windows)]
    {
        let backup = windows_replace(exe, candidate, |from, to| fs::rename(from, to))?;
        if let Err(error) = fs::remove_file(&backup) {
            eprintln!(
                "更新前の実行ファイルを保持しました: {}（終了後に削除できます: {error}）",
                backup.display()
            );
        }
        Ok(())
    }
    #[cfg(not(windows))]
    fs::rename(candidate, exe).with_context(|| format!("置き換えられません: {}", exe.display()))
}

// Also compiled in Unix tests to exercise Windows rollback independently.
#[cfg(any(windows, test))]
fn windows_replace(
    exe: &Path,
    candidate: &Path,
    mut rename: impl FnMut(&Path, &Path) -> std::io::Result<()>,
) -> Result<PathBuf> {
    let backup = NamedTempFile::new_in(exe.parent().context("親ディレクトリがありません")?)?;
    let backup = backup.into_temp_path();
    // Reserve a unique name, then release it so Windows can rename into it.
    fs::remove_file(&backup)?;
    rename(exe, &backup).context("更新前の実行ファイルを移動できません")?;
    if let Err(error) = rename(candidate, exe) {
        if let Err(rollback) = rename(&backup, exe) {
            let saved = backup.keep()?;
            bail!(
                "置き換えと復元に失敗しました: {error}; {rollback}。更新前の実行ファイル: {}",
                saved.display()
            );
        }
        return Err(error).context("置き換えに失敗しました。現在の版を復元しました");
    }
    Ok(backup.keep()?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strict_numeric_semver() {
        assert!(parse_version("0.10.0") > parse_version("0.9.9"));
        for value in [
            "",
            "v1.2.3",
            "1.2",
            "1.2.3.4",
            "+1.2.3",
            "01.2.3",
            "1.2.3-rc.1",
            "1.2.3+build",
            "1.2. 3",
            "18446744073709551616.0.0",
        ] {
            assert_eq!(parse_version(value), None, "{value}");
        }
    }

    #[test]
    fn strict_manifest_rejects_malformed_and_duplicate_entries() {
        let hash = "A".repeat(64);
        assert_eq!(
            checksum(&format!("{hash} *{ASSET}\n"), ASSET).unwrap(),
            "a".repeat(64)
        );
        for sums in [
            format!("abc  {ASSET}"),
            format!("{}  {ASSET}", "g".repeat(64)),
            format!("{hash}  {ASSET}\n{hash}  {ASSET}"),
            format!("{hash}  {ASSET}\nbroken"),
            format!("{hash}  ../{ASSET}"),
            format!("{hash}  other"),
        ] {
            assert!(checksum(&sums, ASSET).is_err(), "{sums}");
        }
    }

    fn mock_update(exe: &Path, hash: &str, tag: &str, version_ok: bool) -> Result<bool> {
        let bytes = b"new executable";
        update(
            exe,
            "0.1.0",
            |url| {
                if url.ends_with("/latest") {
                    return Ok(format!(r#"{{"tag_name":"{tag}"}}"#).into_bytes());
                }
                if url.ends_with("/SHA256SUMS") {
                    return Ok(format!("{hash}  {ASSET}\n").into_bytes());
                }
                Ok(bytes.to_vec())
            },
            |candidate, version| {
                assert_eq!(fs::read(candidate)?, bytes);
                assert_eq!(version, "0.2.0");
                if version_ok {
                    Ok(())
                } else {
                    bail!("wrong version")
                }
            },
        )
    }

    #[test]
    fn successful_update_and_failures_preserve_installed_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let exe = dir.path().join("aiClean-test");
        let good_hash = hex(&Sha256::digest(b"new executable"));
        fs::write(&exe, b"original").unwrap();
        assert!(mock_update(&exe, &"0".repeat(64), "v0.2.0", true).is_err());
        assert_eq!(fs::read(&exe).unwrap(), b"original");
        assert!(mock_update(&exe, &good_hash, "v0.2.0", false).is_err());
        assert_eq!(fs::read(&exe).unwrap(), b"original");
        assert!(mock_update(&exe, &good_hash, "v01.2.0", true).is_err());
        assert_eq!(fs::read(&exe).unwrap(), b"original");
        assert!(!mock_update(&exe, &good_hash, "v0.1.0", true).unwrap());
        assert_eq!(fs::read(&exe).unwrap(), b"original");
        assert!(mock_update(&exe, &good_hash, "v0.2.0", true).unwrap());
        assert_eq!(fs::read(&exe).unwrap(), b"new executable");
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 1);
    }

    #[test]
    fn changed_installation_and_unavailable_release_preserve_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let exe = dir.path().join("aiClean");
        fs::write(&exe, b"original").unwrap();
        assert!(
            update(
                &exe,
                "0.1.0",
                |_| bail!("unpublished release"),
                |_, _| Ok(())
            )
            .is_err()
        );
        assert_eq!(fs::read(&exe).unwrap(), b"original");
        let bytes = b"replacement";
        let hash = hex(&Sha256::digest(bytes));
        assert!(
            update(
                &exe,
                "0.1.0",
                |url| {
                    if url.ends_with("/latest") {
                        Ok(br#"{"tag_name":"v0.2.0"}"#.to_vec())
                    } else if url.ends_with("/SHA256SUMS") {
                        Ok(format!("{hash}  {ASSET}\n").into_bytes())
                    } else {
                        Ok(bytes.to_vec())
                    }
                },
                |_, _| {
                    fs::write(&exe, b"concurrent installation")?;
                    Ok(())
                }
            )
            .is_err()
        );
        assert_eq!(fs::read(&exe).unwrap(), b"concurrent installation");
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 1);
    }

    #[test]
    fn lock_prevents_overlap_and_releases_on_drop() {
        let dir = tempfile::tempdir().unwrap();
        let exe = dir.path().join("aiClean");
        let guard = UpdateLock::acquire(&exe).unwrap();
        assert!(UpdateLock::acquire(&exe).is_err());
        drop(guard);
        assert!(UpdateLock::acquire(&exe).is_ok());
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 0);
    }

    #[test]
    fn windows_failed_install_rolls_back_and_failed_rollback_retains_backup() {
        let dir = tempfile::tempdir().unwrap();
        let exe = dir.path().join("aiClean.exe");
        let candidate = dir.path().join("candidate.exe");
        fs::write(&exe, b"original").unwrap();
        fs::write(&candidate, b"replacement").unwrap();
        let mut calls = 0;
        assert!(
            windows_replace(&exe, &candidate, |from, to| {
                calls += 1;
                if calls == 2 {
                    return Err(std::io::Error::other("injected installation failure"));
                }
                fs::rename(from, to)
            })
            .is_err()
        );
        assert_eq!(calls, 3);
        assert_eq!(fs::read(&exe).unwrap(), b"original");
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 2);
        let mut calls = 0;
        let error = windows_replace(&exe, &candidate, |from, to| {
            calls += 1;
            if calls > 1 {
                return Err(std::io::Error::other("injected failure"));
            }
            fs::rename(from, to)
        })
        .unwrap_err();
        assert!(error.to_string().contains("更新前の実行ファイル"));
        assert!(
            fs::read_dir(dir.path())
                .unwrap()
                .any(|entry| fs::read(entry.unwrap().path()).unwrap() == b"original")
        );
    }

    #[cfg(unix)]
    #[test]
    fn candidate_version_must_be_exact_and_successful() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let candidate = dir.path().join("candidate");
        for (body, valid) in [
            ("printf 'aiClean 0.2.0\\n'", true),
            ("printf 'aiClean 0.2.1\\n'", false),
            ("printf ' aiClean 0.2.0\\n'", false),
            ("printf 'aiClean 0.2.0\\n'; exit 1", false),
        ] {
            fs::write(&candidate, format!("#!/bin/sh\n{body}\n")).unwrap();
            fs::set_permissions(&candidate, fs::Permissions::from_mode(0o755)).unwrap();
            assert_eq!(check_version(&candidate, "0.2.0").is_ok(), valid);
        }
    }

    #[cfg(unix)]
    #[test]
    fn staged_version_timeout_preserves_candidate_and_kills_child() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let candidate = dir.path().join("candidate");
        fs::write(&candidate, "#!/bin/sh\nexec sleep 30\n").unwrap();
        fs::set_permissions(&candidate, fs::Permissions::from_mode(0o755)).unwrap();
        let start = Instant::now();
        assert!(
            check_version_with_timeout(&candidate, "0.2.0", Duration::from_millis(50)).is_err()
        );
        assert!(start.elapsed() < Duration::from_secs(2));
        assert!(candidate.exists());
    }

    // Launch a copy of this harness, because its current_exe must actually be
    // running when the Windows rename-and-replace path is exercised.
    #[cfg(windows)]
    #[test]
    fn windows_running_executable_replacement() {
        let dir = tempfile::tempdir().unwrap();
        let harness = dir.path().join("running-harness.exe");
        fs::copy(std::env::current_exe().unwrap(), &harness).unwrap();
        let output = Command::new(&harness)
            .args([
                "--exact",
                "update::tests::windows_running_executable_child",
                "--nocapture",
            ])
            .env("AICLEAN_UPDATE_TEST_CHILD", "1")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(fs::read(harness).unwrap(), b"replacement candidate");
    }

    #[cfg(windows)]
    #[test]
    fn windows_running_executable_child() {
        if std::env::var_os("AICLEAN_UPDATE_TEST_CHILD").is_none() {
            return;
        }
        let exe = std::env::current_exe().unwrap();
        let candidate = exe.with_file_name("candidate.exe");
        fs::write(&candidate, b"replacement candidate").unwrap();
        replace(&exe, &candidate).unwrap();
        assert_eq!(fs::read(&exe).unwrap(), b"replacement candidate");
        // Running backup remains available until this harness exits.
        assert!(fs::read_dir(exe.parent().unwrap()).unwrap().count() >= 2);
    }
}
