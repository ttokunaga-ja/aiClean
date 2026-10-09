//! Native OS boundaries. Child output is captured and never echoed (process arguments can contain secrets).
use crate::model::Provider;
use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use std::{
    ffi::OsString,
    fs,
    io::Read,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::{Duration, Instant},
};

pub(crate) struct CommandOutput {
    pub success: bool,
    pub code: Option<i32>,
    pub stdout: String,
}
/// A bounded runner; stderr is intentionally not exposed in errors.
pub(crate) fn run_command(
    program: &str,
    args: &[OsString],
    env: &[(OsString, OsString)],
) -> Result<CommandOutput> {
    let mut command = Command::new(program);
    command
        .args(args)
        .envs(env.iter().cloned())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let mut child = command.spawn().context("OS helper could not start")?;
    let stdout = child.stdout.take().context("OS helper pipe unavailable")?;
    // Drain in parallel so a large process inventory cannot block the child. Limit retained output.
    let (sender, receiver) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut bytes = Vec::new();
        let result = stdout
            .take(4 * 1024 * 1024 + 1)
            .read_to_end(&mut bytes)
            .map(|_| bytes);
        let _ = sender.send(result);
    });
    let deadline = Instant::now() + Duration::from_secs(30);
    let exit = loop {
        if let Some(exit) = child.try_wait()? {
            break exit;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            bail!("OS helper timed out; inspect/recover any pending operation");
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    let bytes = receiver
        .recv_timeout(deadline.saturating_duration_since(Instant::now()))
        .map_err(|_| anyhow::anyhow!("OS helper output exceeded bounded deadline"))??;
    if bytes.len() > 4 * 1024 * 1024 {
        bail!("OS helper inventory exceeded safe bound");
    }
    let stdout = String::from_utf8(bytes).context("OS helper returned invalid text")?;
    Ok(CommandOutput {
        success: exit.success(),
        code: exit.code(),
        stdout,
    })
}
#[cfg(target_os = "windows")]
pub(crate) fn powershell(script: &str, env: &[(OsString, OsString)]) -> Result<CommandOutput> {
    let root = std::env::var_os("SystemRoot").context("Windows SystemRoot unavailable")?;
    let exe = PathBuf::from(root).join("System32/WindowsPowerShell/v1.0/powershell.exe");
    run_command(exe.to_str().context("Unsupported helper path")?,&["-NoLogo".into(),"-NoProfile".into(),"-NonInteractive".into(),"-Command".into(),format!("$ErrorActionPreference='Stop'; [Console]::OutputEncoding=New-Object System.Text.UTF8Encoding($false); {script}").into()],env)
}
fn process_is_busy(command: &str, _provider: Provider) -> bool {
    let s = command.to_ascii_lowercase();
    let first = s.split_whitespace().next().unwrap_or("");
    let name = first
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(first)
        .trim_matches('"');
    if name == "aiclean" || name == "aiclean.exe" || name == "ps" {
        return false;
    }
    [
        "claude.app/",
        "codex.app/",
        "claude.exe",
        "codex.exe",
        "@anthropic-ai/claude-code",
        "@openai/codex",
        "visual studio code",
        "code helper",
        "cursor.app/",
        "cursor.exe",
        "windsurf.app/",
        "windsurf.exe",
        "zed.app/",
        "zed.exe",
        "vscodium",
        "antigravity",
        "intellij idea",
        "pycharm",
        "webstorm",
        "idea64.exe",
        "code -",
        "code.exe",
    ]
    .iter()
    .any(|needle| s.contains(needle))
        || matches!(
            name,
            "claude" | "codex" | "code" | "cursor" | "windsurf" | "zed"
        )
        || s.split_whitespace().any(|part| {
            let name = part
                .trim_matches(['"', '\''])
                .rsplit(['/', '\\'])
                .next()
                .unwrap_or(part);
            matches!(name, "claude" | "claude.js" | "codex" | "codex.js")
        })
}
#[cfg(any(target_os = "macos", test))]
fn process_command(line: &str, current_pid: u32) -> Result<Option<&str>> {
    let line = line.trim();
    let (pid, command) = line
        .split_once(char::is_whitespace)
        .context("Process inventory entry cannot be verified")?;
    let pid = pid
        .parse::<u32>()
        .context("Process identity cannot be verified")?;
    if pid == current_pid {
        return Ok(None);
    }
    let command = command.trim_start();
    if command.is_empty() {
        bail!("Process command cannot be verified");
    }
    Ok(Some(command))
}
#[cfg(any(target_os = "windows", test))]
fn windows_inventory_busy(text: &str, provider: Provider) -> Result<bool> {
    #[derive(serde::Deserialize)]
    struct Inventory {
        helper_pid: u32,
        processes: Vec<Process>,
    }
    #[derive(serde::Deserialize)]
    struct Process {
        pid: u32,
        name: String,
        command: Option<String>,
    }
    let inventory: Inventory =
        serde_json::from_str(text).context("Process inventory could not be verified")?;
    if inventory.helper_pid == 0 || inventory.processes.is_empty() {
        bail!("Process inventory is incomplete");
    }
    for process in inventory.processes {
        if process.pid == inventory.helper_pid || process.pid == std::process::id() {
            continue;
        }
        let name = process.name.to_ascii_lowercase();
        if process.command.is_none()
            && matches!(
                name.as_str(),
                "node.exe" | "electron.exe" | "claude.exe" | "codex.exe"
            )
        {
            return Ok(true);
        }
        if process_is_busy(
            &format!("{} {}", process.name, process.command.unwrap_or_default()),
            provider,
        ) {
            return Ok(true);
        }
    }
    Ok(false)
}
pub fn ensure_quiet(provider: Provider) -> Result<()> {
    #[cfg(target_os = "macos")]
    let output = run_command(
        "/bin/ps",
        &["-ww".into(), "-axo".into(), "pid=,comm=,args=".into()],
        &[],
    )?;
    #[cfg(target_os = "windows")]
    let output = powershell(
        r"@{helper_pid=$PID;processes=@(Get-CimInstance Win32_Process | Where-Object { $_.ProcessId -ne $PID } | ForEach-Object {@{pid=$_.ProcessId;name=$_.Name;command=$_.CommandLine}})} | ConvertTo-Json -Depth 4 -Compress",
        &[],
    )?;
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        let _ = provider;
        bail!("Cleanup supports only native macOS and Windows");
    }
    #[cfg(any(target_os = "macos", target_os = "windows"))]
    {
        if !output.success || output.stdout.trim().is_empty() {
            bail!("Process state could not be verified; cleanup refused");
        }
        #[cfg(target_os = "windows")]
        if windows_inventory_busy(&output.stdout, provider)? {
            bail!(
                "Conversation application, CLI, or code host is running; close it before mutation"
            );
        }
        #[cfg(target_os = "macos")]
        for line in output.stdout.lines().filter(|line| !line.trim().is_empty()) {
            #[cfg(target_os = "macos")]
            let command = match process_command(line, std::process::id())? {
                Some(command) => command,
                None => continue,
            };
            if process_is_busy(command, provider) {
                bail!(
                    "Conversation application, CLI, or code host is running; close it before mutation"
                );
            }
        }
        Ok(())
    }
}
pub fn purge_status() -> Result<Value> {
    #[cfg(target_os = "macos")]
    {
        let output = run_command(
            "/usr/bin/defaults",
            &[
                "read".into(),
                "com.apple.finder".into(),
                "FXRemoveOldTrashItems".into(),
            ],
            &[],
        )?;
        let configured = if output.success {
            match output.stdout.trim() {
                "1" | "true" => Some(true),
                "0" | "false" => Some(false),
                _ => None,
            }
        } else {
            None
        };
        Ok(
            json!({"os":"macos","mechanism":"Finder","configured":configured,"enabled":configured,"days":if configured==Some(true){Some(30)}else{None},"verified":configured.is_some(),"setting":"FXRemoveOldTrashItems","note":"OS performs purge; aiClean never purges Trash"}),
        )
    }
    #[cfg(target_os = "windows")]
    {
        // Only the documented policy is accepted. Unspecified or unverified user settings fail closed.
        let output = powershell(
            r#"$p=Get-ItemProperty -LiteralPath 'HKLM:\Software\Policies\Microsoft\Windows\StorageSense' -ErrorAction SilentlyContinue; if ($null -eq $p) { @{configured=$null;enabled=$null;days=$null;verified=$false} | ConvertTo-Json -Compress } else { $enabled=$p.AllowStorageSenseGlobal; $days=$p.ConfigStorageSenseRecycleBinCleanupThreshold; $cadence=$p.ConfigStorageSenseGlobalCadence; @{configured=($enabled -eq 1 -and $days -eq 30 -and $cadence -in @(1,7,30));enabled=$enabled;days=$days;cadence=$cadence;verified=($null -ne $enabled -and $null -ne $days -and $null -ne $cadence);source='documented StorageSense policy'} | ConvertTo-Json -Compress }"#,
            &[],
        )?;
        if !output.success {
            return Ok(json!({"os":"windows","configured":null,"verified":false}));
        }
        let mut value: Value = serde_json::from_str(output.stdout.trim())
            .context("Cannot verify Storage Sense policy")?;
        value["os"] = json!("windows");
        Ok(value)
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    Ok(json!({"os":std::env::consts::OS,"configured":null,"verified":false,"supported":false}))
}
pub fn ensure_purge_enabled() -> Result<()> {
    let s = purge_status()?;
    if s["configured"] != true {
        bail!(
            "OS 30-day automatic Trash purge is not verifiably enabled; configure it in OS settings first"
        );
    }
    Ok(())
}
pub fn ensure_headroom(path: &Path, additional_bytes: u64) -> Result<()> {
    let required = additional_bytes
        .checked_add(8 * 1024 * 1024 * 1024)
        .context("Storage estimate overflow")?;
    if fs2::available_space(path)? < required {
        bail!("Cleanup requires 8 GiB free space after the estimated recoverable copy");
    }
    Ok(())
}
#[cfg(any(target_os = "windows", test))]
fn system_drive_matches(path: &str, windows_directory: &str) -> bool {
    fn drive(path: &str) -> Option<u8> {
        let path = path.strip_prefix("\\\\?\\").unwrap_or(path);
        let bytes = path.as_bytes();
        if bytes.len() < 3
            || !bytes[0].is_ascii_alphabetic()
            || bytes[1] != b':'
            || !matches!(bytes[2], b'\\' | b'/')
        {
            return None;
        }
        Some(bytes[0].to_ascii_uppercase())
    }
    let expected = drive(windows_directory);
    expected.is_some() && drive(path) == expected
}
/// Windows Storage Sense is only documented to purge the Windows system drive.
pub fn ensure_trash_purge_supported(path: &Path) -> Result<()> {
    #[cfg(target_os = "windows")]
    {
        #[link(name = "kernel32")]
        unsafe extern "system" {
            fn GetWindowsDirectoryW(buffer: *mut u16, size: u32) -> u32;
        }
        let mut buffer = vec![0u16; 32768];
        let length = unsafe { GetWindowsDirectoryW(buffer.as_mut_ptr(), buffer.len() as u32) };
        if length == 0 || length as usize >= buffer.len() {
            bail!("Windows system drive cannot be verified");
        }
        let directory = String::from_utf16(&buffer[..length as usize])
            .context("Unknown Windows system directory encoding")?;
        if !system_drive_matches(
            path.to_str().context("Unknown Trash path encoding")?,
            &directory,
        ) {
            bail!(
                "Storage Sense only verifies automatic Trash purge on the Windows system drive; other drives refused"
            );
        }
    }
    #[cfg(not(target_os = "windows"))]
    let _ = path;
    Ok(())
}
/// Refuse shared inode/file records before editing a provider's regular source file.
pub fn ensure_single_link(path: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.is_file() || metadata.file_type().is_symlink() {
        bail!("Link-count verification requires a regular source file");
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if metadata.nlink() != 1 {
            bail!("Hardlinked source file refused");
        }
    }
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::{fs::MetadataExt, io::AsRawHandle};
        use windows::Win32::{
            Foundation::HANDLE,
            Storage::FileSystem::{BY_HANDLE_FILE_INFORMATION, GetFileInformationByHandle},
        };
        if metadata.file_attributes() & 0x400 != 0 {
            bail!("Reparse-point source file refused");
        }
        let file = fs::File::open(path)?;
        let mut information = BY_HANDLE_FILE_INFORMATION::default();
        unsafe { GetFileInformationByHandle(HANDLE(file.as_raw_handle()), &mut information) }
            .context("Native source link count unavailable")?;
        if information.nNumberOfLinks != 1 {
            bail!("Hardlinked source file refused");
        }
    }
    #[cfg(not(any(unix, target_os = "windows")))]
    bail!("Source link count cannot be verified on this OS");
    Ok(())
}
/// Establish privacy for an aiClean-owned directory before writing raw conversation data.
pub fn ensure_private_directory(path: &Path) -> Result<()> {
    crate::schedule::safe_path(path)?;
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        bail!("Private storage must be a regular directory");
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    }
    #[cfg(target_os = "windows")]
    {
        let result = powershell(
            r#"$sid=[Security.Principal.WindowsIdentity]::GetCurrent().User; $acl=New-Object Security.AccessControl.DirectorySecurity; $acl.SetOwner($sid); $acl.SetAccessRuleProtection($true,$false); foreach($principal in @($sid,(New-Object Security.Principal.SecurityIdentifier('S-1-5-18')))){$rule=New-Object Security.AccessControl.FileSystemAccessRule($principal,'FullControl','ContainerInherit,ObjectInherit','None','Allow');$acl.AddAccessRule($rule)}; Set-Acl -LiteralPath $env:AICLEAN_PRIVATE_DIR -AclObject $acl"#,
            &[("AICLEAN_PRIVATE_DIR".into(), path.as_os_str().to_owned())],
        )?;
        if !result.success {
            bail!("Private storage ACL could not be established");
        }
    }
    #[cfg(not(any(unix, target_os = "windows")))]
    bail!("Private storage access cannot be verified on this OS");
    Ok(())
}
/// Atomically replace an owned durable receipt. Caller must flush the source file first.
pub fn exclusive_restore_file(source: &Path, destination: &Path) -> Result<()> {
    crate::schedule::safe_path(source)?;
    crate::schedule::safe_path(destination)?;
    if source.parent() != destination.parent() {
        bail!("Exclusive restore must remain in one directory");
    }
    ensure_single_link(source)?;
    #[cfg(target_os = "macos")]
    {
        use std::{ffi::CString, os::unix::ffi::OsStrExt};
        unsafe extern "C" {
            fn renamex_np(
                from: *const std::ffi::c_char,
                to: *const std::ffi::c_char,
                flags: u32,
            ) -> i32;
        }
        let from = CString::new(source.as_os_str().as_bytes())?;
        let to = CString::new(destination.as_os_str().as_bytes())?;
        if unsafe { renamex_np(from.as_ptr(), to.as_ptr(), 4) } != 0 {
            return Err(std::io::Error::last_os_error()).context("Exclusive native restore failed");
        }
        fs::File::open(destination.parent().context("Restore has no parent")?)?.sync_all()?;
    }
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::ffi::OsStrExt;
        #[link(name = "kernel32")]
        unsafe extern "system" {
            fn MoveFileExW(existing: *const u16, new: *const u16, flags: u32) -> i32;
        }
        let wide = |p: &Path| -> Result<Vec<u16>> {
            let mut value = p.as_os_str().encode_wide().collect::<Vec<_>>();
            if value.contains(&0) {
                bail!("NUL in restore path");
            }
            value.push(0);
            Ok(value)
        };
        let from = wide(source)?;
        let to = wide(destination)?;
        // WRITE_THROUGH only: no REPLACE_EXISTING and no copy/delete between volumes.
        if unsafe { MoveFileExW(from.as_ptr(), to.as_ptr(), 8) } == 0 {
            return Err(std::io::Error::last_os_error()).context("Exclusive native restore failed");
        }
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    bail!("Exclusive restore supports only native macOS and Windows");
    #[cfg(any(target_os = "macos", target_os = "windows"))]
    Ok(())
}
/// Atomically replace an owned durable receipt. Caller must flush the source file first.
pub fn durable_replace(source: &Path, destination: &Path) -> Result<()> {
    crate::schedule::safe_path(source)?;
    crate::schedule::safe_path(destination)?;
    if source.parent() != destination.parent() {
        bail!("Durable replacement must remain within one owned directory");
    }
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::ffi::OsStrExt;
        #[link(name = "kernel32")]
        unsafe extern "system" {
            fn MoveFileExW(existing: *const u16, new: *const u16, flags: u32) -> i32;
        }
        let wide = |p: &Path| -> Result<Vec<u16>> {
            let mut value = p.as_os_str().encode_wide().collect::<Vec<_>>();
            if value.contains(&0) {
                bail!("NUL in durable receipt path");
            }
            value.push(0);
            Ok(value)
        };
        let source = wide(source)?;
        let destination = wide(destination)?;
        // MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH. Never copy between volumes.
        if unsafe { MoveFileExW(source.as_ptr(), destination.as_ptr(), 0x1 | 0x8) } == 0 {
            return Err(std::io::Error::last_os_error())
                .context("Durable native receipt replacement failed");
        }
    }
    #[cfg(not(target_os = "windows"))]
    {
        fs::rename(source, destination)?;
        fs::File::open(destination.parent().context("Receipt has no parent")?)?.sync_all()?;
    }
    Ok(())
}
#[cfg(any(target_os = "macos", test))]
fn original_matches(bytes: &[u8], path: &Path) -> bool {
    path.to_str().is_some_and(|p| p.as_bytes() == bytes)
}
fn one_match(mut matches: Vec<PathBuf>) -> Result<Option<PathBuf>> {
    if matches.len() > 1 {
        bail!("Ambiguous native Trash metadata; recovery refused");
    }
    Ok(matches.pop())
}
fn absolute_original(path: &Path) -> Result<PathBuf> {
    if !path.is_absolute() {
        bail!("Trash requires an absolute original path");
    }
    if path
        .components()
        .any(|c| matches!(c, std::path::Component::ParentDir))
    {
        bail!("Unsafe Trash path");
    }
    Ok(path.to_path_buf())
}
#[cfg(target_os = "macos")]
const ORIGINAL_XATTR: &str = "local.aiClean.originalPath";
#[cfg(target_os = "macos")]
fn mac_url(path: &Path) -> Result<objc2::rc::Retained<objc2_foundation::NSURL>> {
    Ok(objc2_foundation::NSURL::fileURLWithPath(
        &objc2_foundation::NSString::from_str(
            path.to_str()
                .context("Native Trash requires a UTF-8 path")?,
        ),
    ))
}
#[cfg(target_os = "macos")]
fn mac_trash_root(original: &Path) -> Result<PathBuf> {
    use objc2_foundation::{NSFileManager, NSSearchPathDirectory, NSSearchPathDomainMask};
    // The original may already be gone after a crash; its parent remains on the same volume.
    let parent = original.parent().context("Trash original has no parent")?;
    let parent_url = mac_url(parent)?;
    let url = NSFileManager::defaultManager()
        .URLForDirectory_inDomain_appropriateForURL_create_error(
            NSSearchPathDirectory::TrashDirectory,
            NSSearchPathDomainMask::UserDomainMask,
            Some(&parent_url),
            false,
        )
        .map_err(|_| anyhow::anyhow!("Native Trash location unavailable"))?;
    Ok(PathBuf::from(
        url.path()
            .context("Native Trash URL has no path")?
            .to_string(),
    ))
}
/// Locate a native Trash entry using original-path metadata; do not assume the OS preserves names.
pub fn find_trash(original_bundle: &Path) -> Result<Option<PathBuf>> {
    let original = absolute_original(original_bundle)?;
    #[cfg(target_os = "macos")]
    {
        let root = mac_trash_root(&original)?;
        mac_find_metadata(&root, &original)
    }
    #[cfg(target_os = "windows")]
    {
        let dos = windows_dos_original(
            original
                .to_str()
                .context("Native Trash path encoding unknown")?,
        )?;
        let output = powershell(
            WINDOWS_FIND_TRASH,
            &[("AICLEAN_ORIGINAL".into(), dos.into())],
        )?;
        if !output.success {
            bail!("Native Recycle Bin lookup failed");
        }
        let matches: Vec<PathBuf> = serde_json::from_str(output.stdout.trim())
            .context("Invalid native Recycle Bin receipt")?;
        one_match(matches)
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        let _ = original;
        bail!("Native Trash unsupported on this OS");
    }
}
#[cfg(target_os = "macos")]
fn mac_find_metadata(root: &Path, original: &Path) -> Result<Option<PathBuf>> {
    crate::schedule::safe_path(root)?;
    let known = root.join(
        original
            .file_name()
            .context("Original bundle has no filename")?,
    );
    let direct = mac_metadata_matches(&known, original)?;
    let mut matches = if direct {
        vec![known.clone()]
    } else {
        Vec::new()
    };
    match fs::read_dir(root) {
        Ok(entries) => {
            for entry in entries {
                let path = entry?.path();
                if path == known {
                    continue;
                }
                let metadata = fs::symlink_metadata(&path)?;
                if !metadata.is_dir() || metadata.file_type().is_symlink() {
                    continue;
                }
                if let Some(bytes) = xattr::get(&path, ORIGINAL_XATTR)?
                    && original_matches(&bytes, original)
                {
                    matches.push(path);
                }
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        // macOS can allow access to a known item while disallowing listing the Trash directory.
        // Before the first move, an existing source proves the bundle has not yet entered Trash.
        Err(e)
            if e.raw_os_error() == Some(1) || e.kind() == std::io::ErrorKind::PermissionDenied =>
        {
            if direct {
                return Ok(Some(known));
            }
            if original.exists() {
                return Ok(None);
            }
            return Err(e).context("Cannot locate unacknowledged native Trash bundle; original sessions were not yet mutated");
        }
        Err(e) => return Err(e).context("Cannot inspect native Trash"),
    }
    one_match(matches)
}
#[cfg(target_os = "macos")]
fn mac_metadata_matches(path: &Path, original: &Path) -> Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(m) => {
            if !m.is_dir() || m.file_type().is_symlink() {
                bail!("Native Trash entry is not a safe bundle directory");
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(e).context("Cannot verify known native Trash entry"),
    }
    Ok(xattr::get(path, ORIGINAL_XATTR)?.is_some_and(|b| original_matches(&b, original)))
}
pub fn trash(path: &Path) -> Result<PathBuf> {
    let original = absolute_original(path)?;
    ensure_trash_purge_supported(&original)?;
    crate::schedule::safe_path(&original)?;
    let metadata = fs::symlink_metadata(&original)?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        bail!("Only regular bundle directories can enter Trash");
    }
    if find_trash(&original)
        .context("Native Trash pre-move lookup failed")?
        .is_some()
    {
        bail!("Original bundle already has a native Trash receipt");
    }
    #[cfg(target_os = "macos")]
    {
        xattr::set(
            &original,
            ORIGINAL_XATTR,
            original
                .to_str()
                .context("Unsupported original path")?
                .as_bytes(),
        )?;
        fs::File::open(&original)?.sync_all()?;
        let mut resulting = None;
        let source_url = mac_url(&original)?;
        objc2_foundation::NSFileManager::defaultManager()
            .trashItemAtURL_resultingItemURL_error(&source_url, Some(&mut resulting))
            .map_err(|_| anyhow::anyhow!("Native Trash move failed; recover before retrying"))?;
        let actual = PathBuf::from(
            resulting
                .context("Native Trash did not return its actual URL; recover operation")?
                .path()
                .context("Native Trash result lacks path")?
                .to_string(),
        );
        crate::schedule::safe_path(&actual)?;
        if !mac_metadata_matches(&actual, &original)? {
            bail!("Native Trash receipt did not match actual recoverable bundle");
        }
        Ok(actual)
    }
    #[cfg(target_os = "windows")]
    {
        windows_recycle(&original)?;
        find_trash(&original)
            .context("Native Recycle Bin post-move receipt lookup failed")?
            .context("Native Recycle Bin receipt unavailable; recover operation")
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    bail!("Native Trash unsupported on this OS")
}
#[cfg(target_os = "windows")]
fn windows_recycle(path: &Path) -> Result<()> {
    use windows::{
        Win32::{
            System::Com::{
                CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED, COINIT_DISABLE_OLE1DDE,
                CoCreateInstance, CoInitializeEx, CoUninitialize,
            },
            UI::Shell::{
                FOF_NOERRORUI, FOF_SILENT, FOFX_ADDUNDORECORD, FOFX_EARLYFAILURE,
                FOFX_RECYCLEONDELETE, FileOperation, IFileOperation, IShellItem,
                SHCreateItemFromParsingName,
            },
        },
        core::PCWSTR,
    };
    let dos = windows_dos_original(
        path.to_str()
            .context("Native Trash path encoding unknown")?,
    )?;
    let mut wide = dos.encode_utf16().collect::<Vec<_>>();
    if wide.contains(&0) {
        bail!("NUL in native Recycle Bin path");
    }
    wide.push(0);
    // Windows 8+ strict recycle-only operation. No permanent-delete fallback and no dialogs.
    unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED | COINIT_DISABLE_OLE1DDE).ok() }
        .context("Native shell apartment could not initialize")?;
    struct Apartment;
    impl Drop for Apartment {
        fn drop(&mut self) {
            unsafe { CoUninitialize() };
        }
    }
    let _apartment = Apartment;
    unsafe {
        let operation: IFileOperation =
            CoCreateInstance(&FileOperation, None, CLSCTX_INPROC_SERVER)
                .context("Native recycle operation unavailable")?;
        operation
            .SetOperationFlags(
                FOFX_RECYCLEONDELETE
                    | FOFX_ADDUNDORECORD
                    | FOFX_EARLYFAILURE
                    | FOF_NOERRORUI
                    | FOF_SILENT,
            )
            .context("Strict native recycle flags unavailable")?;
        let item: IShellItem = SHCreateItemFromParsingName(PCWSTR(wide.as_ptr()), None)
            .context("Native recycle item unavailable")?;
        operation
            .DeleteItem(&item, None)
            .context("Native recycle operation refused")?;
        operation
            .PerformOperations()
            .context("Native recycle operation failed; recover before retry")?;
        if operation.GetAnyOperationsAborted()?.as_bool() {
            bail!("Native recycle operation aborted; recover before retry");
        }
    }
    Ok(())
}
#[cfg(any(target_os = "windows", test))]
fn windows_dos_original(path: &str) -> Result<String> {
    let path = path.strip_prefix(r"\\?\").unwrap_or(path);
    let bytes = path.as_bytes();
    if bytes.len() < 3 || !bytes[0].is_ascii_alphabetic() || &bytes[1..3] != b":\\" {
        bail!("Native Trash original is not an absolute local drive path");
    }
    if path.chars().any(|c| c < ' ' || c == '/') || path[2..].contains(':') {
        bail!("Ambiguous Windows path or alternate data stream refused");
    }
    for component in path[3..].split('\\') {
        if component.is_empty()
            || component == "."
            || component == ".."
            || component.ends_with(['.', ' '])
        {
            bail!("Ambiguous Windows path component");
        }
        let base = component
            .split('.')
            .next()
            .unwrap_or("")
            .to_ascii_uppercase();
        if matches!(base.as_str(), "CON" | "PRN" | "AUX" | "NUL")
            || ["COM", "LPT"].iter().any(|prefix| {
                base.strip_prefix(prefix).is_some_and(|number| {
                    matches!(
                        number,
                        "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9" | "¹" | "²" | "³"
                    )
                })
            })
        {
            bail!("DOS device name refused in native Trash path");
        }
    }
    Ok(path.to_owned())
}
#[cfg(target_os = "windows")]
const WINDOWS_FIND_TRASH: &str = r#"
$original=[IO.Path]::GetFullPath($env:AICLEAN_ORIGINAL); $drive=[IO.Path]::GetPathRoot($original); $sid=[Security.Principal.WindowsIdentity]::GetCurrent().User.Value; $root=Join-Path (Join-Path $drive '$Recycle.Bin') $sid; $matches=@();
if(Test-Path -LiteralPath $root){ foreach($i in @(Get-ChildItem -LiteralPath $root -Filter '$I*' -Force -File)) { $b=[IO.File]::ReadAllBytes($i.FullName); if($b.Length -lt 26){throw 'Invalid Recycle Bin receipt'}; $version=[BitConverter]::ToInt64($b,0); if($version -eq 1){$offset=24;$len=$b.Length-24} elseif($version -eq 2){if($b.Length -lt 28){throw 'Invalid receipt'};$offset=28;$len=[BitConverter]::ToInt32($b,24)*2;if($len -lt 0 -or $offset+$len -gt $b.Length){throw 'Invalid receipt'}} else {throw 'Unsupported Recycle Bin receipt'}; $old=[Text.Encoding]::Unicode.GetString($b,$offset,$len).TrimEnd([char]0); if([String]::Equals($old,$original,[StringComparison]::OrdinalIgnoreCase)){ $r=Join-Path $root ('$R'+$i.Name.Substring(2)); if(Test-Path -LiteralPath $r -PathType Container){if((Get-Item -LiteralPath $r -Force).Attributes -band [IO.FileAttributes]::ReparsePoint){throw 'Unsafe recycle entry'}; $matches+= $r} } } }; ConvertTo-Json -InputObject @($matches) -Compress
"#;

#[cfg(test)]
mod tests {
    use super::*;
    fn windows_fixture_receipt_original(bytes: &[u8]) -> Result<String> {
        if bytes.len() < 28 || bytes.len() > 65564 {
            bail!("Unsafe native receipt size");
        }
        let version = u64::from_le_bytes(bytes[..8].try_into()?);
        let (offset, length): (usize, usize) = match version {
            1 => (24, bytes.len() - 24),
            2 => {
                let chars = u32::from_le_bytes(bytes[24..28].try_into()?) as usize;
                (28, chars.checked_mul(2).context("Receipt length overflow")?)
            }
            _ => bail!("Unknown native receipt version"),
        };
        let end = offset
            .checked_add(length)
            .context("Receipt length overflow")?;
        if end > bytes.len() || length % 2 != 0 {
            bail!("Invalid native receipt path length");
        }
        let wide = bytes[offset..end]
            .chunks_exact(2)
            .map(|c| u16::from_le_bytes([c[0], c[1]]))
            .collect::<Vec<_>>();
        let path = String::from_utf16(&wide).context("Invalid native receipt Unicode")?;
        let path = path.trim_end_matches('\0');
        if path.contains('\0') {
            bail!("Embedded NUL in native receipt");
        }
        Ok(path.to_owned())
    }
    #[cfg(target_os = "windows")]
    fn cleanup_owned_recycle_receipt(
        actual: &Path,
        original: &Path,
        fixture_root: &Path,
    ) -> Result<()> {
        if original.parent() != Some(fixture_root)
            || !fixture_root
                .file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with(".aiClean-native-fixture-"))
        {
            bail!("Not this exclusively-created fixture root");
        }
        let uuid = original
            .file_name()
            .and_then(|n| n.to_str())
            .context("Fixture UUID unavailable")?;
        if uuid::Uuid::parse_str(uuid)?.to_string() != uuid {
            bail!("Not an exact fixture UUID basename");
        }
        if actual.exists() || fs::read(original.join("fixture.txt"))? != b"recoverable fixture" {
            bail!("Fixture has not been restored intact");
        }
        let name = actual
            .file_name()
            .and_then(|n| n.to_str())
            .context("Native recycle basename unavailable")?;
        let suffix = name
            .strip_prefix("$R")
            .context("Not a native recycle data directory")?;
        if suffix.is_empty() {
            bail!("Invalid native recycle basename");
        }
        let parent = actual
            .parent()
            .context("Native recycle parent unavailable")?;
        if parent.parent().and_then(Path::file_name) != Some(std::ffi::OsStr::new("$Recycle.Bin")) {
            bail!("Not a native Recycle Bin receipt parent");
        }
        let receipt = parent.join(format!("$I{suffix}"));
        crate::schedule::safe_path(&receipt)?;
        ensure_single_link(&receipt)?;
        let size = fs::metadata(&receipt)?.len();
        if size > 65564 {
            bail!("Unsafe native receipt size");
        }
        let old = windows_fixture_receipt_original(&fs::read(&receipt)?)?;
        let expected =
            windows_dos_original(original.to_str().context("Fixture path encoding unknown")?)?;
        if !windows_dos_original(&old)?.eq_ignore_ascii_case(&expected) {
            bail!("Native receipt original does not match this fixture UUID path");
        }
        fs::remove_file(&receipt)?;
        Ok(())
    }
    #[test]
    fn fixture_original_normalization_preserves_unicode_and_refuses_ads_or_aliases() {
        assert_eq!(
            windows_dos_original(r"\\?\C:\Users\ユーザー\uuid").unwrap(),
            r"C:\Users\ユーザー\uuid"
        );
        for path in [
            r"C:\root\uuid:stream",
            r"C:\root:stream\uuid",
            r"C:relative",
            r"\\?\C:\root.\uuid",
            r"\\?\C:\root \uuid",
            r"C:\root\..\uuid",
            r"C:\nul.txt\uuid",
            r"\\.\C:\root\uuid",
        ] {
            assert!(windows_dos_original(path).is_err(), "{path}");
        }
    }
    #[test]
    fn fixture_receipt_requires_valid_version_length_and_utf16() {
        let old = r"C:\Users\owner\unique-uuid";
        let wide = old.encode_utf16().chain(Some(0)).collect::<Vec<_>>();
        let mut bytes = vec![0u8; 28];
        bytes[..8].copy_from_slice(&2u64.to_le_bytes());
        bytes[24..28].copy_from_slice(&(wide.len() as u32).to_le_bytes());
        for value in wide {
            bytes.extend(value.to_le_bytes());
        }
        assert_eq!(windows_fixture_receipt_original(&bytes).unwrap(), old);
        bytes[24..28].copy_from_slice(&u32::MAX.to_le_bytes());
        assert!(windows_fixture_receipt_original(&bytes).is_err());
    }
    #[test]
    fn windows_inventory_ignores_helper_command_containing_provider_names() {
        let helper=json!({"helper_pid":42,"processes":[{"pid":42,"name":"powershell.exe","command":"-Command script contains claude.exe"},{"pid":43,"name":"explorer.exe","command":null}]}).to_string();
        assert!(!windows_inventory_busy(&helper, Provider::Codex).unwrap());
        let cli=json!({"helper_pid":42,"processes":[{"pid":43,"name":"node.exe","command":"node /opt/@openai/codex/bin/codex.js"}]}).to_string();
        assert!(windows_inventory_busy(&cli, Provider::Codex).unwrap());
        assert!(windows_inventory_busy("partial inventory", Provider::Codex).is_err());
    }
    #[test]
    fn process_inventory_excludes_self_even_binary_path_has_spaces() {
        let line = "42 /Users/a b/aiClean /Users/a b/aiClean run --scheduled codex";
        assert_eq!(process_command(line, 42).unwrap(), None);
        assert!(process_command("unverifiable inventory", 42).is_err());
        assert!(process_is_busy(
            process_command("43 node /opt/@openai/codex/bin/codex.js", 42)
                .unwrap()
                .unwrap(),
            Provider::Codex
        ));
    }
    #[test]
    fn storage_sense_scope_refuses_other_drives_and_unc_paths() {
        assert!(system_drive_matches(
            "C:\\Users\\ユーザー\\.aiClean",
            "C:\\Windows"
        ));
        assert!(system_drive_matches(
            "\\\\?\\c:\\Users\\owner",
            "C:\\Windows"
        ));
        assert!(!system_drive_matches("D:\\state", "C:\\Windows"));
        assert!(!system_drive_matches(
            "\\\\server\\share\\state",
            "C:\\Windows"
        ));
        assert!(!system_drive_matches("C:relative", "C:\\Windows"));
    }
    #[test]
    #[cfg(any(target_os = "macos", target_os = "windows"))]
    fn exclusive_restore_refuses_existing_destination_and_leaves_one_link() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let source = root.join("temp");
        let dest = root.join("restored");
        fs::write(&source, b"restore").unwrap();
        fs::write(&dest, b"existing").unwrap();
        assert!(exclusive_restore_file(&source, &dest).is_err());
        assert_eq!(fs::read(&dest).unwrap(), b"existing");
        assert!(source.exists());
        fs::remove_file(&dest).unwrap();
        exclusive_restore_file(&source, &dest).unwrap();
        assert!(!source.exists());
        ensure_single_link(&dest).unwrap();
    }
    #[test]
    fn hardlinked_regular_files_are_refused() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let file = root.join("database.sqlite");
        fs::write(&file, b"database fixture").unwrap();
        ensure_single_link(&file).unwrap();
        fs::hard_link(&file, root.join("outside-link.sqlite")).unwrap();
        assert!(ensure_single_link(&file).is_err());
    }
    #[test]
    #[cfg(any(target_os = "macos", target_os = "windows"))]
    #[ignore = "Native Trash fixture integration, requires OS Trash permission"]
    fn native_trash_fixture_has_actual_recoverable_path() {
        let home = dirs::home_dir().unwrap();
        let temp = tempfile::Builder::new()
            .prefix(".aiClean-native-fixture-")
            .tempdir_in(home)
            .unwrap();
        let root = temp.path().canonicalize().unwrap();
        let original = root.join(uuid::Uuid::new_v4().to_string());
        fs::create_dir(&original).unwrap();
        fs::write(original.join("fixture.txt"), b"recoverable fixture").unwrap();
        let member = original.join("fixture.txt");
        let original_permissions = fs::metadata(&member).unwrap().permissions();
        let mut frozen_permissions = original_permissions.clone();
        frozen_permissions.set_readonly(true);
        fs::set_permissions(&member, frozen_permissions).unwrap();
        eprintln!(
            "aiClean native Trash fixture original: {}",
            original.display()
        );
        let actual = trash(&original).unwrap_or_else(|error| {
            eprintln!(
                "aiClean native Trash fixture original_present_after_error: {}",
                original.exists()
            );
            if error.to_string().contains("pre-move") {
                assert!(
                    original.exists(),
                    "Pre-move failure must preserve fixture bundle"
                );
            }
            panic!("{error:#}");
        });
        assert!(!original.exists());
        assert_eq!(find_trash(&original).unwrap(), Some(actual.clone()));
        assert_eq!(
            fs::read(actual.join("fixture.txt")).unwrap(),
            b"recoverable fixture"
        );
        assert!(
            fs::metadata(actual.join("fixture.txt"))
                .unwrap()
                .permissions()
                .readonly()
        );
        // Restore this exclusively-created fixture before TempDir removes it. Never purge Trash.
        fs::rename(&actual, &original).unwrap();
        #[cfg(target_os = "windows")]
        cleanup_owned_recycle_receipt(&actual, &original, &root).unwrap();
        assert_eq!(find_trash(&original).unwrap(), None);
        // Only this restored fixture member loses its test-only readonly bit for TempDir cleanup.
        fs::set_permissions(&member, original_permissions).unwrap();
    }
    #[test]
    #[cfg(target_os = "macos")]
    fn native_metadata_recovers_renamed_bundle_and_refuses_duplicates() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let original = root.join("original-uuid");
        let renamed = root.join("os-renamed-bundle");
        fs::create_dir(&renamed).unwrap();
        xattr::set(
            &renamed,
            ORIGINAL_XATTR,
            original.to_str().unwrap().as_bytes(),
        )
        .unwrap();
        assert_eq!(mac_find_metadata(&root, &original).unwrap(), Some(renamed));
        let duplicate = root.join("duplicate-receipt");
        fs::create_dir(&duplicate).unwrap();
        xattr::set(
            &duplicate,
            ORIGINAL_XATTR,
            original.to_str().unwrap().as_bytes(),
        )
        .unwrap();
        assert!(mac_find_metadata(&root, &original).is_err());
    }
    #[test]
    fn process_detection_includes_apps_node_cli_and_code_hosts() {
        for command in [
            "/Applications/Codex.app/Contents/MacOS/Codex",
            "node /usr/lib/node_modules/@openai/codex/bin/codex.js",
            "/Applications/Visual Studio Code.app/Contents/MacOS/Electron",
            "node /tmp/node_modules/@anthropic-ai/claude-code/cli.js",
        ] {
            assert!(process_is_busy(command, Provider::Codex), "{command}");
        }
        assert!(!process_is_busy(
            "/usr/bin/ps -axo comm=,args=",
            Provider::Codex
        ));
        assert!(!process_is_busy(
            "/usr/local/bin/aiClean run codex",
            Provider::Codex
        ));
    }
    #[test]
    fn original_metadata_matches_renamed_trash_entry() {
        assert!(original_matches(
            b"/tmp/unique-uuid",
            Path::new("/tmp/unique-uuid")
        ));
        assert!(!original_matches(
            b"/tmp/other",
            Path::new("/tmp/unique-uuid")
        ));
    }
    #[test]
    fn ambiguous_recovery_refuses() {
        assert!(one_match(vec![PathBuf::from("one"), PathBuf::from("two")]).is_err());
        assert_eq!(one_match(vec![]).unwrap(), None);
    }
}
