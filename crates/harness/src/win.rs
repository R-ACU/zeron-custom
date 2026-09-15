//! Windows process support: executable resolution, shim unwrapping and the
//! signal equivalents the unix paths get from `kill(2)`.
//!
//! Three problems this module solves, all invisible on unix:
//!
//! 1. A bare name is not a file. `claude` on PATH is `claude.cmd` (npm),
//!    `claude.exe` (native installer) or an extensionless bash shim that
//!    Windows cannot execute at all. [`candidate_names`] expands a bare name
//!    over PATHEXT, always trying `.exe`, `.cmd` and `.bat`, and leaves the
//!    bare name last so a Git-Bash-only shim is the last resort, never the
//!    first hit.
//! 2. Spawning a `.cmd` goes through `cmd.exe /c` (std since 1.77), which
//!    re-parses every argument and refuses the ones it cannot escape. It also
//!    puts a shell between us and the agent: the pid we hold is cmd.exe's, so
//!    a kill leaves the real agent alive with our pipes open. npm shims are a
//!    two-line file pointing at the program they wrap, so [`launch_spec`]
//!    reads that target out and spawns it directly (an `.exe` as itself, a
//!    `.js` entry through `node`, preferring a vendored platform binary when
//!    the package ships one). Only a shim we cannot parse keeps the cmd.exe
//!    path.
//! 3. There is no SIGTERM. Agent children are created with
//!    CREATE_NEW_PROCESS_GROUP, which makes the child its own group leader
//!    and lets [`send_signal`] deliver a console CTRL_BREAK to exactly that
//!    group as the graceful stop, escalating to TerminateProcess for the
//!    kill.

use std::path::{Path, PathBuf};

use windows_sys::Win32::Foundation::CloseHandle;
use windows_sys::Win32::System::Console::{
    AttachConsole, CTRL_BREAK_EVENT, FreeConsole, GenerateConsoleCtrlEvent, GetConsoleWindow,
};
use windows_sys::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW, TH32CS_SNAPPROCESS,
};
use windows_sys::Win32::System::Threading::{
    CREATE_NEW_PROCESS_GROUP, CREATE_NO_WINDOW, OpenProcess, PROCESS_TERMINATE, TerminateProcess,
};

/// Extensions CreateProcess can launch on its own. `.ps1`, `.vbs` and the
/// other PATHEXT entries need an interpreter, so they never become candidates.
const SPAWNABLE: [&str; 4] = [".com", ".exe", ".bat", ".cmd"];
/// Appended to whatever PATHEXT says, so a machine with a trimmed PATHEXT
/// still finds npm's `.cmd` shims.
const ALWAYS: [&str; 3] = [".exe", ".cmd", ".bat"];

/// The rust target triple npm platform packages vendor their binaries under.
#[cfg(target_arch = "x86_64")]
const VENDOR_TRIPLE: &str = "x86_64-pc-windows-msvc";
#[cfg(target_arch = "aarch64")]
const VENDOR_TRIPLE: &str = "aarch64-pc-windows-msvc";

/// File names to try for a bare executable name, in search order: PATHEXT's
/// own order first (minus what we cannot spawn), then the bare name — which
/// on Windows is usually a shell script no CreateProcess call can run.
pub(crate) fn candidate_names(exe: &str) -> Vec<String> {
    let ext = Path::new(exe)
        .extension()
        .map(|e| format!(".{}", e.to_string_lossy().to_lowercase()));
    // An explicit `.exe`/`.cmd` is already a file name; take it as given.
    if ext.as_deref().is_some_and(|e| SPAWNABLE.contains(&e)) {
        return vec![exe.to_owned()];
    }
    let mut names: Vec<String> = Vec::new();
    for ext in pathext() {
        names.push(format!("{exe}{ext}"));
    }
    names.push(exe.to_owned());
    names
}

/// PATHEXT in its own order, lowercased, restricted to spawnable extensions,
/// with `.exe`/`.cmd`/`.bat` guaranteed present.
fn pathext() -> Vec<String> {
    let mut exts: Vec<String> = std::env::var_os("PATHEXT")
        .map(|value| {
            value
                .to_string_lossy()
                .split(';')
                .map(|e| e.trim().to_lowercase())
                .filter(|e| SPAWNABLE.contains(&e.as_str()))
                .collect()
        })
        .unwrap_or_default();
    for ext in ALWAYS {
        if !exts.iter().any(|e| e == ext) {
            exts.push(ext.to_owned());
        }
    }
    exts
}

/// The tool a binary name belongs to, for per-tool install dirs: the adapter
/// suffixes zeron spawns (`cursor-agent`, `pi-acp`) live under the tool's own
/// dot-directory, not one named after the adapter.
fn tool_name(exe: &str) -> &str {
    let stem = exe.strip_suffix(".exe").unwrap_or(exe);
    stem.strip_suffix("-agent")
        .or_else(|| stem.strip_suffix("-acp"))
        .or_else(|| stem.strip_suffix("-cli"))
        .unwrap_or(stem)
}

/// Where Windows installers actually put agent CLIs, probed after PATH and
/// the logon PATH snapshot. Covers the npm global prefix, the "download a
/// binary into a dot-dir" installers (claude's `~/.local/bin` and
/// `~/.claude/local`, codex, cursor, opencode) and per-user MSI/Programs
/// installs.
pub(crate) fn install_dirs(exe: &str) -> Vec<PathBuf> {
    let tool = tool_name(exe);
    let mut dirs: Vec<PathBuf> = Vec::new();
    if let Some(home) = crate::home_dir() {
        dirs.push(home.join(".local").join("bin"));
        dirs.push(home.join(format!(".{tool}")).join("bin"));
        dirs.push(home.join(format!(".{tool}")).join("local"));
        dirs.push(home.join(".npm-global").join("bin"));
    }
    if let Some(appdata) = env_dir("APPDATA") {
        dirs.push(appdata.join("npm"));
    }
    if let Some(local) = env_dir("LOCALAPPDATA") {
        dirs.push(local.join("Programs").join(tool));
        dirs.push(local.join("Programs").join(tool).join("bin"));
        dirs.push(local.join(tool).join("bin"));
    }
    if let Some(files) = env_dir("ProgramFiles") {
        dirs.push(files.join(tool));
        dirs.push(files.join(tool).join("bin"));
    }
    dirs
}

/// Node version managers on Windows: nvm-windows (a junction the manager
/// repoints, plus every installed version), volta, bun, fnm and the npm
/// global prefix itself.
pub(crate) fn node_version_manager_bins() -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = Vec::new();
    if let Some(appdata) = env_dir("APPDATA") {
        dirs.push(appdata.join("npm"));
        dirs.push(appdata.join("fnm").join("aliases").join("default"));
        dirs.push(
            appdata
                .join("fnm")
                .join("aliases")
                .join("default")
                .join("bin"),
        );
    }
    if let Some(local) = env_dir("LOCALAPPDATA") {
        dirs.push(local.join("Volta").join("bin"));
        dirs.push(local.join("fnm_multishells"));
    }
    if let Some(home) = crate::home_dir() {
        dirs.push(home.join(".bun").join("bin"));
        dirs.push(home.join(".volta").join("bin"));
    }
    // nvm-windows: NVM_SYMLINK is the active version's junction; the
    // per-version dirs below NVM_HOME cover a symlink that is not set up.
    if let Some(link) = env_dir("NVM_SYMLINK") {
        dirs.push(link);
    }
    let nvm_roots = env_dir("NVM_HOME")
        .into_iter()
        .chain(env_dir("APPDATA").map(|d| d.join("nvm")));
    for root in nvm_roots {
        if let Ok(entries) = std::fs::read_dir(&root) {
            let mut versions: Vec<PathBuf> = entries
                .flatten()
                .map(|e| e.path())
                .filter(|p| {
                    p.file_name()
                        .is_some_and(|n| n.to_string_lossy().starts_with('v'))
                })
                .collect();
            versions.sort();
            versions.reverse();
            dirs.append(&mut versions);
        }
    }
    dirs
}

fn env_dir(var: &str) -> Option<PathBuf> {
    std::env::var_os(var)
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
}

/// What to actually spawn for a resolved program, as (program, leading args).
///
/// `.cmd`/`.bat` npm shims unwrap to the program they wrap; a JS entry runs
/// through `node` unless its package vendors a native binary for this target;
/// a shebang script runs through its interpreter (npm also installs
/// extensionless `#!/bin/sh` shims, and zeron's own test fixtures are such
/// scripts). Everything we cannot decode is returned unchanged, which for a
/// `.cmd` means std's cmd.exe path still applies.
pub(crate) fn launch_spec(exe: &Path) -> (PathBuf, Vec<String>) {
    let unchanged = || (exe.to_path_buf(), Vec::new());
    let ext = exe
        .extension()
        .map(|e| e.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    match ext.as_str() {
        "exe" | "com" => unchanged(),
        "cmd" | "bat" => {
            let Ok(text) = std::fs::read_to_string(exe) else {
                return unchanged();
            };
            let dir = exe.parent().unwrap_or(Path::new("."));
            match parse_cmd_shim(&text, dir) {
                Some(target) => program_for(&target).unwrap_or_else(unchanged),
                None => unchanged(),
            }
        }
        _ => program_for(exe).unwrap_or_else(unchanged),
    }
}

/// How to run a concrete file: a PE binary directly, a JS entry through node
/// (or its vendored native binary), a shebang script through its interpreter.
fn program_for(file: &Path) -> Option<(PathBuf, Vec<String>)> {
    let ext = file
        .extension()
        .map(|e| e.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    if matches!(ext.as_str(), "exe" | "com") {
        return Some((file.to_path_buf(), Vec::new()));
    }
    let head = read_head(file);
    if head.starts_with(b"MZ") {
        return Some((file.to_path_buf(), Vec::new()));
    }
    let interpreter = if matches!(ext.as_str(), "js" | "mjs" | "cjs") {
        Some("node")
    } else {
        shebang_interpreter(&head)
    }?;
    if interpreter == "node"
        && let Some(native) = vendored_native_binary(file)
    {
        return Some((native, Vec::new()));
    }
    let program = crate::find_executable(interpreter, interpreter_dirs(interpreter))?;
    Some((program, vec![file.display().to_string()]))
}

fn read_head(file: &Path) -> Vec<u8> {
    use std::io::Read;
    let mut head = vec![0u8; 512];
    let Ok(mut f) = std::fs::File::open(file) else {
        return Vec::new();
    };
    match f.read(&mut head) {
        Ok(n) => {
            head.truncate(n);
            head
        }
        Err(_) => Vec::new(),
    }
}

/// The interpreter a `#!` line asks for, by base name (`/usr/bin/env node` →
/// `node`). `sh`/`bash`/`dash` all answer to `sh`, which on Windows means Git
/// for Windows' `sh.exe`.
pub(crate) fn shebang_interpreter(head: &[u8]) -> Option<&'static str> {
    let line = head.strip_prefix(b"#!")?;
    let end = line
        .iter()
        .position(|b| *b == b'\n' || *b == b'\r')
        .unwrap_or(line.len());
    let line = String::from_utf8_lossy(&line[..end]).to_string();
    for word in line.split_whitespace() {
        let name = word.rsplit(['/', '\\']).next().unwrap_or(word);
        match name {
            "env" => continue,
            "node" | "nodejs" => return Some("node"),
            "sh" | "bash" | "dash" | "zsh" => return Some("sh"),
            "python" | "python3" => return Some("python"),
            _ => return None,
        }
    }
    None
}

/// Extra places to look for an interpreter: Git for Windows ships the only
/// `sh.exe` most machines have, and it is not on PATH unless the user put it
/// there.
fn interpreter_dirs(interpreter: &str) -> Vec<PathBuf> {
    if interpreter != "sh" {
        return Vec::new();
    }
    let mut dirs = Vec::new();
    for root in [
        env_dir("ProgramFiles").map(|d| d.join("Git")),
        env_dir("ProgramFiles(x86)").map(|d| d.join("Git")),
        env_dir("LOCALAPPDATA").map(|d| d.join("Programs").join("Git")),
        env_dir("GIT_INSTALL_ROOT"),
    ]
    .into_iter()
    .flatten()
    {
        dirs.push(root.join("usr").join("bin"));
        dirs.push(root.join("bin"));
    }
    dirs
}

/// The target of an npm `.cmd` shim, resolved against the shim's directory.
///
/// npm writes the wrapped program as a `%dp0%`/`%~dp0`-relative literal on
/// the line that runs it (`"%dp0%\node_modules\@scope\pkg\bin\cli.js" %*`),
/// so the last such line wins and the `SET`/`IF`/`FOR` bookkeeping above it
/// is skipped. A shim that only invokes variables (npm's own `npm.cmd`)
/// yields `None` and keeps the cmd.exe path.
pub(crate) fn parse_cmd_shim(text: &str, shim_dir: &Path) -> Option<PathBuf> {
    for line in text.lines().rev() {
        let trimmed = line.trim();
        let head = trimmed
            .split_whitespace()
            .next()
            .unwrap_or_default()
            .to_ascii_uppercase();
        if matches!(head.as_str(), "SET" | "IF" | "FOR" | "REM" | "::") {
            continue;
        }
        if let Some(target) = shim_target_in(trimmed, shim_dir) {
            return Some(target);
        }
    }
    None
}

/// The first `%dp0%`-relative program path inside a quoted token on one line.
fn shim_target_in(line: &str, shim_dir: &Path) -> Option<PathBuf> {
    for token in line.split('"') {
        let Some(rest) = token
            .strip_prefix("%dp0%")
            .or_else(|| token.strip_prefix("%~dp0"))
        else {
            continue;
        };
        let rest = rest.trim_start_matches(['\\', '/']);
        let ext = Path::new(rest)
            .extension()
            .map(|e| e.to_string_lossy().to_lowercase())
            .unwrap_or_default();
        if !matches!(ext.as_str(), "exe" | "js" | "mjs" | "cjs") {
            continue;
        }
        let target = shim_dir.join(rest.replace('/', "\\"));
        if target.exists() {
            return Some(target);
        }
    }
    None
}

/// The native binary an npm package vendors for this target, given its JS
/// entry (`@openai/codex` ships `codex.exe` under a `codex-win32-x64`
/// platform package and only shells out to it). Preferring the binary keeps
/// node out of the middle, where it would swallow our console events and
/// leave the real agent alive when the pid we hold is killed.
fn vendored_native_binary(entry: &Path) -> Option<PathBuf> {
    let stem = entry.file_stem()?.to_string_lossy().to_string();
    // <pkg>/bin/<stem>.js — the package root is the parent of `bin`.
    let pkg = entry.parent()?.parent()?;
    let relative = Path::new("vendor")
        .join(VENDOR_TRIPLE)
        .join("bin")
        .join(format!("{stem}.exe"));
    let direct = pkg.join(&relative);
    if direct.exists() {
        return Some(direct);
    }
    // Platform packages land in the package's own node_modules, scoped or not.
    let node_modules = pkg.join("node_modules");
    for dir in read_dirs(&node_modules) {
        let candidate = dir.join(&relative);
        if candidate.exists() {
            return Some(candidate);
        }
        if dir
            .file_name()
            .is_some_and(|n| n.to_string_lossy().starts_with('@'))
        {
            for scoped in read_dirs(&dir) {
                let candidate = scoped.join(&relative);
                if candidate.exists() {
                    return Some(candidate);
                }
            }
        }
    }
    None
}

fn read_dirs(dir: &Path) -> Vec<PathBuf> {
    std::fs::read_dir(dir)
        .map(|entries| entries.flatten().map(|e| e.path()).collect())
        .unwrap_or_default()
}

/// Creation flags for every agent child: its own process group (so
/// [`send_signal`] can address it and only it) and, when we have no console
/// to hand down, no console window flashing up on spawn.
pub(crate) fn creation_flags() -> u32 {
    let mut flags = CREATE_NEW_PROCESS_GROUP;
    if !has_console() {
        flags |= CREATE_NO_WINDOW;
    }
    flags
}

fn has_console() -> bool {
    // SAFETY: a parameterless query with no preconditions.
    !unsafe { GetConsoleWindow() }.is_null()
}

/// The unix `kill` equivalent: CTRL_BREAK as the graceful stop,
/// TerminateProcess as the kill.
pub(crate) fn send_signal(pid: u32, signal: crate::Signal) {
    match signal {
        crate::Signal::Term => {
            if !ctrl_break(pid) {
                tracing::debug!(
                    target: "zeron_harness::win",
                    pid,
                    "CTRL_BREAK could not be delivered; the kill escalation will end the child"
                );
            }
        }
        crate::Signal::Kill => terminate(pid),
    }
}

/// Deliver CTRL_BREAK to the child's process group. When the daemon has a
/// console of its own the child inherited it, so the event can be sent
/// straight out. A GUI/service launch has none, and the child (spawned with
/// CREATE_NO_WINDOW) owns a hidden one: attach to it for the length of the
/// call, which is safe precisely because we had no console to lose.
fn ctrl_break(pid: u32) -> bool {
    static CONSOLE: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let _guard = CONSOLE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    // SAFETY: console APIs on our own process and a pid we spawned; the
    // attach/detach pair is serialized by the mutex above.
    unsafe {
        if has_console() {
            return GenerateConsoleCtrlEvent(CTRL_BREAK_EVENT, pid) != 0;
        }
        if AttachConsole(pid) == 0 {
            return false;
        }
        let delivered = GenerateConsoleCtrlEvent(CTRL_BREAK_EVENT, pid) != 0;
        FreeConsole();
        delivered
    }
}

/// Kill the whole tree below `pid`, children first. TerminateProcess is not
/// recursive, and an agent's helper processes (node under a `.cmd` shim, a
/// shell script's `sleep`) inherit its stdout: while any of them lives the
/// pipe stays open and the run stream never ends.
fn terminate(pid: u32) {
    for child in child_pids(pid) {
        terminate(child);
    }
    // SAFETY: a pid we spawned and have not reaped; the handle is closed on
    // every path.
    unsafe {
        let handle = OpenProcess(PROCESS_TERMINATE, 0, pid);
        if handle.is_null() {
            return;
        }
        TerminateProcess(handle, 1);
        CloseHandle(handle);
    }
}

/// Direct children of `pid` from a process snapshot.
fn child_pids(pid: u32) -> Vec<u32> {
    let mut children = Vec::new();
    // SAFETY: Toolhelp snapshot of our own session; the entry struct is
    // zero-initialised with its size set as the API requires, and the handle
    // is closed on every path.
    unsafe {
        let snapshot = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
        if snapshot.is_null() || snapshot == windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE {
            return children;
        }
        let mut entry: PROCESSENTRY32W = std::mem::zeroed();
        entry.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;
        if Process32FirstW(snapshot, &mut entry) != 0 {
            loop {
                if entry.th32ParentProcessID == pid && entry.th32ProcessID != pid {
                    children.push(entry.th32ProcessID);
                }
                if Process32NextW(snapshot, &mut entry) == 0 {
                    break;
                }
            }
        }
        CloseHandle(snapshot);
    }
    children
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The real `%APPDATA%\npm\claude.cmd` npm writes for claude-code
    /// (CLI 2.1.x): the wrapped program is a native binary in node_modules.
    const CLAUDE_CMD: &str = "@ECHO off\r\nGOTO start\r\n:find_dp0\r\nSET dp0=%~dp0\r\nEXIT /b\r\n:start\r\nSETLOCAL\r\nCALL :find_dp0\r\n\"%dp0%\\node_modules\\@anthropic-ai\\claude-code\\bin\\claude.exe\"   %*\r\n";

    /// The real `%APPDATA%\npm\codex.cmd`: node plus the package's JS entry.
    const CODEX_CMD: &str = "@ECHO off\r\nGOTO start\r\n:find_dp0\r\nSET dp0=%~dp0\r\nEXIT /b\r\n:start\r\nSETLOCAL\r\nCALL :find_dp0\r\n\r\nIF EXIST \"%dp0%\\node.exe\" (\r\n  SET \"_prog=%dp0%\\node.exe\"\r\n) ELSE (\r\n  SET \"_prog=node\"\r\n  SET PATHEXT=%PATHEXT:;.JS;=;%\r\n)\r\n\r\nendLocal & goto #_undefined_# 2>NUL || title %COMSPEC% & \"%_prog%\"  \"%dp0%\\node_modules\\@openai\\codex\\bin\\codex.js\" %*\r\n";

    /// npm's own `npm.cmd`: every path is behind a variable, so there is
    /// nothing to unwrap and the cmd.exe path has to stand.
    const NPM_CMD: &str = ":: Created by npm, please don't edit manually.\r\n@ECHO OFF\r\nSETLOCAL\r\nSET \"NODE_EXE=%~dp0\\node.exe\"\r\nSET \"NPM_CLI_JS=%~dp0\\node_modules\\npm\\bin\\npm-cli.js\"\r\n\"%NODE_EXE%\" \"%NPM_CLI_JS%\" %*\r\n";

    fn touch(path: &Path) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, "x").unwrap();
    }

    #[test]
    fn candidate_names_try_pathext_then_the_bare_name() {
        let names = candidate_names("claude");
        assert_eq!(names.last().unwrap(), "claude", "bare name is the last try");
        assert!(names.contains(&"claude.exe".to_owned()));
        assert!(names.contains(&"claude.cmd".to_owned()));
        assert!(names.contains(&"claude.bat".to_owned()));
        assert!(
            names.iter().position(|n| n == "claude.exe") < names.iter().position(|n| n == "claude"),
            "an executable must win over the bash shim: {names:?}"
        );
        // A name that already carries a spawnable extension is taken as is.
        assert_eq!(candidate_names("node.exe"), vec!["node.exe".to_owned()]);
    }

    #[test]
    fn cmd_shim_resolves_a_native_target() {
        let dir = tempfile::tempdir().unwrap();
        let exe = dir
            .path()
            .join("node_modules")
            .join("@anthropic-ai")
            .join("claude-code")
            .join("bin")
            .join("claude.exe");
        touch(&exe);
        assert_eq!(parse_cmd_shim(CLAUDE_CMD, dir.path()), Some(exe));
    }

    #[test]
    fn cmd_shim_resolves_a_js_entry_past_its_set_lines() {
        let dir = tempfile::tempdir().unwrap();
        let js = dir
            .path()
            .join("node_modules")
            .join("@openai")
            .join("codex")
            .join("bin")
            .join("codex.js");
        touch(&js);
        // `SET "_prog=%dp0%\node.exe"` must not be mistaken for the target.
        touch(&dir.path().join("node.exe"));
        assert_eq!(parse_cmd_shim(CODEX_CMD, dir.path()), Some(js));
    }

    #[test]
    fn variable_only_shim_is_left_to_cmd_exe() {
        let dir = tempfile::tempdir().unwrap();
        touch(&dir.path().join("node.exe"));
        touch(
            &dir.path()
                .join("node_modules")
                .join("npm")
                .join("bin")
                .join("npm-cli.js"),
        );
        assert_eq!(parse_cmd_shim(NPM_CMD, dir.path()), None);
    }

    #[test]
    fn vendored_platform_binary_wins_over_the_js_entry() {
        let dir = tempfile::tempdir().unwrap();
        let pkg = dir.path().join("node_modules").join("@openai").join("codex");
        let js = pkg.join("bin").join("codex.js");
        touch(&js);
        assert_eq!(vendored_native_binary(&js), None);
        let native = pkg
            .join("node_modules")
            .join("@openai")
            .join("codex-win32-x64")
            .join("vendor")
            .join(VENDOR_TRIPLE)
            .join("bin")
            .join("codex.exe");
        touch(&native);
        assert_eq!(vendored_native_binary(&js), Some(native));
    }

    #[test]
    fn shebang_maps_to_an_interpreter() {
        assert_eq!(shebang_interpreter(b"#!/bin/sh\nread -r x\n"), Some("sh"));
        assert_eq!(shebang_interpreter(b"#!/usr/bin/env bash\n"), Some("sh"));
        assert_eq!(shebang_interpreter(b"#!/usr/bin/env node\n"), Some("node"));
        assert_eq!(shebang_interpreter(b"MZ\x90\x00"), None);
        assert_eq!(shebang_interpreter(b"#!/usr/bin/perl\n"), None);
    }

    #[test]
    fn a_js_file_launches_through_node() {
        let dir = tempfile::tempdir().unwrap();
        let js = dir.path().join("cli.js");
        std::fs::write(&js, "console.log(1)\n").unwrap();
        let (program, args) = launch_spec(&js);
        assert_eq!(
            program.file_stem().unwrap().to_string_lossy(),
            "node",
            "resolved program: {}",
            program.display()
        );
        assert_eq!(args, vec![js.display().to_string()]);
    }

    #[test]
    fn an_exe_is_spawned_unchanged() {
        let exe = PathBuf::from("C:\\Windows\\System32\\cmd.exe");
        assert_eq!(launch_spec(&exe), (exe, Vec::new()));
    }
}
