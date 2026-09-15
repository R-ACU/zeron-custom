//! Shared resolver for the external tools the engine shells out to (`git`, `gh`),
//! plus the Windows stand-in for a 0600 file mode.
//!
//! On unix every login shell has both on `PATH` and the bare program name is
//! enough, so this is the identity function there and the spawn sites keep their
//! exact previous behaviour.
//!
//! On Windows a process inherits the `PATH` of whatever started it. The headed
//! app launched from Explorer, and the daemon started by a Scheduled Task, both
//! routinely run with a `PATH` that never saw the Git for Windows or GitHub CLI
//! installer, so a bare `git` spawn fails with NotFound while both tools are
//! installed and work fine in a terminal. Resolve against `PATH` first (adding
//! the executable extensions, because `CreateProcess` only appends `.exe` for a
//! bare name and not for every `PATHEXT` entry), then against the known install
//! directories. The bare name is returned when nothing matches, so the spawn
//! error the caller reports stays the one it reported before.

use std::ffi::OsString;

#[cfg(not(windows))]
pub(crate) fn resolve_tool(tool: &str) -> OsString {
    OsString::from(tool)
}

#[cfg(windows)]
pub(crate) fn resolve_tool(tool: &str) -> OsString {
    use std::collections::HashMap;
    use std::sync::{Mutex, OnceLock};

    // A resolved tool is stable for the process lifetime; the lookup touches the
    // filesystem, and the git spawn sites run per diff tick.
    static CACHE: OnceLock<Mutex<HashMap<String, OsString>>> = OnceLock::new();
    let cache = CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    if let Ok(cache) = cache.lock() {
        if let Some(found) = cache.get(tool) {
            return found.clone();
        }
    }
    let resolved = resolve_tool_uncached(tool);
    if let Ok(mut cache) = cache.lock() {
        cache.insert(tool.to_string(), resolved.clone());
    }
    resolved
}

#[cfg(windows)]
fn resolve_tool_uncached(tool: &str) -> OsString {
    use std::path::{Path, PathBuf};

    // Anything that already carries a path (tests inject absolute executables)
    // is passed through untouched.
    if Path::new(tool).components().count() > 1 {
        return OsString::from(tool);
    }

    let extensions: Vec<String> = std::env::var("PATHEXT")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| ".EXE;.CMD;.BAT;.COM".to_string())
        .split(';')
        .map(|extension| extension.trim().to_ascii_lowercase())
        .filter(|extension| extension.starts_with('.'))
        .collect();

    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Some(path) = std::env::var_os("PATH") {
        for directory in std::env::split_paths(&path) {
            if directory.as_os_str().is_empty() {
                continue;
            }
            for extension in &extensions {
                candidates.push(directory.join(format!("{tool}{extension}")));
            }
            candidates.push(directory.join(tool));
        }
    }
    candidates.extend(install_locations(tool));

    for candidate in candidates {
        if candidate.is_file() {
            return candidate.into_os_string();
        }
    }
    OsString::from(tool)
}

/// Default install directories of the tools the engine needs, in the order the
/// installers prefer them: the machine-wide Git for Windows layout, the 32-bit
/// view for a 64-bit process, then the per-user install.
#[cfg(windows)]
fn install_locations(tool: &str) -> Vec<std::path::PathBuf> {
    use std::path::PathBuf;

    fn env_join(variable: &str, tail: &[&str]) -> Option<PathBuf> {
        let base = std::env::var_os(variable).filter(|value| !value.is_empty())?;
        let mut path = PathBuf::from(base);
        path.extend(tail);
        Some(path)
    }

    let mut locations = Vec::new();
    match tool {
        "git" => {
            for program_files in ["ProgramFiles", "ProgramFiles(x86)", "ProgramW6432"] {
                locations.extend(env_join(program_files, &["Git", "cmd", "git.exe"]));
                locations.extend(env_join(program_files, &["Git", "bin", "git.exe"]));
            }
            locations.extend(env_join(
                "LOCALAPPDATA",
                &["Programs", "Git", "cmd", "git.exe"],
            ));
            locations.extend(env_join(
                "LOCALAPPDATA",
                &["Programs", "Git", "bin", "git.exe"],
            ));
        }
        "gh" => {
            for program_files in ["ProgramFiles", "ProgramFiles(x86)", "ProgramW6432"] {
                locations.extend(env_join(program_files, &["GitHub CLI", "gh.exe"]));
            }
            locations.extend(env_join(
                "LOCALAPPDATA",
                &["Programs", "GitHub CLI", "gh.exe"],
            ));
        }
        _ => {}
    }
    locations
}

/// Windows stand-in for creating a secret file with mode 0600.
///
/// There is no mode bit to set. The real protection is that `%USERPROFILE%` and
/// everything the engine creates below it already inherit a per-user ACL, so a
/// secret written there is not readable by other interactive users to begin
/// with. What inheritance does NOT remove is a wider ACE the profile picked up
/// from somewhere else (a shared or relocated profile, a folder someone granted
/// a group access to), and that is the gap this closes: drop inherited ACEs and
/// leave exactly one ACE for the current user, which is as close to 0600 as a
/// DACL gets.
///
/// Best effort in every direction. `icacls` applies the grant and the
/// inheritance change as one transaction, so a run that cannot resolve the
/// principal leaves the previous ACL untouched rather than locking the engine
/// out of its own secret. The call is fire-and-forget (no wait, no console
/// window) because both callers are synchronous functions reachable from async
/// tasks, and the outcome is advisory.
#[cfg(windows)]
pub(crate) fn restrict_to_current_user(path: &std::path::Path) {
    use std::os::windows::process::CommandExt;

    const CREATE_NO_WINDOW: u32 = 0x0800_0000;

    let Some(user) = std::env::var_os("USERNAME").filter(|value| !value.is_empty()) else {
        return;
    };
    let mut principal = std::ffi::OsString::new();
    if let Some(domain) = std::env::var_os("USERDOMAIN").filter(|value| !value.is_empty()) {
        principal.push(domain);
        principal.push("\\");
    }
    principal.push(user);
    principal.push(":F");

    let spawned = std::process::Command::new("icacls")
        .arg(path)
        .arg("/grant:r")
        .arg(&principal)
        .arg("/inheritance:r")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .creation_flags(CREATE_NO_WINDOW)
        .spawn();
    if let Err(error) = spawned {
        tracing::debug!(%error, path = %path.display(), "icacls hardening skipped");
    }
}

#[cfg(test)]
mod tests {
    use super::resolve_tool;

    #[test]
    fn a_tool_on_path_resolves_to_something_spawnable() {
        // git is a hard requirement of the engine on every platform, so this is
        // an honest end-to-end check of the resolver rather than a mock.
        let git = resolve_tool("git");
        let output = std::process::Command::new(&git)
            .arg("--version")
            .output()
            .expect("resolved git spawns");
        assert!(output.status.success(), "git --version failed");
    }

    #[test]
    fn an_unknown_tool_falls_back_to_the_bare_name() {
        assert_eq!(
            resolve_tool("zeron-not-a-real-tool"),
            std::ffi::OsString::from("zeron-not-a-real-tool")
        );
    }

    #[cfg(windows)]
    #[test]
    fn an_absolute_program_is_passed_through() {
        let absolute = r"C:\Windows\System32\cmd.exe";
        assert_eq!(resolve_tool(absolute), std::ffi::OsString::from(absolute));
    }
}
