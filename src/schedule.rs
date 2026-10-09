//! Per-provider native schedules. Spec builders and tests never touch the user's scheduler.
use crate::model::{Provider, Roots};
#[cfg(any(target_os = "macos", test))]
use crate::platform::CommandOutput;
#[cfg(target_os = "macos")]
use crate::platform::run_command;
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
#[cfg(target_os = "macos")]
use std::ffi::OsString;
use std::{
    collections::BTreeMap,
    fs::{self, OpenOptions},
    io::Write,
    path::{Component, Path, PathBuf},
};

#[derive(Debug, Clone, Serialize, Deserialize)]
struct TaskConfig {
    provider: Provider,
    executable: PathBuf,
    state: PathBuf,
    environment: BTreeMap<String, String>,
    enabled: bool,
}
fn label(p: Provider) -> String {
    format!("local.aiClean.{}", p.name())
}
fn config_path(state: &Path, p: Provider) -> PathBuf {
    state.join("schedule").join(format!("{}.json", p.name()))
}
fn stop_path(state: &Path, p: Provider) -> PathBuf {
    state.join(format!("stop-{}", p.name()))
}
/// Reject symlinks and Windows reparse points in every existing path component.
pub(crate) fn safe_path(path: &Path) -> Result<()> {
    if !path.is_absolute() {
        bail!("Owned paths must be absolute");
    }
    let mut current = PathBuf::new();
    for part in path.components() {
        if matches!(part, Component::ParentDir) {
            bail!("Parent traversal refused");
        }
        current.push(part);
        // A Windows prefix (for example \\?\C:) is not itself an absolute root.
        // Querying it before RootDir is appended can fail with ERROR_INVALID_FUNCTION.
        // The absolute-path check above ensures its root is validated on the next component.
        if matches!(part, Component::Prefix(_)) {
            continue;
        }
        match fs::symlink_metadata(&current) {
            Ok(m) => {
                if m.file_type().is_symlink() {
                    bail!("Symlink path refused");
                }
                #[cfg(target_os = "windows")]
                {
                    use std::os::windows::fs::MetadataExt;
                    if m.file_attributes() & 0x400 != 0 {
                        bail!("Reparse-point path refused");
                    }
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e).context("Cannot verify owned path"),
        }
    }
    Ok(())
}
fn private_dir(path: &Path, secure_existing: bool) -> Result<()> {
    safe_path(path)?;
    let existed = path.exists();
    fs::create_dir_all(path)?;
    safe_path(path)?;
    if existed && !secure_existing {
        return Ok(());
    }
    crate::platform::ensure_private_directory(path)?;
    Ok(())
}
fn private_write(path: &Path, contents: &[u8]) -> Result<()> {
    private_write_inner(path, contents, true)
}
#[cfg(target_os = "macos")]
fn private_write_shared_parent(path: &Path, contents: &[u8]) -> Result<()> {
    private_write_inner(path, contents, false)
}
fn private_write_inner(path: &Path, contents: &[u8], secure_existing: bool) -> Result<()> {
    safe_path(path)?;
    let parent = path.parent().context("Owned file has no parent")?;
    private_dir(parent, secure_existing)?;
    let tmp = parent.join(format!(".aiClean-{}", uuid::Uuid::new_v4()));
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(&tmp)?;
    file.write_all(contents)?;
    file.sync_all()?;
    drop(file);
    safe_path(path)?;
    crate::platform::durable_replace(&tmp, path)?;
    Ok(())
}
fn xml(s: &str) -> Result<String> {
    if s.chars()
        .any(|c| c < ' ' && !matches!(c, '\t' | '\n' | '\r'))
    {
        bail!("Invalid XML path character");
    }
    Ok(s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;"))
}
fn path_text(path: &Path) -> Result<&str> {
    path.to_str().context("Schedule path must be UTF-8")
}
#[cfg(any(target_os = "macos", test))]
fn launchd_spec(c: &TaskConfig) -> Result<String> {
    let executable = xml(path_text(&c.executable)?)?;
    let state = xml(path_text(&c.state)?)?;
    let mut env = String::new();
    for (k, v) in &c.environment {
        env.push_str(&format!(
            "<key>{}</key><string>{}</string>",
            xml(k)?,
            xml(v)?
        ));
    }
    Ok(format!(
        r#"<?xml version="1.0" encoding="UTF-8"?><!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd"><plist version="1.0"><dict><key>Label</key><string>{}</string><key>ProgramArguments</key><array><string>{executable}</string><string>--state-dir</string><string>{state}</string><string>run</string><string>--scheduled</string><string>{}</string></array><key>EnvironmentVariables</key><dict>{env}</dict><key>RunAtLoad</key><false/><key>StartCalendarInterval</key><dict><key>Day</key><integer>10</integer><key>Hour</key><integer>3</integer><key>Minute</key><integer>30</integer></dict><key>ProcessType</key><string>Background</string><key>Disabled</key><{}/></dict></plist>"#,
        label(c.provider),
        c.provider.name(),
        if c.enabled { "false" } else { "true" }
    ))
}
#[cfg(any(target_os = "windows", test))]
fn windows_spec(c: &TaskConfig, sid: &str, start_boundary: &str) -> Result<String> {
    // Task Scheduler cannot attach per-action environment variables. This fixed, private wrapper
    // reads our JSON as data and invokes the absolute binary, without evaluating config as code.
    let helper =
        PathBuf::from(std::env::var_os("SystemRoot").unwrap_or_else(|| "C:\\Windows".into()))
            .join("System32/WindowsPowerShell/v1.0/powershell.exe");
    let wrapper = c
        .state
        .join("schedule")
        .join(format!("{}.ps1", c.provider.name()));
    let args = format!(
        "-NoLogo -NoProfile -NonInteractive -ExecutionPolicy Bypass -File {}",
        windows_quote(path_text(&wrapper)?)
    );
    Ok(format!(
        r#"<?xml version="1.0"?><Task version="1.2" xmlns="http://schemas.microsoft.com/windows/2004/02/mit/task"><RegistrationInfo><Description>aiClean recoverable monthly {}</Description></RegistrationInfo><Triggers><CalendarTrigger><StartBoundary>{}</StartBoundary><Enabled>true</Enabled><ScheduleByMonth><DaysOfMonth><Day>10</Day></DaysOfMonth><Months><January/><February/><March/><April/><May/><June/><July/><August/><September/><October/><November/><December/></Months></ScheduleByMonth></CalendarTrigger></Triggers><Principals><Principal id="CurrentUser"><UserId>{}</UserId><LogonType>InteractiveToken</LogonType><RunLevel>LeastPrivilege</RunLevel></Principal></Principals><Settings><MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy><DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries><StopIfGoingOnBatteries>false</StopIfGoingOnBatteries><StartWhenAvailable>true</StartWhenAvailable><Enabled>{}</Enabled><ExecutionTimeLimit>PT0S</ExecutionTimeLimit><AllowHardTerminate>false</AllowHardTerminate></Settings><Actions Context="CurrentUser"><Exec><Command>{}</Command><Arguments>{}</Arguments></Exec></Actions></Task>"#,
        c.provider.name(),
        xml(start_boundary)?,
        xml(sid)?,
        c.enabled,
        xml(path_text(&helper)?)?,
        xml(&args)?
    ))
}
fn next_boundary(now: chrono::NaiveDateTime) -> Result<String> {
    use chrono::Datelike;
    let this = chrono::NaiveDate::from_ymd_opt(now.year(), now.month(), 10)
        .context("Invalid schedule date")?
        .and_hms_opt(3, 30, 0)
        .context("Invalid schedule time")?;
    let next = if this > now {
        this
    } else {
        let (year, month) = if now.month() == 12 {
            (now.year() + 1, 1)
        } else {
            (now.year(), now.month() + 1)
        };
        chrono::NaiveDate::from_ymd_opt(year, month, 10)
            .context("Invalid next schedule date")?
            .and_hms_opt(3, 30, 0)
            .context("Invalid next schedule time")?
    };
    Ok(next.format("%Y-%m-%dT%H:%M:%S").to_string())
}
#[cfg(any(target_os = "windows", test))]
fn windows_quote(s: &str) -> String {
    let mut out = String::from("\"");
    let mut backslashes = 0;
    for c in s.chars() {
        if c == '\\' {
            backslashes += 1;
        } else {
            if c == '"' {
                out.push_str(&"\\".repeat(backslashes * 2 + 1));
            } else {
                out.push_str(&"\\".repeat(backslashes));
            }
            backslashes = 0;
            out.push(c);
        }
    }
    out.push_str(&"\\".repeat(backslashes * 2));
    out.push('"');
    out
}
fn capture(state: &Path, p: Provider) -> Result<TaskConfig> {
    safe_path(state)?;
    let executable = std::env::current_exe()?.canonicalize()?;
    safe_path(&executable)?;
    let roots = Roots::from_env()?;
    let mut environment = BTreeMap::new();
    for (name, path) in [
        ("CODEX_HOME", roots.codex),
        ("CLAUDE_CONFIG_DIR", roots.claude),
    ] {
        let abs = if path.is_absolute() {
            path
        } else {
            std::env::current_dir()?.join(path)
        };
        safe_path(&abs)?;
        environment.insert(name.to_owned(), path_text(&abs)?.to_owned());
    }
    Ok(TaskConfig {
        provider: p,
        executable,
        state: state.to_path_buf(),
        environment,
        enabled: true,
    })
}
fn read_config(state: &Path, p: Provider) -> Result<Option<TaskConfig>> {
    let path = config_path(state, p);
    safe_path(&path)?;
    match fs::read(&path) {
        Ok(b) => {
            let c: TaskConfig = serde_json::from_slice(&b).context("Invalid task config")?;
            if c.provider != p || c.state != state {
                bail!("Task config does not match requested state/provider");
            }
            Ok(Some(c))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).context("Cannot read task config"),
    }
}
fn save_config(c: &TaskConfig) -> Result<()> {
    private_dir(&c.state, true)?;
    private_write(
        &config_path(&c.state, c.provider),
        &serde_json::to_vec_pretty(c)?,
    )
}
fn require_supported() -> Result<()> {
    if !cfg!(any(target_os = "macos", target_os = "windows")) {
        bail!("Native scheduling supports only macOS and Windows");
    }
    Ok(())
}
fn scheduler_lock(state: &Path) -> Result<std::fs::File> {
    use fs2::FileExt;
    private_dir(state, true)?;
    let path = state.join("scheduler.lock");
    safe_path(&path)?;
    let mut options = OpenOptions::new();
    options.read(true).write(true).create(true).truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let file = options.open(&path)?;
    crate::platform::ensure_single_link(&path)?;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    loop {
        match file.try_lock_exclusive() {
            Ok(()) => return Ok(file),
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                if std::time::Instant::now() >= deadline {
                    bail!("Scheduler is busy; stop signal remains recorded");
                }
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            Err(e) => return Err(e).context("Scheduler lock unavailable"),
        }
    }
}
fn request_stop_and_lock(state: &Path, p: Provider) -> Result<std::fs::File> {
    let path = stop_path(state, p);
    let token = uuid::Uuid::new_v4().to_string();
    private_write(&path, token.as_bytes())?;
    let lock = scheduler_lock(state);
    // An in-flight enable may have cleared the first token while we waited. Persist it again
    // under the scheduler lock, or after a timeout so failure remains a cooperative stop request.
    private_write(&path, token.as_bytes())?;
    lock
}

pub fn enable(state: &Path, p: Provider) -> Result<Value> {
    require_supported()?;
    safe_path(state)?;
    crate::platform::ensure_trash_purge_supported(state)?;
    let _lock = crate::storage::maintenance_lock(state)?;
    let _scheduler_lock = scheduler_lock(state)?;
    let previous = read_config(state, p)?;
    verify_task_ownership(state, p, previous.as_ref())?;
    let c = capture(state, p)?;
    save_config(&c)?;
    #[cfg(target_os = "macos")]
    mac_enable(&c)?;
    #[cfg(target_os = "windows")]
    windows_enable(&c)?;
    let signal = stop_path(state, p);
    safe_path(&signal)?;
    match fs::remove_file(&signal) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e).context("Cannot clear stop signal"),
    };
    status(state, p)
}
pub fn stop(state: &Path, p: Provider) -> Result<Value> {
    require_supported()?;
    safe_path(state)?;
    let _scheduler_lock = request_stop_and_lock(state, p)?;
    let previous = read_config(state, p)?;
    verify_task_ownership(state, p, previous.as_ref())?;
    if let Some(mut c) = previous {
        c.enabled = false;
        save_config(&c)?;
    }
    #[cfg(target_os = "macos")]
    mac_stop(state, p)?;
    #[cfg(target_os = "windows")]
    windows_stop(p)?;
    status(state, p)
}
/// Remove only aiClean's owned scheduler artifacts. The uninstall caller holds maintenance_lock.
pub fn remove(state: &Path, p: Provider) -> Result<Value> {
    require_supported()?;
    safe_path(state)?;
    let _scheduler_lock = scheduler_lock(state)?;
    let config = read_config(state, p)?;
    verify_task_ownership(state, p, config.as_ref())?;
    #[cfg(target_os = "macos")]
    {
        let path = plist_path(p)?;
        safe_path(&path)?;
        if path.exists() {
            let config = config
                .as_ref()
                .context("Cannot prove plist ownership without task config")?;
            if !plist_owned(&fs::read(&path)?, config)? {
                bail!("Owned plist was modified; task removal refused");
            }
        }
        let target = format!("{}/{}", mac_domain()?, label(p));
        let task = mac_call(&["print", &target])?;
        if task.success {
            if job_running(&task.stdout) {
                bail!("Cleanup task remains running; removal deferred without killing it");
            }
            let c = config
                .as_ref()
                .context("Cannot prove loaded task ownership without task config")?;
            if !task.stdout.contains(path_text(&c.state)?) {
                bail!("Loaded task uses another state directory; removal refused");
            }
            if !mac_call(&["disable", &target])?.success
                || !mac_call(&["bootout", &target])?.success
            {
                bail!("Owned launchd task removal failed");
            }
        } else if task.code != Some(113) {
            bail!("Task state is unknown; removal refused");
        }
        remove_owned_file(&path)?;
    }
    #[cfg(target_os = "windows")]
    {
        let c = config.as_ref();
        let wrapper = state.join("schedule").join(format!("{}.ps1", p.name()));
        safe_path(&wrapper)?;
        if wrapper.exists() && fs::read(&wrapper)? != WINDOWS_WRAPPER.as_bytes() {
            bail!("Owned task wrapper was modified; removal refused");
        }
        let helper = PathBuf::from(
            std::env::var_os("SystemRoot").context("Windows SystemRoot unavailable")?,
        )
        .join("System32/WindowsPowerShell/v1.0/powershell.exe");
        let args = format!(
            "-NoLogo -NoProfile -NonInteractive -ExecutionPolicy Bypass -File {}",
            windows_quote(path_text(&wrapper)?)
        );
        let result = crate::platform::powershell(
            "$t=$null; try{$t=Get-ScheduledTask -TaskName $env:AICLEAN_TASK_NAME -TaskPath '\\' -ErrorAction Stop}catch{if($_.FullyQualifiedErrorId -notlike 'CmdletizationQuery_NotFound*'){throw}}; if($null -ne $t){if($env:AICLEAN_CONFIG_PRESENT -ne 'true' -or $t.Actions.Count -ne 1 -or $t.Actions[0].Execute -ne $env:AICLEAN_HELPER -or $t.Actions[0].Arguments -ne $env:AICLEAN_ARGUMENTS){throw 'Cannot prove task ownership'};if([string]$t.State -eq 'Running'){throw 'Cleanup is running'};Disable-ScheduledTask -InputObject $t|Out-Null;Unregister-ScheduledTask -InputObject $t -Confirm:$false}",
            &[
                ("AICLEAN_TASK_NAME".into(), label(p).into()),
                (
                    "AICLEAN_CONFIG_PRESENT".into(),
                    c.is_some().to_string().into(),
                ),
                ("AICLEAN_HELPER".into(), helper.into_os_string()),
                ("AICLEAN_ARGUMENTS".into(), args.into()),
            ],
        )?;
        if !result.success {
            bail!("Cannot remove owned Windows task while ownership/state is unverified");
        }
    }
    if config.is_some() {
        for extension in ["json", "xml", "ps1"] {
            remove_owned_file(
                &state
                    .join("schedule")
                    .join(format!("{}.{extension}", p.name())),
            )?;
        }
        remove_owned_file(&stop_path(state, p))?;
    }
    Ok(json!({"provider":p,"task":label(p),"removed":true,"sessions_and_receipts_preserved":true}))
}
#[cfg(any(target_os = "macos", test))]
fn plist_owned(bytes: &[u8], config: &TaskConfig) -> Result<bool> {
    let mut config = config.clone();
    for enabled in [true, false] {
        config.enabled = enabled;
        if bytes == launchd_spec(&config)?.as_bytes() {
            return Ok(true);
        }
    }
    Ok(false)
}
#[cfg(any(target_os = "macos", test))]
fn loaded_action_owned(output: &str, config: &TaskConfig) -> Result<bool> {
    let mut arguments = Vec::new();
    let mut in_arguments = false;
    for line in output.lines() {
        let line = line.trim();
        if line == "arguments = {" {
            in_arguments = true;
            continue;
        }
        if in_arguments && line == "}" {
            break;
        }
        if in_arguments {
            arguments.push(line.trim_matches('"').to_owned());
        }
    }
    Ok(arguments
        == [
            path_text(&config.executable)?,
            "--state-dir",
            path_text(&config.state)?,
            "run",
            "--scheduled",
            config.provider.name(),
        ])
}
fn verify_task_ownership(state: &Path, p: Provider, config: Option<&TaskConfig>) -> Result<()> {
    #[cfg(target_os = "macos")]
    {
        let path = plist_path(p)?;
        safe_path(&path)?;
        if path.exists() {
            let c = config.context("Existing plist has no matching private ownership config")?;
            if !plist_owned(&fs::read(&path)?, c)? {
                bail!("Existing plist is not owned by this state, executable, and provider");
            }
        }
        let target = format!("{}/{}", mac_domain()?, label(p));
        let task = mac_call(&["print", &target])?;
        if task.success {
            let c = config.context("Loaded task has no matching ownership config")?;
            if !loaded_action_owned(&task.stdout, c)? {
                bail!("Loaded task action is not owned by this state, executable, and provider");
            }
        } else if task.code != Some(113) {
            bail!("Existing task ownership/state cannot be verified");
        }
    }
    #[cfg(target_os = "windows")]
    {
        let wrapper = state.join("schedule").join(format!("{}.ps1", p.name()));
        safe_path(&wrapper)?;
        if wrapper.exists() && fs::read(&wrapper)? != WINDOWS_WRAPPER.as_bytes() {
            bail!("Existing wrapper is not owned by this state/provider");
        }
        let helper = PathBuf::from(
            std::env::var_os("SystemRoot").context("Windows SystemRoot unavailable")?,
        )
        .join("System32/WindowsPowerShell/v1.0/powershell.exe");
        let args = format!(
            "-NoLogo -NoProfile -NonInteractive -ExecutionPolicy Bypass -File {}",
            windows_quote(path_text(&wrapper)?)
        );
        let output = crate::platform::powershell(
            "$t=$null;try{$t=Get-ScheduledTask -TaskName $env:AICLEAN_TASK_NAME -TaskPath '\\' -ErrorAction Stop}catch{if($_.FullyQualifiedErrorId -notlike 'CmdletizationQuery_NotFound*'){throw}};if($null -ne $t){if($env:AICLEAN_CONFIG_PRESENT -ne 'true' -or $t.Actions.Count -ne 1 -or $t.Actions[0].Execute -ne $env:AICLEAN_HELPER -or $t.Actions[0].Arguments -ne $env:AICLEAN_ARGUMENTS){throw 'Task ownership is unverified'};if($t.Principal.UserId -ne [Security.Principal.WindowsIdentity]::GetCurrent().User.Value){throw 'Task user does not match'}}",
            &[
                ("AICLEAN_TASK_NAME".into(), label(p).into()),
                (
                    "AICLEAN_CONFIG_PRESENT".into(),
                    config.is_some().to_string().into(),
                ),
                ("AICLEAN_HELPER".into(), helper.into_os_string()),
                ("AICLEAN_ARGUMENTS".into(), args.into()),
            ],
        )?;
        if !output.success {
            bail!(
                "Existing task is not verifiably owned by this state, executable, provider, and user"
            );
        }
    }
    #[cfg(not(target_os = "windows"))]
    let _ = state;
    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    let _ = (p, config);
    Ok(())
}
fn remove_owned_file(path: &Path) -> Result<()> {
    safe_path(path)?;
    match fs::symlink_metadata(path) {
        Ok(meta) => {
            if !meta.is_file() {
                bail!("Owned artifact is not a regular file");
            }
            fs::remove_file(path)?;
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e).context("Cannot inspect owned artifact"),
    };
    Ok(())
}
pub fn status(state: &Path, p: Provider) -> Result<Value> {
    safe_path(state)?;
    let c = read_config(state, p)?;
    let mut result = json!({"provider":p,"task":label(p),"desired_enabled":c.as_ref().map(|c|c.enabled),"config_present":c.is_some(),"stop_requested":stop_path(state,p).exists(),"schedule":{"day":10,"hour":3,"minute":30,"timezone":"OS local","run_at_load":false,"next_calendar_occurrence":next_boundary(chrono::Local::now().naive_local())?}});
    #[cfg(target_os = "macos")]
    {
        result["actual"] = mac_status(p)?;
    }
    #[cfg(target_os = "windows")]
    {
        result["actual"] = windows_status(p)?;
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        result["actual"] = json!({"supported":false,"enabled":null,"loaded":null});
    }
    Ok(result)
}
#[cfg(target_os = "macos")]
fn mac_domain() -> Result<String> {
    let o = run_command("/usr/bin/id", &["-u".into()], &[])?;
    if !o.success || o.stdout.trim().parse::<u32>().is_err() {
        bail!("Current macOS user could not be verified");
    }
    Ok(format!("gui/{}", o.stdout.trim()))
}
#[cfg(target_os = "macos")]
fn plist_path(p: Provider) -> Result<PathBuf> {
    Ok(dirs::home_dir()
        .context("Home unavailable")?
        .join("Library/LaunchAgents")
        .join(format!("{}.plist", label(p))))
}
#[cfg(target_os = "macos")]
fn mac_call(args: &[&str]) -> Result<CommandOutput> {
    run_command(
        "/bin/launchctl",
        &args.iter().map(OsString::from).collect::<Vec<_>>(),
        &[],
    )
}
#[cfg(target_os = "macos")]
fn mac_enable(c: &TaskConfig) -> Result<()> {
    let domain = mac_domain()?;
    let path = plist_path(c.provider)?;
    private_write_shared_parent(&path, launchd_spec(c)?.as_bytes())?;
    register_launchd(
        &domain,
        &label(c.provider),
        path_text(&path)?,
        &mut mac_call,
    )
}
#[cfg(any(target_os = "macos", test))]
fn register_launchd(
    domain: &str,
    label: &str,
    path: &str,
    runner: &mut dyn FnMut(&[&str]) -> Result<CommandOutput>,
) -> Result<()> {
    let target = format!("{domain}/{label}");
    let loaded = runner(&["print", &target])?;
    if loaded.success && job_running(&loaded.stdout) {
        bail!("Owned cleanup is running; wait for it to finish before enabling");
    }
    if !loaded.success && loaded.code != Some(113) {
        bail!("Cannot verify existing launchd task; registration refused");
    }
    if loaded.success && !runner(&["bootout", &target])?.success {
        bail!("Existing owned launchd task could not be unloaded");
    }
    if !runner(&["enable", &target])?.success {
        bail!("launchd task could not be enabled");
    }
    if !runner(&["bootstrap", domain, path])?.success {
        bail!("launchd task registration failed; desired config remains available");
    }
    Ok(())
}
#[cfg(target_os = "macos")]
fn mac_stop(state: &Path, p: Provider) -> Result<()> {
    let target = format!("{}/{}", mac_domain()?, label(p));
    if !mac_call(&["disable", &target])?.success {
        bail!("launchd persistent disable failed; stop signal remains set");
    }
    let path = plist_path(p)?;
    safe_path(&path)?;
    if path.exists()
        && let Some(c) = read_config(state, p)?
    {
        private_write_shared_parent(&path, launchd_spec(&c)?.as_bytes())?;
    }
    let job = mac_call(&["print", &target])?;
    if job.success && !job_running(&job.stdout) && !mac_call(&["bootout", &target])?.success {
        bail!("launchd task could not be unloaded; stop signal remains set");
    }
    Ok(())
}
#[cfg(any(target_os = "macos", test))]
fn job_running(output: &str) -> bool {
    output
        .lines()
        .any(|line| line.trim() == "state = running" || line.trim_start().starts_with("pid = "))
}
#[cfg(any(target_os = "macos", test))]
fn parse_disabled_override(output: &str, label: &str) -> Option<bool> {
    output
        .lines()
        .find(|line| line.contains(&format!("\"{label}\"")))
        .and_then(|line| {
            let value = line.split_once("=>")?.1.trim().trim_end_matches([',', ';']);
            match value {
                "true" | "disabled" => Some(true),
                "false" | "enabled" => Some(false),
                _ => None,
            }
        })
}
#[cfg(target_os = "macos")]
fn mac_status(p: Provider) -> Result<Value> {
    let domain = mac_domain()?;
    let target = format!("{domain}/{}", label(p));
    let loaded = mac_call(&["print", &target])?;
    let overrides = mac_call(&["print-disabled", &domain])?;
    let disabled = if overrides.success {
        parse_disabled_override(&overrides.stdout, &label(p))
    } else {
        None
    };
    let path = plist_path(p)?;
    safe_path(&path)?;
    Ok(
        json!({"os":"macos","plist_present":path.exists(),"loaded":if loaded.success{Some(true)}else if loaded.code==Some(113) {Some(false)}else{None},"enabled":disabled.map(|d|!d),"enabled_override":disabled.map(|d|!d),"running":loaded.success && job_running(&loaded.stdout),"unload_deferred":disabled==Some(true) && loaded.success && job_running(&loaded.stdout),"loaded_query_success":loaded.success,"disabled_query_success":overrides.success}),
    )
}
#[cfg(any(target_os = "windows", test))]
const WINDOWS_WRAPPER: &str = r#"$ErrorActionPreference='Stop'
$config=Get-Content -LiteralPath ([IO.Path]::ChangeExtension($PSCommandPath,'.json')) -Encoding UTF8 -Raw | ConvertFrom-Json
if(-not $config.enabled){exit 0}
$env:CODEX_HOME=$config.environment.CODEX_HOME
$env:CLAUDE_CONFIG_DIR=$config.environment.CLAUDE_CONFIG_DIR
& $config.executable --state-dir $config.state run --scheduled $config.provider
exit $LASTEXITCODE
"#;
#[cfg(target_os = "windows")]
fn windows_enable(c: &TaskConfig) -> Result<()> {
    let sid = crate::platform::powershell(
        "[Security.Principal.WindowsIdentity]::GetCurrent().User.Value",
        &[],
    )?;
    if !sid.success {
        bail!("Current Windows SID unavailable");
    }
    let wrapper = c
        .state
        .join("schedule")
        .join(format!("{}.ps1", c.provider.name()));
    private_write(&wrapper, WINDOWS_WRAPPER.as_bytes())?;
    let spec = c
        .state
        .join("schedule")
        .join(format!("{}.xml", c.provider.name()));
    private_write(
        &spec,
        windows_spec(
            c,
            sid.stdout.trim(),
            &next_boundary(chrono::Local::now().naive_local())?,
        )?
        .as_bytes(),
    )?;
    let o = crate::platform::powershell(
        "$xml=Get-Content -LiteralPath $env:AICLEAN_TASK_XML -Encoding UTF8 -Raw; Register-ScheduledTask -TaskName $env:AICLEAN_TASK_NAME -TaskPath '\\' -Xml $xml -Force | Out-Null",
        &[
            ("AICLEAN_TASK_XML".into(), spec.into_os_string()),
            ("AICLEAN_TASK_NAME".into(), label(c.provider).into()),
        ],
    )?;
    if !o.success {
        bail!("Windows task registration failed");
    }
    Ok(())
}
#[cfg(target_os = "windows")]
fn windows_stop(p: Provider) -> Result<()> {
    let o = crate::platform::powershell(
        "$t=$null; try{$t=Get-ScheduledTask -TaskName $env:AICLEAN_TASK_NAME -TaskPath '\\' -ErrorAction Stop}catch{if($_.FullyQualifiedErrorId -notlike 'CmdletizationQuery_NotFound*'){throw}}; if($null -ne $t){Disable-ScheduledTask -InputObject $t | Out-Null; Unregister-ScheduledTask -InputObject $t -Confirm:$false}",
        &[("AICLEAN_TASK_NAME".into(), label(p).into())],
    )?;
    if !o.success {
        bail!("Windows task removal failed; stop signal remains set");
    }
    Ok(())
}
#[cfg(target_os = "windows")]
fn windows_status(p: Provider) -> Result<Value> {
    let o = crate::platform::powershell(
        "$t=$null; try{$t=Get-ScheduledTask -TaskName $env:AICLEAN_TASK_NAME -TaskPath '\\' -ErrorAction Stop}catch{if($_.FullyQualifiedErrorId -notlike 'CmdletizationQuery_NotFound*'){throw}}; if($null -eq $t){@{registered=$false;enabled=$null;state=$null}|ConvertTo-Json -Compress}else{@{registered=$true;enabled=$t.Settings.Enabled;state=[string]$t.State;next_run=(Get-ScheduledTaskInfo -InputObject $t).NextRunTime.ToString('yyyy-MM-ddTHH:mm:ss')}|ConvertTo-Json -Compress}",
        &[("AICLEAN_TASK_NAME".into(), label(p).into())],
    )?;
    if !o.success {
        return Ok(
            json!({"registered":null,"enabled":null,"query_success":false,"query_exit_code":o.code}),
        );
    }
    Ok(serde_json::from_str(o.stdout.trim())?)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    #[cfg(target_os = "windows")]
    #[ignore = "Native Task Scheduler fixture, explicitly creates and removes only a unique no-run test task"]
    fn native_windows_task_xml_roundtrip_without_running() {
        let temp = tempfile::tempdir().unwrap();
        let fixture_state = temp.path().join("fixture state 日本語");
        fs::create_dir(&fixture_state).unwrap();
        let state = fixture_state.canonicalize().unwrap();
        let config = TaskConfig {
            provider: Provider::Codex,
            executable: std::env::current_exe().unwrap(),
            state: state.clone(),
            environment: Default::default(),
            enabled: true,
        };
        let sid = crate::platform::powershell(
            "[Security.Principal.WindowsIdentity]::GetCurrent().User.Value",
            &[],
        )
        .unwrap();
        assert!(sid.success);
        let sid = sid.stdout.trim();
        assert!(sid.starts_with("S-1-"));
        // No cleanup action is available even if a host clock is unexpectedly advanced.
        let wrapper = state.join("schedule").join("codex.ps1");
        private_write(&wrapper, b"exit 0\r\n").unwrap();
        let spec = state.join("fixture.xml");
        private_write(
            &spec,
            windows_spec(&config, sid, "2100-01-10T03:30:00")
                .unwrap()
                .as_bytes(),
        )
        .unwrap();
        let task_name = format!("local.aiClean.fixture.{}", uuid::Uuid::new_v4());
        eprintln!("aiClean native Task Scheduler fixture task: {task_name}");
        let result = crate::platform::powershell(
            NATIVE_TASK_FIXTURE,
            &[
                ("AICLEAN_TASK_NAME".into(), task_name.into()),
                ("AICLEAN_TASK_XML".into(), spec.into_os_string()),
                ("AICLEAN_FIXTURE_SID".into(), sid.into()),
                ("AICLEAN_FIXTURE_WRAPPER".into(), wrapper.into_os_string()),
            ],
        )
        .unwrap();
        assert!(
            result.success,
            "Native Task Scheduler fixture failed: {}",
            result.stdout
        );
        let report: Value = serde_json::from_str(result.stdout.trim()).unwrap();
        assert_eq!(report["success"], true);
        assert_eq!(report["removed"], true);
        assert_eq!(report["never_run"], true);
    }
    #[cfg(target_os = "windows")]
    const NATIVE_TASK_FIXTURE: &str = r#"
$name=$env:AICLEAN_TASK_NAME; $sid=$env:AICLEAN_FIXTURE_SID;
if($name -notmatch '^local\.aiClean\.fixture\.[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$'){throw 'Invalid fixture task name'}
[xml]$expected=Get-Content -LiteralPath $env:AICLEAN_TASK_XML -Encoding UTF8 -Raw;
$exec=$expected.Task.Actions.Exec; $expectedCommand=[string]$exec.Command; $expectedArgs=[string]$exec.Arguments;
if([datetime]$expected.Task.Triggers.CalendarTrigger.StartBoundary -le [datetime]::Now.AddYears(1)){throw 'Fixture boundary is not safely in future'}
function Get-FixtureTask {
  try { Get-ScheduledTask -TaskName $name -TaskPath '\' -ErrorAction Stop }
  catch { if($_.FullyQualifiedErrorId -notlike 'CmdletizationQuery_NotFound*'){throw} }
}
function Get-OwnedFixtureTask {
  $t=Get-FixtureTask; if($null -eq $t){throw 'Fixture task missing'}
  if($t.TaskName -cne $name -or $t.TaskPath -cne '\' -or @($t.Actions).Count -ne 1 -or $t.Actions[0].Execute -cne $expectedCommand -or $t.Actions[0].Arguments -cne $expectedArgs){throw 'Fixture action ownership mismatch'}
  $user=[string]$t.Principal.UserId;
  if($user -notlike 'S-1-*'){$user=(New-Object Security.Principal.NTAccount($user)).Translate([Security.Principal.SecurityIdentifier]).Value}
  if($user -cne $sid){throw 'Fixture principal ownership mismatch'}
  if([string]$t.State -eq 'Running'){throw 'Fixture unexpectedly running'}
  $info=Get-ScheduledTaskInfo -InputObject $t;
  if($info.LastRunTime.Year -ge 2000){throw 'Fixture unexpectedly ran'}
  return $t
}
$created=$false; $removed=$false; $failure=$null; $message=$null; $hresult=$null; $settings=$null; $stage='collision check';
try {
  if($null -ne (Get-FixtureTask)){throw 'Fixture task collision'}
  $stage='invoke harmless fixture wrapper'; & $expectedCommand -NoLogo -NoProfile -NonInteractive -ExecutionPolicy Bypass -File $env:AICLEAN_FIXTURE_WRAPPER 2>$null; if($LASTEXITCODE -ne 0){throw 'Harmless fixture wrapper invocation failed'}
  $stage='register generated XML'; Register-ScheduledTask -TaskName $name -TaskPath '\' -Xml $expected.OuterXml | Out-Null; $created=$true;
  $stage='export schema and principal'; $t=Get-OwnedFixtureTask;
  [xml]$actual=Export-ScheduledTask -TaskName $name -TaskPath '\';
  $settings=@{logon_type=[string]$actual.Task.Principals.Principal.LogonType;run_level=[string]$actual.Task.Principals.Principal.RunLevel;cim_run_level=[string]$t.Principal.RunLevel;execution_time_limit=[string]$actual.Task.Settings.ExecutionTimeLimit;allow_hard_terminate=[string]$actual.Task.Settings.AllowHardTerminate;start_when_available=[string]$actual.Task.Settings.StartWhenAvailable};
  foreach($key in @($settings.Keys)){if($settings[$key].Length -gt 128){$settings[$key]=$settings[$key].Substring(0,128)}}
  $trigger=$actual.Task.Triggers.CalendarTrigger;
  if(@($actual.Task.Triggers.ChildNodes).Count -ne 1 -or $null -eq $trigger -or [datetime]$trigger.StartBoundary -le [datetime]::Now.AddYears(1) -or [string]$trigger.ScheduleByMonth.DaysOfMonth.Day -ne '10' -or @($trigger.ScheduleByMonth.Months.ChildNodes).Count -ne 12){throw 'Fixture calendar schema mismatch'}
  $xmlRunLevel=[string]$actual.Task.Principals.Principal.RunLevel;
  if($null -eq $t.Principal.RunLevel -or [int]$t.Principal.RunLevel -ne 0 -or ($xmlRunLevel -ne '' -and $xmlRunLevel -ne 'LeastPrivilege')){throw 'Fixture least privilege mismatch'}
  if([string]$actual.Task.Principals.Principal.LogonType -ne 'InteractiveToken' -or [string]$actual.Task.Settings.ExecutionTimeLimit -ne 'PT0S' -or [string]$actual.Task.Settings.AllowHardTerminate -ne 'false' -or [string]$actual.Task.Settings.StartWhenAvailable -ne 'true'){throw 'Fixture settings schema mismatch'}
  $stage='disable'; Disable-ScheduledTask -InputObject $t | Out-Null; $t=Get-OwnedFixtureTask; if($t.Settings.Enabled){throw 'Disable was not persistent'}
  $stage='enable'; Enable-ScheduledTask -InputObject $t | Out-Null; $t=Get-OwnedFixtureTask; if(-not $t.Settings.Enabled){throw 'Enable was not persistent'}
} catch {$failure=$stage; $message=[string]$_.Exception.Message; if($message.Length -gt 1024){$message=$message.Substring(0,1024)}; $hresult=$_.Exception.HResult}
finally {
  if($created){try {$t=Get-OwnedFixtureTask; Unregister-ScheduledTask -InputObject $t -Confirm:$false; if($null -ne (Get-FixtureTask)){throw 'Fixture still registered'}; $removed=$true} catch {$failure='owned fixture cleanup'; $message=[string]$_.Exception.Message; if($message.Length -gt 1024){$message=$message.Substring(0,1024)}; $hresult=$_.Exception.HResult}}
}
@{success=($null -eq $failure);failed_stage=$failure;exception_message=$message;exception_hresult=$hresult;actual_settings=$settings;removed=$removed;never_run=($null -eq $failure)} | ConvertTo-Json -Compress;
if($null -ne $failure){exit 1}
"#;
    #[test]
    #[cfg(target_os = "windows")]
    fn canonical_verbatim_root_is_validated_as_an_absolute_root() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        assert!(
            matches!(root.components().next(),Some(Component::Prefix(prefix)) if prefix.kind().is_verbatim())
        );
        safe_path(&root).unwrap();
        safe_path(&root.join("not-created-yet").join("receipt.json")).unwrap();
    }
    #[test]
    fn windows_wrapper_reads_utf8_config_as_data() {
        assert!(WINDOWS_WRAPPER.contains("-Encoding UTF8 -Raw | ConvertFrom-Json"));
        assert!(WINDOWS_WRAPPER.contains("run --scheduled $config.provider"));
        assert!(!WINDOWS_WRAPPER.contains("Invoke-Expression"));
    }
    #[test]
    fn stop_racing_enable_retains_token_after_scheduler_lock() {
        let temp = tempfile::tempdir().unwrap();
        let state = temp.path().canonicalize().unwrap();
        let guard = scheduler_lock(&state).unwrap();
        let other = state.clone();
        let waiter = std::thread::spawn(move || {
            let _guard = request_stop_and_lock(&other, Provider::Codex).unwrap();
        });
        let path = stop_path(&state, Provider::Codex);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while !path.exists() {
            assert!(std::time::Instant::now() < deadline);
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        let initial = fs::read(&path).unwrap();
        // Simulate enable's final clear while it holds the shared scheduler lock.
        fs::remove_file(&path).unwrap();
        drop(guard);
        waiter.join().unwrap();
        assert_eq!(fs::read(&path).unwrap(), initial);
    }
    #[test]
    fn task_ownership_requires_exact_state_executable_and_provider() {
        let spec = TaskConfig {
            provider: Provider::Codex,
            executable: "/opt/aiClean".into(),
            state: "/users/owner/.aiClean".into(),
            environment: Default::default(),
            enabled: true,
        };
        let xml = launchd_spec(&spec).unwrap();
        assert!(plist_owned(xml.as_bytes(), &spec).unwrap());
        let mut disabled = spec.clone();
        disabled.enabled = false;
        assert!(plist_owned(launchd_spec(&disabled).unwrap().as_bytes(), &spec).unwrap());
        let output = "arguments = {\n/opt/aiClean\n--state-dir\n/users/owner/.aiClean\nrun\n--scheduled\ncodex\n}\n";
        assert!(loaded_action_owned(output, &spec).unwrap());
        for (mut unrelated, field) in [
            (spec.clone(), "state"),
            (spec.clone(), "executable"),
            (spec.clone(), "provider"),
        ] {
            match field {
                "state" => unrelated.state = "/users/other/.aiClean".into(),
                "executable" => unrelated.executable = "/opt/other".into(),
                _ => unrelated.provider = Provider::Claude,
            };
            assert!(!plist_owned(xml.as_bytes(), &unrelated).unwrap());
            assert!(!loaded_action_owned(output, &unrelated).unwrap());
        }
    }
    #[test]
    fn artifact_removal_preserves_other_receipts_and_refuses_directory() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let owned = root.join("codex.json");
        let receipt = root.join("operation-receipt.json");
        fs::write(&owned, b"owned").unwrap();
        fs::write(&receipt, b"durable receipt").unwrap();
        remove_owned_file(&owned).unwrap();
        assert_eq!(fs::read(receipt).unwrap(), b"durable receipt");
        assert!(remove_owned_file(&root).is_err());
    }
    #[test]
    #[cfg(any(target_os = "macos", target_os = "windows"))]
    fn enable_refuses_busy_lock_before_writing_or_registering() {
        let temp = tempfile::tempdir().unwrap();
        let state = temp.path().canonicalize().unwrap();
        let _lock = crate::storage::maintenance_lock(&state).unwrap();
        assert!(enable(&state, Provider::Codex).is_err());
        assert!(!config_path(&state, Provider::Codex).exists());
    }
    #[test]
    fn launchd_actual_override_words_and_boolean_forms() {
        for word in ["true", "disabled"] {
            assert_eq!(
                parse_disabled_override(
                    &format!("\"local.aiClean.codex\" => {word}"),
                    "local.aiClean.codex"
                ),
                Some(true)
            );
        }
        for word in ["false", "enabled"] {
            assert_eq!(
                parse_disabled_override(
                    &format!("\"local.aiClean.codex\" => {word}"),
                    "local.aiClean.codex"
                ),
                Some(false)
            );
        }
        assert_eq!(
            parse_disabled_override("\"local.aiClean.claude\" => enabled", "local.aiClean.codex"),
            None
        );
    }
    #[test]
    #[cfg(unix)]
    fn shared_parent_permissions_are_preserved() {
        use std::os::unix::fs::PermissionsExt;
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        fs::set_permissions(&root, fs::Permissions::from_mode(0o755)).unwrap();
        private_write_inner(&root.join("owned.plist"), b"fixture", false).unwrap();
        assert_eq!(
            fs::metadata(&root).unwrap().permissions().mode() & 0o777,
            0o755
        );
        assert_eq!(
            fs::metadata(root.join("owned.plist"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }
    #[test]
    fn injected_registration_never_runs_task() {
        let mut calls = Vec::new();
        let mut runner = |args: &[&str]| {
            calls.push(args.iter().map(|s| s.to_string()).collect::<Vec<_>>());
            Ok(CommandOutput {
                success: args[0] != "print",
                code: Some(if args[0] == "print" { 113 } else { 0 }),
                stdout: String::new(),
            })
        };
        register_launchd(
            "gui/501",
            "local.aiClean.codex",
            "/private/fixture.plist",
            &mut runner,
        )
        .unwrap();
        assert_eq!(
            calls.iter().map(|c| c[0].as_str()).collect::<Vec<_>>(),
            vec!["print", "enable", "bootstrap"]
        );
    }
    #[test]
    fn injected_registration_refuses_running_job_without_killing() {
        let mut calls = Vec::new();
        let mut runner = |args: &[&str]| {
            calls.push(args[0].to_string());
            Ok(CommandOutput {
                success: true,
                code: Some(0),
                stdout: "state = running\npid = 123\n".to_string(),
            })
        };
        assert!(
            register_launchd(
                "gui/501",
                "local.aiClean.codex",
                "/private/fixture.plist",
                &mut runner
            )
            .is_err()
        );
        assert_eq!(calls, vec!["print"]);
    }
    #[test]
    fn first_windows_boundary_is_in_future() {
        let date = chrono::NaiveDate::from_ymd_opt(2026, 10, 10)
            .unwrap()
            .and_hms_opt(3, 30, 0)
            .unwrap();
        assert_eq!(next_boundary(date).unwrap(), "2026-11-10T03:30:00");
        assert_eq!(
            next_boundary(date - chrono::Duration::minutes(1)).unwrap(),
            "2026-10-10T03:30:00"
        );
    }
    #[test]
    fn launchd_is_monthly_without_immediate_or_login_run() {
        let spec = TaskConfig {
            provider: Provider::Codex,
            executable: "/opt/a & b/aiClean".into(),
            state: "/tmp/state".into(),
            environment: Default::default(),
            enabled: true,
        };
        let xml = launchd_spec(&spec).unwrap();
        assert!(xml.contains("<key>Day</key><integer>10</integer>"));
        assert!(xml.contains("<key>RunAtLoad</key><false/>"));
        assert!(xml.contains("a &amp; b"));
        assert!(xml.contains("--state-dir"));
        assert!(xml.contains("--scheduled"));
        assert!(!xml.contains("StartInterval"));
    }
    #[test]
    fn windows_is_monthly_interactive_no_logon_trigger() {
        let spec = TaskConfig {
            provider: Provider::Claude,
            executable: "C:\\Users\\a b\\aiClean.exe".into(),
            state: "C:\\state".into(),
            environment: Default::default(),
            enabled: true,
        };
        let xml = windows_spec(&spec, "S-1-5-21-test", "2026-10-10T03:30:00").unwrap();
        // Registration consumes a Unicode COM BSTR even when our file is UTF-8.
        assert!(xml.starts_with("<?xml version=\"1.0\"?>"));
        assert!(!xml.contains("encoding="));
        assert!(xml.contains("<Day>10</Day>"));
        assert!(xml.contains("<LogonType>InteractiveToken</LogonType>"));
        assert!(!xml.contains("LogonTrigger"));
        assert!(xml.contains("03:30:00"));
        assert!(xml.contains("<ExecutionTimeLimit>PT0S</ExecutionTimeLimit>"));
        assert!(xml.contains("<AllowHardTerminate>false</AllowHardTerminate>"));
        assert!(xml.contains("-ExecutionPolicy Bypass -File"));
    }
    #[test]
    fn symlink_config_root_refused() {
        #[cfg(unix)]
        {
            let t = tempfile::tempdir().unwrap();
            std::os::unix::fs::symlink(t.path(), t.path().join("link")).unwrap();
            assert!(safe_path(&t.path().join("link")).is_err());
        }
    }
}
