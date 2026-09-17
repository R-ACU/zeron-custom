//! Start at login: per-user registrations that launch the Zeron window and/or
//! the background engine (`zeron headless`) when the user signs in.
//!
//! The registration itself is the source of truth — nothing is mirrored into
//! `ui-settings.json`, so an entry removed in Task Manager or by
//! `zeron daemon uninstall` shows up correctly the next time Settings opens.
//!
//! - **Windows**: string values under
//!   `HKCU\Software\Microsoft\Windows\CurrentVersion\Run` ("Zeron" and
//!   "Zeron Engine"). Per-user, so no elevation is needed (unlike the
//!   `ONLOGON` Scheduled Task `zeron daemon install` creates). Both launch
//!   through `%SystemRoot%\System32\conhost.exe --headless` — the same trick as
//!   the daemon's task action — because `zeron.exe` is a console-subsystem
//!   binary and would otherwise flash a console window at sign-in. Task
//!   Manager's "Startup apps" switch lives next door under
//!   `Explorer\StartupApproved\Run`; it is read (a disabled entry shows as off)
//!   and cleared when the entry is switched on or off here.
//! - **Linux**: XDG autostart entries in `$XDG_CONFIG_HOME/autostart`.
//! - **macOS**: not supported yet ([`is_supported`] is false).
//!
//! Engine single-instance semantics (`zeron_engine::InstanceLock`): a second
//! engine for the same data dir exits right away, so a duplicate background
//! start (for example next to the legacy "Zeron" Scheduled Task) is harmless
//! but pointless. The page therefore treats an installed daemon service as
//! "engine already starts at login" instead of registering a second start.
//!
//! Everything here blocks (registry calls, a `schtasks` probe, file IO): call
//! it off the UI thread.

use std::path::{Path, PathBuf};

/// One thing that can start at login.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LoginItem {
    /// The headed app window.
    App,
    /// `zeron headless`, hidden.
    Engine,
}

impl LoginItem {
    pub const ALL: [LoginItem; 2] = [LoginItem::App, LoginItem::Engine];

    /// The `HKCU\…\Run` value name on Windows.
    pub const fn registry_value_name(self) -> &'static str {
        match self {
            LoginItem::App => "Zeron",
            LoginItem::Engine => "Zeron Engine",
        }
    }

    /// The XDG autostart file name on Linux.
    pub const fn desktop_file_name(self) -> &'static str {
        match self {
            LoginItem::App => "zeron.desktop",
            LoginItem::Engine => "zeron-engine.desktop",
        }
    }

    /// Arguments passed to `zeron` by the login registration.
    pub const fn args(self) -> &'static [&'static str] {
        match self {
            LoginItem::App => &[AUTOSTART_FLAG],
            LoginItem::Engine => &["headless"],
        }
    }
}

/// Marks a headed launch that came from the login registration. `main` uses
/// it to give a background engine that is starting at the same moment the
/// chance to own the data dir first, so the window attaches to it instead of
/// embedding an engine that would stop when the window closes.
pub const AUTOSTART_FLAG: &str = "--autostart";

/// The state of one login registration.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ItemStatus {
    /// A registration exists.
    pub registered: bool,
    /// The registration exists but the OS was told to skip it (Task Manager
    /// "Startup apps" on Windows, `Hidden=true` on Linux).
    pub disabled_by_system: bool,
    /// The registration launches a different executable than the running one
    /// (e.g. a dev build vs. the installed copy). Switching the item on again
    /// rewrites it for the running executable.
    pub other_executable: Option<String>,
}

impl ItemStatus {
    /// Whether this item actually starts at the next sign-in.
    pub fn enabled(&self) -> bool {
        self.registered && !self.disabled_by_system
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AutostartStatus {
    pub app: ItemStatus,
    pub engine: ItemStatus,
    /// A `zeron daemon install` service exists (Windows: the "Zeron" Scheduled
    /// Task; Linux: the `zeron.service` systemd user unit), which already
    /// starts the engine on its own.
    pub engine_service_installed: bool,
}

impl AutostartStatus {
    pub fn item(&self, item: LoginItem) -> &ItemStatus {
        match item {
            LoginItem::App => &self.app,
            LoginItem::Engine => &self.engine,
        }
    }

    /// Whether some mechanism starts the background engine at sign-in.
    pub fn engine_starts_at_login(&self) -> bool {
        self.engine.enabled() || self.engine_service_installed
    }
}

/// Whether this platform has a start-at-login implementation.
pub const fn is_supported() -> bool {
    cfg!(any(windows, target_os = "linux"))
}

/// Read the live registrations. Blocking.
pub fn read_status() -> Result<AutostartStatus, String> {
    platform::read_status()
}

/// Register (`enabled`) or remove the login start for `item`, pointing it at
/// the running executable. Blocking.
pub fn set_enabled(item: LoginItem, enabled: bool) -> Result<(), String> {
    platform::set_enabled(item, enabled)
}

/// Remove the `zeron daemon install` service registration (Windows: the
/// "Zeron" Scheduled Task; Linux: the systemd user unit) by running the
/// running executable's own `daemon uninstall`.
///
/// That code lives in the binary crate, which depends on this one, so the UI
/// cannot call it directly. Spawned without a shell and without a console
/// window; judged by the exit code alone (`schtasks` output is localized).
/// Blocking — call it off the UI thread.
pub fn remove_engine_service() -> Result<(), String> {
    let exe = current_exe()?;
    let mut command = std::process::Command::new(&exe);
    command
        .args(["daemon", "uninstall"])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        command.creation_flags(CREATE_NO_WINDOW);
    }
    let status = command
        .status()
        .map_err(|err| format!("Could not run \"zeron daemon uninstall\": {err}"))?;
    if status.success() {
        Ok(())
    } else {
        Err("Could not remove the Zeron background task.".to_string())
    }
}

#[cfg_attr(not(any(windows, target_os = "linux")), allow(dead_code))]
fn current_exe() -> Result<PathBuf, String> {
    std::env::current_exe().map_err(|err| format!("Could not resolve the Zeron executable: {err}"))
}

// ---------------------------------------------------------------------------
// Windows command lines (pure; unit-tested on every OS)
// ---------------------------------------------------------------------------

/// `%SystemRoot%\System32\conhost.exe`, falling back to `C:\Windows` when the
/// variable is missing. An absolute path, so the Run entry never depends on
/// the logon PATH.
pub fn windows_conhost_path(system_root: Option<&str>) -> String {
    let root = system_root
        .map(|root| root.trim().trim_end_matches(['\\', '/']))
        .filter(|root| !root.is_empty())
        .unwrap_or(r"C:\Windows");
    format!(r"{root}\System32\conhost.exe")
}

/// Quote one argument so `CommandLineToArgvW` / the MSVC runtime parse it
/// back verbatim. Always quoted (paths with spaces are the common case);
/// backslashes are doubled only where they precede a quote.
pub fn windows_quote_arg(arg: &str) -> String {
    let mut out = String::with_capacity(arg.len() + 2);
    out.push('"');
    let mut backslashes = 0usize;
    for ch in arg.chars() {
        match ch {
            '\\' => backslashes += 1,
            '"' => {
                out.extend(std::iter::repeat_n('\\', backslashes * 2 + 1));
                out.push('"');
                backslashes = 0;
            }
            _ => {
                out.extend(std::iter::repeat_n('\\', backslashes));
                out.push(ch);
                backslashes = 0;
            }
        }
    }
    // Trailing backslashes precede the closing quote.
    out.extend(std::iter::repeat_n('\\', backslashes * 2));
    out.push('"');
    out
}

/// The Run-value command for `item`:
/// `"<conhost>" --headless "<zeron.exe>" <args…>`.
pub fn windows_command_line(conhost: &str, exe: &str, item: LoginItem) -> String {
    let mut line = format!(
        "{} --headless {}",
        windows_quote_arg(conhost),
        windows_quote_arg(exe)
    );
    for arg in item.args() {
        line.push(' ');
        line.push_str(arg);
    }
    line
}

/// Split a command line the way `CommandLineToArgvW` does for arguments
/// (2n backslashes + quote → n backslashes and a quote toggle; 2n+1 → n
/// backslashes and a literal quote; `""` inside quotes → literal quote).
pub fn split_windows_command_line(line: &str) -> Vec<String> {
    let mut args = Vec::new();
    let mut chars = line.chars().peekable();
    loop {
        while chars.peek().is_some_and(|c| *c == ' ' || *c == '\t') {
            chars.next();
        }
        if chars.peek().is_none() {
            break;
        }
        let mut arg = String::new();
        let mut quoted = false;
        while let Some(&ch) = chars.peek() {
            match ch {
                ' ' | '\t' if !quoted => break,
                '\\' => {
                    let mut backslashes = 0usize;
                    while chars.peek() == Some(&'\\') {
                        chars.next();
                        backslashes += 1;
                    }
                    if chars.peek() == Some(&'"') {
                        arg.extend(std::iter::repeat_n('\\', backslashes / 2));
                        if backslashes % 2 == 1 {
                            arg.push('"');
                            chars.next();
                        }
                    } else {
                        arg.extend(std::iter::repeat_n('\\', backslashes));
                    }
                }
                '"' => {
                    chars.next();
                    if quoted && chars.peek() == Some(&'"') {
                        chars.next();
                        arg.push('"');
                    } else {
                        quoted = !quoted;
                    }
                }
                _ => {
                    arg.push(ch);
                    chars.next();
                }
            }
        }
        args.push(arg);
    }
    args
}

/// The executable a Run command ultimately launches: the argument after
/// `conhost.exe` and its `--` options when wrapped, otherwise the program.
pub fn windows_launched_executable(line: &str) -> Option<String> {
    let args = split_windows_command_line(line);
    let mut iter = args.into_iter();
    let program = iter.next()?;
    let file_name = program.rsplit(['\\', '/']).next().unwrap_or(&program);
    if file_name.eq_ignore_ascii_case("conhost.exe") || file_name.eq_ignore_ascii_case("conhost") {
        iter.find(|arg| !arg.starts_with("--"))
    } else {
        Some(program)
    }
}

/// Case-insensitive, separator-insensitive Windows path equality.
pub fn windows_same_path(a: &str, b: &str) -> bool {
    let normalize = |path: &str| {
        let path = path.strip_prefix(r"\\?\").unwrap_or(path);
        path.replace('/', "\\").to_lowercase()
    };
    normalize(a) == normalize(b)
}

/// `Explorer\StartupApproved\Run` data: 12 bytes whose first byte is even
/// (`02`/`06`) while enabled and odd (`03`/`07`) once the user switched the
/// entry off in Task Manager. Anything unrecognised counts as enabled.
pub fn startup_approved_disabled(data: &[u8]) -> bool {
    data.first().is_some_and(|flag| flag & 1 == 1)
}

// ---------------------------------------------------------------------------
// XDG autostart entries (pure; unit-tested on every OS)
// ---------------------------------------------------------------------------

/// Quote one `Exec=` argument per the Desktop Entry spec: double-quoted with
/// `"`, `` ` ``, `$` and `\` backslash-escaped, then the string-value escape
/// layer doubles every backslash again, and `%` becomes `%%`.
pub fn desktop_exec_quote(arg: &str) -> String {
    let mut out = String::from("\"");
    for ch in arg.chars() {
        match ch {
            '"' | '`' | '$' => {
                out.push_str("\\\\");
                out.push(ch);
            }
            '\\' => out.push_str("\\\\\\\\"),
            '%' => out.push_str("%%"),
            _ => out.push(ch),
        }
    }
    out.push('"');
    out
}

/// `~/.zeron/app/current/zeron` for an executable inside the curl|sh
/// installer's versioned app root (same rule as `zeron daemon install`'s
/// systemd unit), `None` otherwise.
pub fn installer_exe_path(exe: &Path, home: Option<&Path>) -> Option<PathBuf> {
    let app_root = home?.join(".zeron/app");
    exe.starts_with(&app_root)
        .then(|| app_root.join("current").join("zeron"))
}

pub fn render_desktop_entry(exe: &Path, item: LoginItem) -> String {
    let mut exec = desktop_exec_quote(&exe.to_string_lossy());
    for arg in item.args() {
        exec.push(' ');
        exec.push_str(arg);
    }
    let (name, comment) = match item {
        LoginItem::App => ("Zeron", "Open Zeron at login"),
        LoginItem::Engine => (
            "Zeron Engine",
            "Run the Zeron engine in the background at login",
        ),
    };
    format!(
        "[Desktop Entry]\nType=Application\nName={name}\nComment={comment}\nExec={exec}\nTerminal=false\nNoDisplay=true\nX-GNOME-Autostart-enabled=true\n"
    )
}

/// The program (first `Exec=` argument, unescaped) of an autostart entry and
/// whether the entry is switched off (`Hidden=true` or
/// `X-GNOME-Autostart-enabled=false`).
pub fn parse_desktop_entry(content: &str) -> (Option<String>, bool) {
    let mut program = None;
    let mut disabled = false;
    let mut in_main_group = false;
    for line in content.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            in_main_group = line == "[Desktop Entry]";
            continue;
        }
        if !in_main_group {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        match key.trim() {
            "Exec" => program = desktop_exec_program(value.trim()),
            "Hidden" => disabled |= value.trim().eq_ignore_ascii_case("true"),
            "X-GNOME-Autostart-enabled" => disabled |= value.trim().eq_ignore_ascii_case("false"),
            _ => {}
        }
    }
    (program, disabled)
}

fn desktop_exec_program(value: &str) -> Option<String> {
    // String-value layer first (`\\` → `\`, `\s` → space), then the Exec
    // quoting layer on the result.
    let mut unescaped = String::with_capacity(value.len());
    let mut chars = value.chars();
    while let Some(ch) = chars.next() {
        if ch == '\\' {
            match chars.next() {
                Some('s') => unescaped.push(' '),
                Some('t') => unescaped.push('\t'),
                Some('n') => unescaped.push('\n'),
                Some('r') => unescaped.push('\r'),
                Some('\\') => unescaped.push('\\'),
                // Not a string escape: the backslash belongs to the quoting
                // layer (`\"`, `\$`, …).
                Some(other) => {
                    unescaped.push('\\');
                    unescaped.push(other);
                }
                None => unescaped.push('\\'),
            }
        } else {
            unescaped.push(ch);
        }
    }
    let mut program = String::new();
    let mut quoted = false;
    let mut started = false;
    let mut chars = unescaped.chars();
    while let Some(ch) = chars.next() {
        match ch {
            '"' => {
                quoted = !quoted;
                started = true;
            }
            '\\' if quoted => {
                if let Some(next) = chars.next() {
                    program.push(next);
                }
            }
            ' ' | '\t' if !quoted => {
                if started {
                    break;
                }
            }
            _ => {
                program.push(ch);
                started = true;
            }
        }
    }
    let program = program.replace("%%", "%");
    (!program.is_empty()).then_some(program)
}

// ---------------------------------------------------------------------------
// Platform backends
// ---------------------------------------------------------------------------

#[cfg(windows)]
mod platform {
    use std::os::windows::process::CommandExt;
    use std::process::{Command, Stdio};

    use windows_sys::Win32::Foundation::{
        ERROR_FILE_NOT_FOUND, ERROR_MORE_DATA, ERROR_PATH_NOT_FOUND, ERROR_SUCCESS,
    };
    use windows_sys::Win32::System::Registry::{
        HKEY_CURRENT_USER, REG_SZ, RRF_RT_REG_BINARY, RRF_RT_REG_EXPAND_SZ, RRF_RT_REG_SZ,
        RegDeleteKeyValueW, RegGetValueW, RegSetKeyValueW,
    };

    use super::{AutostartStatus, ItemStatus, LoginItem};

    const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
    const STARTUP_APPROVED_KEY: &str =
        r"Software\Microsoft\Windows\CurrentVersion\Explorer\StartupApproved\Run";
    /// The Scheduled Task `zeron daemon install` registers.
    const SCHEDULED_TASK_NAME: &str = "Zeron";
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;

    pub(super) fn read_status() -> Result<AutostartStatus, String> {
        let exe = super::current_exe()?;
        let exe = exe.to_string_lossy().into_owned();
        let item = |item: LoginItem| -> Result<ItemStatus, String> {
            let name = item.registry_value_name();
            let Some(command) = read_value(RUN_KEY, name, RRF_RT_REG_SZ | RRF_RT_REG_EXPAND_SZ)?
            else {
                return Ok(ItemStatus::default());
            };
            let command = wide_bytes_to_string(&command);
            let disabled_by_system = read_value(STARTUP_APPROVED_KEY, name, RRF_RT_REG_BINARY)
                .ok()
                .flatten()
                .is_some_and(|data| super::startup_approved_disabled(&data));
            let other_executable = match super::windows_launched_executable(&command) {
                Some(target) if super::windows_same_path(&target, &exe) => None,
                Some(target) => Some(target),
                None => Some(command.clone()),
            };
            Ok(ItemStatus {
                registered: true,
                disabled_by_system,
                other_executable,
            })
        };
        Ok(AutostartStatus {
            app: item(LoginItem::App)?,
            engine: item(LoginItem::Engine)?,
            engine_service_installed: scheduled_task_installed(),
        })
    }

    pub(super) fn set_enabled(item: LoginItem, enabled: bool) -> Result<(), String> {
        let name = item.registry_value_name();
        if enabled {
            let exe = super::current_exe()?;
            let system_root = std::env::var("SystemRoot").ok();
            let conhost = super::windows_conhost_path(system_root.as_deref());
            let command = super::windows_command_line(&conhost, &exe.to_string_lossy(), item);
            write_string(RUN_KEY, name, &command)?;
        } else {
            delete_value(RUN_KEY, name)?;
        }
        // Either way the Task Manager override no longer applies: switching on
        // here means "start", switching off removed the entry it described.
        delete_value(STARTUP_APPROVED_KEY, name)?;
        Ok(())
    }

    /// Existence of the legacy Scheduled Task, by `schtasks` exit code only
    /// (its output is localized).
    fn scheduled_task_installed() -> bool {
        Command::new("schtasks")
            .args(["/Query", "/TN", SCHEDULED_TASK_NAME])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .creation_flags(CREATE_NO_WINDOW)
            .status()
            .map(|status| status.success())
            .unwrap_or(false)
    }

    /// A missing value, or a missing key (e.g. no `StartupApproved\Run` yet).
    fn is_not_found(status: u32) -> bool {
        status == ERROR_FILE_NOT_FOUND || status == ERROR_PATH_NOT_FOUND
    }

    fn wide(value: &str) -> Vec<u16> {
        value.encode_utf16().chain(std::iter::once(0)).collect()
    }

    fn wide_bytes_to_string(bytes: &[u8]) -> String {
        let units: Vec<u16> = bytes
            .chunks_exact(2)
            .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
            .collect();
        let len = units
            .iter()
            .position(|unit| *unit == 0)
            .unwrap_or(units.len());
        String::from_utf16_lossy(&units[..len])
    }

    /// Raw value bytes under HKCU, `None` when the key or value is absent.
    fn read_value(subkey: &str, name: &str, flags: u32) -> Result<Option<Vec<u8>>, String> {
        let subkey_w = wide(subkey);
        let name_w = wide(name);
        // Start with a size probe (null data pointer), then read into a buffer
        // of the reported size. Retried because an expanded REG_EXPAND_SZ or a
        // value rewritten in between can need more than the probe said.
        let mut buffer: Vec<u8> = Vec::new();
        for _ in 0..4 {
            let mut size = buffer.len() as u32;
            let data: *mut core::ffi::c_void = if buffer.is_empty() {
                std::ptr::null_mut()
            } else {
                buffer.as_mut_ptr().cast()
            };
            // SAFETY: NUL-terminated strings; `data` is either null (the
            // documented size probe) or `buffer` with `size` its byte length.
            let status = unsafe {
                RegGetValueW(
                    HKEY_CURRENT_USER,
                    subkey_w.as_ptr(),
                    name_w.as_ptr(),
                    flags,
                    std::ptr::null_mut(),
                    data,
                    &mut size,
                )
            };
            if is_not_found(status) {
                return Ok(None);
            }
            if status == ERROR_SUCCESS && !buffer.is_empty() {
                buffer.truncate(size as usize);
                return Ok(Some(buffer));
            }
            if status == ERROR_SUCCESS || status == ERROR_MORE_DATA {
                // Headroom for a terminating NUL RegGetValueW may append.
                buffer = vec![0; size as usize + 64];
                continue;
            }
            return Err(format!(
                "Could not read the login entry '{name}' (Windows error {status})."
            ));
        }
        Err(format!(
            "Could not read the login entry '{name}' (it kept changing size)."
        ))
    }

    fn write_string(subkey: &str, name: &str, value: &str) -> Result<(), String> {
        let subkey_w = wide(subkey);
        let name_w = wide(name);
        let data = wide(value);
        let bytes = (data.len() * std::mem::size_of::<u16>()) as u32;
        // SAFETY: NUL-terminated strings; `data` includes its terminating NUL
        // and `bytes` is its size in bytes, as REG_SZ requires.
        let status = unsafe {
            RegSetKeyValueW(
                HKEY_CURRENT_USER,
                subkey_w.as_ptr(),
                name_w.as_ptr(),
                REG_SZ,
                data.as_ptr().cast(),
                bytes,
            )
        };
        if status != ERROR_SUCCESS {
            return Err(format!(
                "Could not register '{name}' to start at login (Windows error {status})."
            ));
        }
        Ok(())
    }

    fn delete_value(subkey: &str, name: &str) -> Result<(), String> {
        let subkey_w = wide(subkey);
        let name_w = wide(name);
        // SAFETY: NUL-terminated strings.
        let status =
            unsafe { RegDeleteKeyValueW(HKEY_CURRENT_USER, subkey_w.as_ptr(), name_w.as_ptr()) };
        if status != ERROR_SUCCESS && !is_not_found(status) {
            return Err(format!(
                "Could not remove the login entry '{name}' (Windows error {status})."
            ));
        }
        Ok(())
    }
}

#[cfg(target_os = "linux")]
mod platform {
    use std::path::PathBuf;

    use super::{AutostartStatus, ItemStatus, LoginItem};

    fn config_dir() -> Result<PathBuf, String> {
        if let Some(dir) = std::env::var_os("XDG_CONFIG_HOME").filter(|dir| !dir.is_empty()) {
            return Ok(PathBuf::from(dir));
        }
        std::env::var_os("HOME")
            .filter(|home| !home.is_empty())
            .map(|home| PathBuf::from(home).join(".config"))
            .ok_or_else(|| "HOME is not set.".to_string())
    }

    fn entry_path(item: LoginItem) -> Result<PathBuf, String> {
        Ok(config_dir()?
            .join("autostart")
            .join(item.desktop_file_name()))
    }

    /// The executable an entry should launch: the running one, except that a
    /// curl|sh installation (`~/.zeron/app/<version>/zeron`, which
    /// `current_exe` sees with the symlink resolved) points at the
    /// `app/current` symlink so an update never strands the entry.
    fn launch_exe() -> Result<PathBuf, String> {
        let exe = super::current_exe()?;
        let home = std::env::var_os("HOME").map(PathBuf::from);
        Ok(super::installer_exe_path(&exe, home.as_deref()).unwrap_or(exe))
    }

    pub(super) fn read_status() -> Result<AutostartStatus, String> {
        let exe = launch_exe()?;
        let item = |item: LoginItem| -> Result<ItemStatus, String> {
            let path = entry_path(item)?;
            let content = match std::fs::read_to_string(&path) {
                Ok(content) => content,
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                    return Ok(ItemStatus::default());
                }
                Err(err) => return Err(format!("Could not read {}: {err}", path.display())),
            };
            let (program, disabled_by_system) = super::parse_desktop_entry(&content);
            let other_executable = match program {
                Some(program) if std::path::Path::new(&program) == exe.as_path() => None,
                Some(program) => Some(program),
                None => Some("an entry without a command".to_string()),
            };
            Ok(ItemStatus {
                registered: true,
                disabled_by_system,
                other_executable,
            })
        };
        let service_installed = config_dir()
            .map(|dir| dir.join("systemd/user/zeron.service").exists())
            .unwrap_or(false);
        Ok(AutostartStatus {
            app: item(LoginItem::App)?,
            engine: item(LoginItem::Engine)?,
            engine_service_installed: service_installed,
        })
    }

    pub(super) fn set_enabled(item: LoginItem, enabled: bool) -> Result<(), String> {
        let path = entry_path(item)?;
        if enabled {
            let exe = launch_exe()?;
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)
                    .map_err(|err| format!("Could not create {}: {err}", parent.display()))?;
            }
            std::fs::write(&path, super::render_desktop_entry(&exe, item))
                .map_err(|err| format!("Could not write {}: {err}", path.display()))
        } else {
            match std::fs::remove_file(&path) {
                Ok(()) => Ok(()),
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
                Err(err) => Err(format!("Could not remove {}: {err}", path.display())),
            }
        }
    }
}

#[cfg(not(any(windows, target_os = "linux")))]
mod platform {
    use super::{AutostartStatus, LoginItem};

    pub(super) fn read_status() -> Result<AutostartStatus, String> {
        Err("Starting Zeron at login is not supported on this platform yet.".to_string())
    }

    pub(super) fn set_enabled(_item: LoginItem, _enabled: bool) -> Result<(), String> {
        Err("Starting Zeron at login is not supported on this platform yet.".to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn conhost_path_uses_system_root() {
        assert_eq!(
            windows_conhost_path(Some(r"D:\Win\")),
            r"D:\Win\System32\conhost.exe"
        );
        assert_eq!(
            windows_conhost_path(None),
            r"C:\Windows\System32\conhost.exe"
        );
        assert_eq!(
            windows_conhost_path(Some("  ")),
            r"C:\Windows\System32\conhost.exe"
        );
    }

    #[test]
    fn quoting_round_trips_through_the_argv_parser() {
        for arg in [
            r"C:\Users\Jane Doe\AppData\Local\Programs\Zeron\zeron.exe",
            r"C:\trailing\",
            r#"odd"quote"#,
            r#"back\"slash"#,
            "",
        ] {
            let quoted = windows_quote_arg(arg);
            assert_eq!(split_windows_command_line(&quoted), vec![arg.to_string()]);
        }
    }

    #[test]
    fn command_lines_wrap_both_items_in_headless_conhost() {
        let conhost = r"C:\Windows\System32\conhost.exe";
        let exe = r"C:\Users\Jane Doe\AppData\Local\Programs\Zeron\zeron.exe";
        assert_eq!(
            windows_command_line(conhost, exe, LoginItem::Engine),
            r#""C:\Windows\System32\conhost.exe" --headless "C:\Users\Jane Doe\AppData\Local\Programs\Zeron\zeron.exe" headless"#
        );
        let app = windows_command_line(conhost, exe, LoginItem::App);
        assert_eq!(
            split_windows_command_line(&app),
            vec![conhost, "--headless", exe, AUTOSTART_FLAG]
        );
        assert_eq!(windows_launched_executable(&app).as_deref(), Some(exe));
    }

    #[test]
    fn launched_executable_handles_legacy_and_bare_commands() {
        assert_eq!(
            windows_launched_executable(r#"conhost.exe --headless "C:\Z\zeron.exe" headless"#)
                .as_deref(),
            Some(r"C:\Z\zeron.exe")
        );
        assert_eq!(
            windows_launched_executable(r#""C:\Program Files\Zeron\zeron.exe""#).as_deref(),
            Some(r"C:\Program Files\Zeron\zeron.exe")
        );
        assert_eq!(windows_launched_executable("   "), None);
    }

    #[test]
    fn windows_paths_compare_case_and_separator_insensitively() {
        assert!(windows_same_path(
            r"C:\Users\a\AppData\Local\Programs\Zeron\zeron.exe",
            r"c:/users/A/appdata/local/programs/zeron/ZERON.EXE"
        ));
        assert!(windows_same_path(r"\\?\C:\Z\zeron.exe", r"C:\Z\zeron.exe"));
        assert!(!windows_same_path(r"C:\Z\zeron.exe", r"C:\Y\zeron.exe"));
    }

    #[test]
    fn startup_approved_flag_reads_the_first_byte() {
        let enabled = [2u8, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0];
        let disabled = [3u8, 0, 0, 0, 1, 2, 3, 4, 5, 6, 7, 8];
        assert!(!startup_approved_disabled(&enabled));
        assert!(startup_approved_disabled(&disabled));
        assert!(!startup_approved_disabled(&[6]));
        assert!(startup_approved_disabled(&[7]));
        assert!(!startup_approved_disabled(&[]));
    }

    #[test]
    fn desktop_entries_round_trip_the_program() {
        let exe = Path::new("/home/jane/My Apps/zeron $1 50%/zeron");
        let entry = render_desktop_entry(exe, LoginItem::Engine);
        assert!(entry.contains("\nExec=\"/home/jane/My Apps/zeron \\\\$1 50%%/zeron\" headless\n"));
        let (program, disabled) = parse_desktop_entry(&entry);
        assert_eq!(program.as_deref(), exe.to_str());
        assert!(!disabled);

        let app = render_desktop_entry(Path::new("/opt/zeron/zeron"), LoginItem::App);
        assert!(app.contains("Exec=\"/opt/zeron/zeron\" --autostart\n"));
    }

    #[test]
    fn installer_builds_launch_through_the_current_symlink() {
        assert_eq!(
            installer_exe_path(
                Path::new("/home/u/.zeron/app/0.3.0/zeron"),
                Some(Path::new("/home/u"))
            ),
            Some(PathBuf::from("/home/u/.zeron/app/current/zeron"))
        );
        assert_eq!(
            installer_exe_path(
                Path::new("/src/target/debug/zeron"),
                Some(Path::new("/home/u"))
            ),
            None
        );
        assert_eq!(installer_exe_path(Path::new("/x/zeron"), None), None);
    }

    #[test]
    fn desktop_entry_backslashes_round_trip() {
        let exe = Path::new(r"/odd\dir/zeron");
        let entry = render_desktop_entry(exe, LoginItem::App);
        assert_eq!(parse_desktop_entry(&entry).0.as_deref(), exe.to_str());
    }

    #[test]
    fn desktop_entries_report_system_disable() {
        let base = "[Desktop Entry]\nExec=zeron headless\n";
        assert_eq!(
            parse_desktop_entry(base),
            (Some("zeron".to_string()), false)
        );
        assert!(parse_desktop_entry(&format!("{base}Hidden=true\n")).1);
        assert!(parse_desktop_entry(&format!("{base}X-GNOME-Autostart-enabled=false\n")).1);
        // Keys outside the main group are ignored.
        assert!(!parse_desktop_entry(&format!("{base}[Desktop Action x]\nHidden=true\n")).1);
    }

    #[test]
    fn status_helpers() {
        let mut status = AutostartStatus::default();
        assert!(!status.engine_starts_at_login());
        status.engine_service_installed = true;
        assert!(status.engine_starts_at_login());
        status.engine_service_installed = false;
        status.engine = ItemStatus {
            registered: true,
            disabled_by_system: true,
            other_executable: None,
        };
        assert!(!status.item(LoginItem::Engine).enabled());
        assert!(!status.engine_starts_at_login());
        status.engine.disabled_by_system = false;
        assert!(status.engine_starts_at_login());
    }
}
