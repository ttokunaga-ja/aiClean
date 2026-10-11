//! Explicitly confirmed provider shutdown before planning. Never display process arguments.
use crate::model::Provider;
use anyhow::{Context, Result, bail};

#[derive(Clone, Debug, PartialEq, Eq)]
struct Process {
    pid: u32,
    parent: u32,
    started: String,
    name: String,
    command: Option<String>,
}
fn same_identity(a: &Process, b: &Process) -> bool {
    a.pid == b.pid && a.started == b.started && a.name == b.name
}
fn validate_approved(now: &[Process], approved: &[Process]) -> Result<()> {
    if now.iter().any(|p| {
        approved
            .iter()
            .any(|a| p.pid == a.pid && !same_identity(p, a))
    }) {
        bail!("Confirmed process identity changed or became unavailable; mutation refused");
    }
    Ok(())
}
trait Backend {
    fn inventory(&mut self) -> Result<Vec<Process>>;
    fn signal(&mut self, process: &Process, force: bool) -> Result<()>;
    fn wait(&mut self);
}
// Parse only two unambiguous argv tokens. Flattened OS command strings cannot
// safely recover unquoted paths with spaces; those remain the quiet guard's refusal.
fn first_two_arguments(command: &str) -> Option<[String; 2]> {
    let mut rest = command.trim_start();
    let mut arguments = Vec::new();
    for _ in 0..2 {
        let (argument, next) = if rest.starts_with(['"', '\'']) {
            let quote = rest.chars().next()?;
            let end = rest[1..].find(quote)? + 1;
            let next = &rest[end + 1..];
            if !next.is_empty() && !next.starts_with(char::is_whitespace) {
                return None;
            }
            (rest[1..end].to_owned(), next)
        } else {
            let end = rest.find(char::is_whitespace).unwrap_or(rest.len());
            let argument = &rest[..end];
            if argument.contains(['"', '\'']) {
                return None;
            }
            (argument.to_owned(), &rest[end..])
        };
        if argument.is_empty() {
            return None;
        }
        arguments.push(argument);
        rest = next.trim_start();
    }
    arguments.try_into().ok()
}
fn owner(process: &Process) -> Option<Provider> {
    let name = process.name.to_ascii_lowercase().replace('\\', "/");
    let base = name.rsplit('/').next().unwrap_or("");
    // The native Claude installer launches a `claude` symlink whose actual
    // executable is the exact version beneath this installer-owned directory.
    if name
        .strip_suffix(base)
        .is_some_and(|directory| directory.ends_with("/.local/share/claude/versions/"))
        && base.split('.').count() == 3
        && base
            .split('.')
            .all(|part| !part.is_empty() && part.bytes().all(|b| b.is_ascii_digit()))
    {
        return Some(Provider::Claude);
    }
    for (provider, cli, app, script) in [
        (
            Provider::Claude,
            "claude",
            "claude.app/",
            "/@anthropic-ai/claude-code/cli.js",
        ),
        (
            Provider::Codex,
            "codex",
            "codex.app/",
            "/@openai/codex/bin/codex.js",
        ),
    ] {
        let app_executable = name.split_once(app).is_some_and(|(_, tail)| {
            tail == format!("contents/macos/{cli}")
                || (tail.starts_with(&format!("contents/frameworks/{cli} helper"))
                    && tail.split_once(".app/contents/macos/").is_some_and(
                        |(container, executable)| {
                            container.strip_prefix("contents/frameworks/") == Some(executable)
                                && (executable == format!("{cli} helper")
                                    || executable.starts_with(&format!("{cli} helper ("))
                                        && executable.ends_with(')'))
                        },
                    ))
        });
        if base == cli || base == format!("{cli}.exe") || app_executable {
            return Some(provider);
        }
        if matches!(base, "node" | "node.exe" | "bun" | "bun.exe")
            && let Some([_, argument]) = process.command.as_deref().and_then(first_two_arguments)
            && argument
                .to_ascii_lowercase()
                .replace('\\', "/")
                .ends_with(script)
        {
            return Some(provider);
        }
    }
    None
}

fn targets(
    processes: &[Process],
    provider: Provider,
    current: u32,
    approved: &[Process],
) -> Result<Vec<Process>> {
    use std::collections::{BTreeMap, BTreeSet};
    let by_pid = processes
        .iter()
        .map(|p| (p.pid, p))
        .collect::<BTreeMap<_, _>>();
    if by_pid.len() != processes.len() || processes.iter().any(|p| p.pid == 0) {
        bail!("Process inventory cannot be verified");
    }
    let mut ancestors = BTreeSet::new();
    let mut pid = current;
    while pid != 0 {
        if !ancestors.insert(pid) {
            bail!("Process ancestry cannot be verified");
        }
        let process = by_pid
            .get(&pid)
            .context("Caller ancestry unavailable; run aiClean in an external terminal")?;
        pid = process.parent;
    }
    let mut selected = processes
        .iter()
        .filter(|p| owner(p) == Some(provider) || approved.iter().any(|a| same_identity(p, a)))
        .map(|p| p.pid)
        .collect::<BTreeSet<_>>();
    loop {
        let before = selected.len();
        for process in processes {
            if selected.contains(&process.parent) {
                if owner(process).is_some_and(|p| p != provider) {
                    bail!(
                        "Another provider is hosted by the selected application; close it manually"
                    );
                }
                selected.insert(process.pid);
            }
        }
        if selected.len() == before {
            break;
        }
    }
    if !selected.is_disjoint(&ancestors) {
        bail!(
            "The selected provider hosts aiClean; run aiClean in an external terminal (caller and ancestors are never stopped)"
        );
    }
    if processes.iter().any(|p| {
        p.command.is_none()
            && matches!(
                p.name
                    .rsplit(['/', '\\'])
                    .next()
                    .unwrap_or("")
                    .to_ascii_lowercase()
                    .as_str(),
                "node.exe" | "electron.exe"
            )
            && !selected.contains(&p.pid)
    }) {
        bail!("Unknown node/electron process ownership; close it manually (no processes stopped)");
    }
    if processes
        .iter()
        .any(|p| selected.contains(&p.pid) && p.started.is_empty())
    {
        bail!("Selected process start identity unavailable; close it manually");
    }
    let mut result = processes
        .iter()
        .filter(|p| selected.contains(&p.pid))
        .cloned()
        .collect::<Vec<_>>();
    // Descendants first: their identity is still inspectable before the app exits.
    result.sort_by_key(|p| {
        let mut depth = 0;
        let mut pid = p.parent;
        let mut seen = BTreeSet::new();
        while selected.contains(&pid) && seen.insert(pid) {
            depth += 1;
            pid = by_pid[&pid].parent;
        }
        std::cmp::Reverse(depth)
    });
    Ok(result)
}
fn preflight<B: Backend>(
    backend: &mut B,
    provider: Provider,
    current: u32,
    interactive: bool,
    mut confirm: impl FnMut(&[Process]) -> Result<bool>,
) -> Result<()> {
    let initial = targets(&backend.inventory()?, provider, current, &[])?;
    if initial.is_empty() {
        return Ok(());
    }
    if !interactive {
        bail!(
            "Selected provider application or CLI is running; non-interactive/scheduled operation refuses termination"
        );
    }
    if !confirm(&initial)? {
        bail!("キャンセルしました。プロセスは停止していません。");
    }
    // Confirmation covers this exact inventory only. Restarted/new processes require a new run.
    for force in [false, true] {
        for process in &initial {
            let now = backend.inventory()?;
            validate_approved(&now, &initial)?;
            let remaining = targets(&now, provider, current, &initial)?;
            if remaining
                .iter()
                .any(|p| !initial.iter().any(|approved| same_identity(p, approved)))
            {
                bail!("Provider process inventory changed; no new processes will be stopped");
            }
            if now.iter().any(|p| same_identity(p, process)) {
                backend.signal(process, force)?;
            }
        }
        for _ in 0..20 {
            let now = backend.inventory()?;
            validate_approved(&now, &initial)?;
            let remaining = targets(&now, provider, current, &initial)?;
            if remaining.is_empty()
                && !now
                    .iter()
                    .any(|p| initial.iter().any(|approved| same_identity(p, approved)))
            {
                return Ok(());
            }
            backend.wait();
        }
    }
    bail!("Provider processes remain after bounded shutdown; mutation refused")
}

struct Native;
impl Backend for Native {
    fn inventory(&mut self) -> Result<Vec<Process>> {
        native_inventory()
    }
    fn wait(&mut self) {
        std::thread::sleep(std::time::Duration::from_millis(250));
    }
    fn signal(&mut self, process: &Process, force: bool) -> Result<()> {
        #[cfg(target_os = "macos")]
        {
            let pid = i32::try_from(process.pid).context("Invalid process PID")?;
            let (started, path) = mac_identity(pid)?;
            if started != process.started || path != process.name {
                bail!("Process identity changed before termination");
            }
            unsafe extern "C" {
                fn kill(pid: i32, signal: i32) -> i32;
            }
            // Darwin signal constants. No subprocess gap between birth identity and signal.
            if unsafe { kill(pid, if force { 9 } else { 15 }) } != 0 {
                let error = std::io::Error::last_os_error();
                if error.raw_os_error() != Some(3) {
                    return Err(error).context("Provider process could not be stopped");
                }
            }
        }
        #[cfg(target_os = "windows")]
        windows_signal(process, force)?;
        #[cfg(not(any(target_os = "macos", target_os = "windows")))]
        {
            let _ = force;
            bail!("Cleanup supports only native macOS and Windows");
        }

        Ok(())
    }
}
#[cfg(target_os = "windows")]
fn windows_signal(process: &Process, force: bool) -> Result<()> {
    type Handle = *mut std::ffi::c_void;
    #[repr(C)]
    #[derive(Clone, Copy, Default)]
    struct FileTime {
        low: u32,
        high: u32,
    }
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn OpenProcess(access: u32, inherit: i32, pid: u32) -> Handle;
        fn GetProcessTimes(
            handle: Handle,
            creation: *mut FileTime,
            exit: *mut FileTime,
            kernel: *mut FileTime,
            user: *mut FileTime,
        ) -> i32;
        fn TerminateProcess(handle: Handle, exit_code: u32) -> i32;
        fn CloseHandle(handle: Handle) -> i32;
    }
    struct OwnedHandle(Handle);
    impl Drop for OwnedHandle {
        fn drop(&mut self) {
            unsafe {
                CloseHandle(self.0);
            }
        }
    }
    // Keep this verified process object open through graceful dispatch/force termination.
    // A process identifier cannot be recycled while this handle keeps its object alive.
    let handle = unsafe { OpenProcess(0x1000 | 1, 0, process.pid) };
    if handle.is_null() {
        return Err(std::io::Error::last_os_error()).context("Process handle unavailable");
    }
    let handle = OwnedHandle(handle);
    let mut creation = FileTime::default();
    let mut exit = creation;
    let mut kernel = creation;
    let mut user = creation;
    if unsafe { GetProcessTimes(handle.0, &mut creation, &mut exit, &mut kernel, &mut user) } == 0 {
        return Err(std::io::Error::last_os_error())
            .context("Native process birth identity unavailable");
    }
    let ticks = ((creation.high as u64) << 32) | creation.low as u64;
    let started = ticks
        .checked_add(504_911_232_000_000_000)
        .context("Invalid native process birth time")?
        .to_string();
    if started != process.started {
        bail!("Process identity changed before termination");
    }
    if force {
        if unsafe { TerminateProcess(handle.0, 1) } == 0 {
            return Err(std::io::Error::last_os_error())
                .context("Provider process could not be stopped");
        }
    } else {
        let output = crate::platform::powershell(
            r"$app=Get-Process -Id $env:AICLEAN_PID -ErrorAction SilentlyContinue; if($null -eq $app){exit 0}; try { $null=$app.Handle; if($app.StartTime.ToUniversalTime().Ticks.ToString() -ne $env:AICLEAN_STARTED){throw 'Process identity changed'}; $null=$app.CloseMainWindow() } finally { $app.Dispose() }",
            &[
                ("AICLEAN_PID".into(), process.pid.to_string().into()),
                ("AICLEAN_STARTED".into(), process.started.clone().into()),
            ],
        )?;
        if !output.success {
            bail!("Provider graceful shutdown could not be requested");
        }
    }
    Ok(())
}
#[cfg(target_os = "macos")]
#[repr(C)]
#[derive(Default)]
struct MacBsdInfo {
    fields: [u32; 12],
    comm: [u8; 16],
    name: [u8; 32],
    trailing: [u32; 6],
    seconds: u64,
    microseconds: u64,
}
#[cfg(target_os = "macos")]
impl MacBsdInfo {
    fn birth(&self) -> String {
        format!("{}:{}", self.seconds, self.microseconds)
    }
}
#[cfg(target_os = "macos")]
fn mac_bsd(pid: i32) -> Result<MacBsdInfo> {
    unsafe extern "C" {
        fn proc_pidinfo(
            pid: i32,
            flavor: i32,
            arg: u64,
            buffer: *mut std::ffi::c_void,
            size: i32,
        ) -> i32;
    }
    let mut info = MacBsdInfo::default();
    let size = std::mem::size_of::<MacBsdInfo>() as i32;
    if unsafe { proc_pidinfo(pid, 3, 0, (&mut info as *mut MacBsdInfo).cast(), size) } != size {
        bail!("Native process birth/state unavailable");
    }
    Ok(info)
}
#[cfg(target_os = "macos")]
fn mac_identity(pid: i32) -> Result<(String, String)> {
    unsafe extern "C" {
        fn proc_pidpath(pid: i32, buffer: *mut std::ffi::c_void, size: u32) -> i32;
    }
    let started = mac_bsd(pid)?.birth();
    let mut path = [0u8; 4096];
    if unsafe { proc_pidpath(pid, path.as_mut_ptr().cast(), path.len() as u32) } <= 0 {
        bail!("Native executable identity unavailable");
    }
    let end = path
        .iter()
        .position(|b| *b == 0)
        .context("Native executable path malformed")?;
    let path = std::str::from_utf8(&path[..end])
        .context("Native executable path encoding unavailable")?
        .to_owned();
    if mac_bsd(pid)?.birth() != started {
        bail!("Native process identity changed during observation");
    }
    Ok((started, path))
}
#[cfg(target_os = "macos")]
fn mac_command(pid: i32) -> Result<String> {
    unsafe extern "C" {
        fn sysctl(
            name: *mut i32,
            namelen: u32,
            oldp: *mut std::ffi::c_void,
            oldlen: *mut usize,
            newp: *mut std::ffi::c_void,
            newlen: usize,
        ) -> i32;
    }
    let mut mib = [1, 49, pid];
    let mut size = 0usize;
    if unsafe {
        sysctl(
            mib.as_mut_ptr(),
            3,
            std::ptr::null_mut(),
            &mut size,
            std::ptr::null_mut(),
            0,
        )
    } != 0
        || size == 0
        || size > 1024 * 1024
    {
        bail!("Native argv unavailable");
    }
    let mut bytes = vec![0u8; size];
    if unsafe {
        sysctl(
            mib.as_mut_ptr(),
            3,
            bytes.as_mut_ptr().cast(),
            &mut size,
            std::ptr::null_mut(),
            0,
        )
    } != 0
    {
        bail!("Native argv unavailable");
    }
    bytes.truncate(size);
    let count = i32::from_ne_bytes(
        bytes
            .get(..4)
            .context("Native argv count missing")?
            .try_into()?,
    );
    if !(1..100_000).contains(&count) {
        bail!("Invalid native argv count");
    }
    let mut cursor = 4;
    cursor += bytes[cursor..]
        .iter()
        .position(|b| *b == 0)
        .context("Native argv executable missing")?
        + 1;
    while bytes.get(cursor) == Some(&0) {
        cursor += 1;
    }
    let mut args = Vec::new();
    for _ in 0..count.min(2) {
        let rest = bytes.get(cursor..).context("Truncated native argv")?;
        let end = rest
            .iter()
            .position(|b| *b == 0)
            .context("Native argv terminator missing")?;
        let argument = std::str::from_utf8(&rest[..end]).context("Native argv encoding unknown")?;
        if argument.contains('"') {
            bail!("Ambiguous native argv quoting");
        }
        args.push(format!("\"{argument}\""));
        cursor += end + 1;
    }
    Ok(args.join(" "))
}
#[cfg(target_os = "macos")]
fn native_inventory() -> Result<Vec<Process>> {
    let identities = crate::platform::run_command(
        "/bin/ps",
        &[
            "-ww".into(),
            "-axo".into(),
            "pid=,ppid=,stat=,lstart=,comm=".into(),
        ],
        &[],
    )?;
    let commands = crate::platform::run_command(
        "/bin/ps",
        &["-ww".into(), "-axo".into(), "pid=,args=".into()],
        &[],
    )?;
    if !identities.success || !commands.success {
        bail!("Process inventory unavailable");
    }
    let mut processes = parse_mac_inventory(&identities.stdout, &commands.stdout)?;
    for process in &mut processes {
        match i32::try_from(process.pid)
            .ok()
            .and_then(|pid| mac_identity(pid).ok())
        {
            Some((started, path)) => {
                process.command = mac_command(process.pid as i32).ok();
                if mac_identity(process.pid as i32).ok() != Some((started.clone(), path.clone())) {
                    process.started.clear();
                    continue;
                }
                process.started = started;
                process.name = path;
            }
            None => process.started.clear(),
        }
    }
    Ok(processes)
}
#[cfg(any(target_os = "macos", test))]
fn parse_mac_inventory(identities: &str, commands: &str) -> Result<Vec<Process>> {
    let commands = commands
        .lines()
        .map(|line| {
            let (pid, command) = line
                .trim()
                .split_once(char::is_whitespace)
                .context("Invalid process command inventory")?;
            Ok((pid.parse::<u32>()?, command.trim().to_owned()))
        })
        .collect::<Result<std::collections::BTreeMap<_, _>>>()?;
    identities
        .lines()
        .filter(|l| !l.trim().is_empty())
        .filter(|line| {
            !line
                .split_whitespace()
                .nth(2)
                .is_some_and(|state| state.starts_with('Z'))
        })
        .map(|line| {
            let mut fields = line.split_whitespace();
            let pid = fields.next().context("PID missing")?.parse()?;
            let parent = fields.next().context("Parent missing")?.parse()?;
            let _state = fields.next().context("Native process state missing")?;
            let started = (0..5)
                .map(|_| fields.next().context("Start identity missing"))
                .collect::<Result<Vec<_>>>()?
                .join(" ");
            let name = fields.collect::<Vec<_>>().join(" ");
            if name.is_empty() {
                bail!("Executable identity missing");
            }
            Ok(Process {
                pid,
                parent,
                started,
                name,
                command: commands.get(&pid).cloned(),
            })
        })
        .collect()
}
#[cfg(target_os = "windows")]
fn native_inventory() -> Result<Vec<Process>> {
    #[derive(serde::Deserialize)]
    struct Inventory {
        helper_pid: u32,
        processes: Vec<WindowsProcess>,
    }
    #[derive(serde::Deserialize)]
    struct WindowsProcess {
        pid: u32,
        parent: u32,
        started: String,
        name: String,
        executable: Option<String>,
        command: Option<String>,
    }
    let output = crate::platform::powershell(
        r"@{helper_pid=$PID;processes=@(Get-CimInstance Win32_Process | Where-Object {$_.ProcessId -ne 0} | ForEach-Object {
$native=$null; $started=''; $path=$null; $command=$null;
try {
    $native=Get-Process -Id $_.ProcessId -ErrorAction Stop; $null=$native.Handle;
    $ticks=$native.StartTime.ToUniversalTime().Ticks;
    # CIM timestamps retain microseconds; compare within one microsecond to the same held process object.
    if($null -eq $_.CreationDate -or [Math]::Abs($ticks-$_.CreationDate.ToUniversalTime().Ticks) -gt 9){throw 'Process identity changed'};
    $path=$native.MainModule.FileName; $started=$ticks.ToString(); $command=$_.CommandLine
} catch {} finally {if($null -ne $native){$native.Dispose()}};
@{pid=$_.ProcessId;parent=$_.ParentProcessId;started=$started;name=$_.Name;executable=$path;command=$command}
})} | ConvertTo-Json -Depth 4 -Compress",
        &[],
    )?;
    if !output.success {
        bail!("Process inventory unavailable");
    }
    let inventory: Inventory =
        serde_json::from_str(&output.stdout).context("Process inventory invalid")?;
    if inventory.helper_pid == 0 || inventory.processes.is_empty() {
        bail!("Process inventory incomplete");
    }
    Ok(inventory
        .processes
        .into_iter()
        .filter(|p| p.pid != inventory.helper_pid)
        .map(|p| Process {
            pid: p.pid,
            parent: p.parent,
            started: p.started,
            name: p.executable.unwrap_or(p.name),
            command: p.command,
        })
        .collect())
}
#[cfg(not(any(target_os = "macos", target_os = "windows")))]
fn native_inventory() -> Result<Vec<Process>> {
    bail!("Cleanup supports only native macOS and Windows")
}

fn affirmative(answer: &str) -> bool {
    matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes")
}
pub fn prepare(provider: Provider, scheduled: bool) -> Result<()> {
    use std::io::{IsTerminal, Write};
    let interactive =
        !scheduled && std::io::stdin().is_terminal() && std::io::stderr().is_terminal();
    preflight(
        &mut Native,
        provider,
        std::process::id(),
        interactive,
        |processes| {
            eprintln!(
                "{provider:?} のアプリ・CLI・補助プロセスをすべて停止して続行します。未保存の作業が失われる可能性があります。"
            );
            for process in processes {
                let name = process.name.rsplit(['/', '\\']).next().unwrap_or("process");
                let safe = name
                    .chars()
                    .filter(|c| {
                        !c.is_control()
                            && !matches!(*c,'\u{202a}'..='\u{202e}'|'\u{2066}'..='\u{2069}')
                    })
                    .take(120)
                    .collect::<String>();
                eprintln!("  PID {}: {}", process.pid, safe);
            }
            eprint!("停止して続行しますか？ [y/N]: ");
            std::io::stderr().flush()?;
            let mut answer = String::new();
            std::io::stdin().read_line(&mut answer)?;
            Ok(affirmative(&answer))
        },
    )?;
    crate::platform::ensure_quiet(provider)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    #[cfg(target_os = "macos")]
    fn native_owned_child_shutdown_accepts_unreaped_zombie() {
        use std::{
            process::{Command, Stdio},
            time::{Duration, Instant},
        };
        struct ChildGuard(std::process::Child);
        impl Drop for ChildGuard {
            fn drop(&mut self) {
                let _ = self.0.kill();
                let _ = self.0.wait();
            }
        }
        let fixture = tempfile::tempdir().unwrap();
        let executable = fixture.path().join("claude");
        let mut compiler = Command::new("/usr/bin/cc")
            .args(["-x", "c", "-o"])
            .arg(&executable)
            .arg("-")
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        std::io::Write::write_all(
            &mut compiler.stdin.take().unwrap(),
            b"#include <unistd.h>\nint main(void){sleep(30);return 0;}\n",
        )
        .unwrap();
        assert!(
            compiler.wait().unwrap().success(),
            "Owned child fixture compiler failed"
        );
        let child = Command::new(&executable)
            .arg("30")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let child = ChildGuard(child);
        let pid = child.0.id();
        let deadline = Instant::now() + Duration::from_secs(3);
        let process = loop {
            if let Some(process) = native_inventory().unwrap().into_iter().find(|p| {
                p.pid == pid && owner(p) == Some(Provider::Claude) && !p.started.is_empty()
            }) {
                break process;
            }
            assert!(
                Instant::now() < deadline,
                "Owned fixture identity never became available"
            );
            std::thread::sleep(Duration::from_millis(20));
        };
        // Only this exclusively spawned fixture receives a native signal. We
        // deliberately leave it unreaped until ChildGuard drops after assertions.
        Native.signal(&process, false).unwrap();
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            if !native_inventory().unwrap().iter().any(|p| p.pid == pid) {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "Stopped unreaped fixture still blocks shutdown"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }
    #[test]
    fn only_exact_entrypoints_and_native_app_executables_are_owned() {
        assert_eq!(
            owner(&p(
                10,
                1,
                "/Users/owner/.local/share/claude/versions/2.1.296"
            )),
            Some(Provider::Claude)
        );
        for path in [
            "/tmp/claude/versions/2.1.296",
            "/Users/owner/.local/share/claude/versions/diagnostic",
            "/Users/owner/.local/share/claude/versions/2.1.296.backup",
        ] {
            assert_eq!(owner(&p(10, 1, path)), None);
        }
        for command in [
            "node /tmp/@anthropic-ai/claude-code/diagnostic.js",
            "node /tmp/@anthropic-ai/claude-code/cli.js.backup",
            "node script.js --prompt /tmp/@anthropic-ai/claude-code/cli.js",
            "node /tmp space/@anthropic-ai/claude-code/cli.js",
        ] {
            let mut process = p(10, 1, "node");
            process.command = Some(command.into());
            assert_eq!(owner(&process), None, "{command}");
        }
        assert_eq!(owner(&p(10, 1, "/tmp/Claude.app/archive/python")), None);
        for (command, provider) in [
            (
                "node \"/tmp space/@anthropic-ai/claude-code/cli.js\"",
                Provider::Claude,
            ),
            (
                r#""C:\Program Files\node.exe" "C:\Users\owner\node_modules\@openai\codex\bin\codex.js""#,
                Provider::Codex,
            ),
        ] {
            let mut process = p(10, 1, "node");
            process.command = Some(command.into());
            assert_eq!(owner(&process), Some(provider));
        }
        assert_eq!(
            owner(&p(
                10,
                1,
                "/Applications/Claude.app/Contents/Frameworks/Claude Helper (Renderer).app/Contents/MacOS/Claude Helper (Renderer)"
            )),
            Some(Provider::Claude)
        );
    }
    #[test]
    fn confirmation_defaults_no_and_only_explicit_yes_accepts() {
        for no in ["", "\n", "n", "no", "true", "yes please", "はい"] {
            assert!(!affirmative(no));
        }
        for yes in ["y", "Y\n", " yes ", "YES"] {
            assert!(affirmative(yes));
        }
    }
    #[test]
    fn unknown_runtime_and_missing_ancestry_never_prompt_or_signal() {
        let mut unknown = p(15, 1, r"C:\Program Files\node.exe");
        unknown.command = None;
        for processes in [
            vec![
                p(1, 0, "launchd"),
                unknown,
                p(10, 1, "claude"),
                p(99, 1, "aiClean"),
            ],
            vec![p(10, 1, "claude"), p(99, 77, "aiClean")],
        ] {
            let mut b = fake(processes);
            assert!(
                preflight(&mut b, Provider::Claude, 99, true, |_| panic!(
                    "unsafe prompt"
                ))
                .is_err()
            );
            assert!(b.signals.is_empty());
        }
    }
    #[test]
    fn mac_inventory_preserves_spaced_paths_and_start_identity() {
        let parsed = parse_mac_inventory(" 42 1 S Sun Oct 11 01:02:03 2026 /Applications/Claude Desktop/Claude.app/Contents/MacOS/Claude\n", "42 /Applications/Claude Desktop/Claude.app --secret private\n").unwrap();
        assert_eq!(parsed[0].pid, 42);
        assert_eq!(parsed[0].started, "Sun Oct 11 01:02:03 2026");
        assert_eq!(owner(&parsed[0]), Some(Provider::Claude));
        assert!(parse_mac_inventory("incomplete", "42 anything").is_err());
    }
    #[test]
    fn recycled_pid_is_refused_without_signal() {
        struct Reused {
            calls: usize,
            signals: usize,
        }
        impl Backend for Reused {
            fn inventory(&mut self) -> Result<Vec<Process>> {
                self.calls += 1;
                let mut process = p(10, 1, "claude");
                if self.calls > 1 {
                    process.started = "new identity".into();
                }
                Ok(vec![p(1, 0, "launchd"), process, p(99, 1, "aiClean")])
            }
            fn signal(&mut self, _: &Process, _: bool) -> Result<()> {
                self.signals += 1;
                Ok(())
            }
            fn wait(&mut self) {}
        }
        let mut b = Reused {
            calls: 0,
            signals: 0,
        };
        assert!(preflight(&mut b, Provider::Claude, 99, true, |_| Ok(true)).is_err());
        assert_eq!(b.signals, 0);
    }
    #[test]
    fn reparented_descendant_is_still_force_stopped() {
        struct Reparented {
            processes: Vec<Process>,
            signals: Vec<(u32, bool)>,
            spawn_new: bool,
        }
        impl Backend for Reparented {
            fn inventory(&mut self) -> Result<Vec<Process>> {
                Ok(self.processes.clone())
            }
            fn signal(&mut self, process: &Process, force: bool) -> Result<()> {
                self.signals.push((process.pid, force));
                if process.pid == 10 {
                    self.processes.retain(|p| p.pid != 10);
                    self.processes
                        .iter_mut()
                        .filter(|p| p.pid == 11)
                        .for_each(|p| p.parent = 1);
                    if self.spawn_new {
                        self.processes.push(p(12, 11, "new-worker"));
                    }
                }
                if force {
                    self.processes.retain(|p| p.pid != process.pid);
                }
                Ok(())
            }
            fn wait(&mut self) {}
        }
        let mut b = Reparented {
            processes: vec![
                p(1, 0, "launchd"),
                p(10, 1, "claude"),
                p(11, 10, "worker"),
                p(99, 1, "aiClean"),
            ],
            signals: vec![],
            spawn_new: false,
        };
        preflight(&mut b, Provider::Claude, 99, true, |_| Ok(true)).unwrap();
        assert_eq!(b.signals, vec![(11, false), (10, false), (11, true)]);
        let mut b = Reparented {
            processes: vec![
                p(1, 0, "launchd"),
                p(10, 1, "claude"),
                p(11, 10, "worker"),
                p(99, 1, "aiClean"),
            ],
            signals: vec![],
            spawn_new: true,
        };
        assert!(preflight(&mut b, Provider::Claude, 99, true, |_| Ok(true)).is_err());
        assert!(b.processes.iter().any(|p| p.pid == 12));
        assert_eq!(b.signals, vec![(11, false), (10, false)]);
    }
    #[test]
    #[cfg(any(target_os = "macos", target_os = "windows"))]
    fn native_inventory_readonly_contains_current_identity() {
        let inventory = native_inventory().unwrap();
        assert!(
            inventory
                .iter()
                .any(|p| p.pid == std::process::id() && !p.started.is_empty())
        );
    }
    fn p(pid: u32, parent: u32, name: &str) -> Process {
        Process {
            pid,
            parent,
            started: "identity-1".into(),
            name: name.into(),
            command: Some(name.into()),
        }
    }
    struct Fake {
        processes: Vec<Process>,
        signals: Vec<(u32, bool)>,
        stubborn: bool,
    }
    impl Backend for Fake {
        fn inventory(&mut self) -> Result<Vec<Process>> {
            Ok(self.processes.clone())
        }
        fn signal(&mut self, process: &Process, force: bool) -> Result<()> {
            self.signals.push((process.pid, force));
            if force || !self.stubborn {
                self.processes.retain(|p| p.pid != process.pid);
            }
            Ok(())
        }
        fn wait(&mut self) {}
    }
    fn fake(processes: Vec<Process>) -> Fake {
        Fake {
            processes,
            signals: vec![],
            stubborn: false,
        }
    }
    #[test]
    fn confirmed_provider_and_descendants_stop_but_other_provider_survives() {
        let mut b = fake(vec![
            p(1, 0, "launchd"),
            p(10, 1, "claude"),
            p(11, 10, "worker"),
            p(20, 1, "codex"),
            p(99, 1, "aiClean"),
        ]);
        preflight(&mut b, Provider::Claude, 99, true, |targets| {
            assert_eq!(targets.len(), 2);
            Ok(true)
        })
        .unwrap();
        assert_eq!(b.signals, vec![(11, false), (10, false)]);
        assert!(b.processes.iter().any(|p| p.pid == 20));
    }
    #[test]
    fn no_noninteractive_and_hosted_caller_never_signal() {
        for (interactive, parent, answer) in [(true, 1, false), (false, 1, true), (true, 10, true)]
        {
            let mut b = fake(vec![
                p(1, 0, "launchd"),
                p(10, 1, "claude"),
                p(99, parent, "aiClean"),
            ]);
            let mut prompts = 0;
            assert!(
                preflight(&mut b, Provider::Claude, 99, interactive, |_| {
                    prompts += 1;
                    Ok(answer)
                })
                .is_err()
            );
            assert!(b.signals.is_empty());
            assert_eq!(prompts, usize::from(interactive && parent != 10));
        }
    }
    #[test]
    fn stubborn_process_escalates_after_graceful_attempt() {
        let mut b = fake(vec![
            p(1, 0, "launchd"),
            p(10, 1, "claude"),
            p(99, 1, "aiClean"),
        ]);
        b.stubborn = true;
        preflight(&mut b, Provider::Claude, 99, true, |_| Ok(true)).unwrap();
        assert_eq!(b.signals, vec![(10, false), (10, true)]);
    }
    #[test]
    fn unselected_provider_and_argument_mentions_never_prompt() {
        let mut mention = p(10, 1, "python");
        mention.command = Some("python script.py --prompt claude codex.exe".into());
        let mut b = fake(vec![
            p(1, 0, "launchd"),
            mention,
            p(20, 1, "codex"),
            p(99, 1, "aiClean"),
        ]);
        preflight(&mut b, Provider::Claude, 99, true, |_| {
            panic!("unrelated processes")
        })
        .unwrap();
        assert!(b.signals.is_empty());
    }
}
