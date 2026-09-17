//! `zeron daemon …` — install/manage `zeron headless` as a background service:
//! a systemd **user** unit on Linux (the VPS deployment target), a launchd
//! LaunchAgent on macOS. The unit runs the current executable with the
//! `ZERON_*` environment captured at install time, so
//! `ZERON_EDGE_URL=… zeron daemon install` bakes that override in.
//!
//! Auth is decoupled: without a saved session the service remains up on the
//! local-only profile. `zeron login` and a service restart opt into sync.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, bail};
use zeron_ui::autostart::LoginItem;

const LAUNCHD_LABEL: &str = "sh.zeron.app";
/// Same unit name the curl|sh installer (`edge/src/install.sh`) writes, so
/// `zeron daemon …` manages that installation rather than a competing copy.
const SYSTEMD_UNIT: &str = "zeron.service";
/// Per-user Scheduled Task name on Windows (`schtasks /TN`).
const SCHEDULED_TASK_NAME: &str = "Zeron";

/// Environment captured into the unit file. `PATH` is always included (the
/// engine spawns harness CLIs like `claude`, which service managers' minimal
/// default PATH won't find); the `ZERON_*`/logging vars only when set.
const CAPTURED_ENV: &[&str] = &[
    "PATH",
    "ZERON_DATA_DIR",
    "ZERON_EDGE_URL",
    "ZERON_EDGE_TOKEN",
    "ZERON_ORG_ID",
    "ZERON_WORKOS_CLIENT_ID",
    "ZERON_WORKOS_API_BASE",
    "ZERON_IPC_PORT",
    "ZERON_CALLBACK_PORT",
    "ZERON_HARNESS",
    "ZERON_DEVICE_NAME",
    "RUST_LOG",
];

pub fn install(data_dir: &Path) -> anyhow::Result<()> {
    let exe = std::env::current_exe().context("resolving the zeron executable path")?;
    let env = captured_env();
    if cfg!(target_os = "macos") {
        let plist = launchd_plist_path()?;
        std::fs::create_dir_all(plist.parent().expect("LaunchAgents parent"))?;
        std::fs::create_dir_all(data_dir)?;
        // Reinstall-friendly: unload any previous incarnation before rewriting.
        let _ = run_quiet("launchctl", &["bootout", &launchd_service_target()?]);
        std::fs::write(
            &plist,
            render_launchd_plist(&exe, &env, &data_dir.join("daemon.log")),
        )?;
        run(
            "launchctl",
            &["bootstrap", &launchd_domain()?, &plist.to_string_lossy()],
        )?;
        println!(
            "Installed and started {LAUNCHD_LABEL} ({}).",
            plist.display()
        );
    } else if cfg!(target_os = "linux") {
        let unit = systemd_unit_path()?;
        std::fs::create_dir_all(unit.parent().expect("systemd user dir"))?;
        std::fs::write(&unit, render_systemd_unit(&exe, &env))?;
        run("systemctl", &["--user", "daemon-reload"])?;
        run("systemctl", &["--user", "enable", "--now", SYSTEMD_UNIT])?;
        println!("Installed and started {SYSTEMD_UNIT} ({}).", unit.display());
        println!(
            "For start-at-boot without an active login session (VPS): loginctl enable-linger $USER"
        );
    } else if cfg!(target_os = "windows") {
        std::fs::create_dir_all(data_dir)?;
        std::fs::write(env_file_path(data_dir), render_env_file(&env))?;
        // `conhost.exe --headless` runs the console app without a visible
        // window (Windows 10 1809+ / 11); a Scheduled Task's own action has
        // no "no window" flag of its own. Quote the exe path: schtasks parses
        // `/TR` itself and needs the quotes preserved for a path with spaces.
        let action = format!("conhost.exe --headless \"{}\" headless", exe.display());
        // Reinstall-friendly: end any previous run before rewriting the task.
        let _ = run_quiet("schtasks", &["/End", "/TN", SCHEDULED_TASK_NAME]);
        let created = run(
            "schtasks",
            &[
                "/Create",
                "/F",
                "/SC",
                "ONLOGON",
                "/RL",
                "LIMITED",
                "/TN",
                SCHEDULED_TASK_NAME,
                "/TR",
                &action,
            ],
        );
        match created {
            Ok(()) => {
                // The task owns the engine start now; a login entry left by
                // an earlier fallback install would only start a duplicate
                // that exits on the engine's single-instance lock.
                let _ = zeron_ui::autostart::set_enabled(LoginItem::Engine, false);
                run("schtasks", &["/Run", "/TN", SCHEDULED_TASK_NAME])?;
                println!("Installed and started the '{SCHEDULED_TASK_NAME}' scheduled task.");
            }
            Err(err) => {
                // A generic ONLOGON trigger is refused without elevation
                // (exit code only; the message is localized). The per-user
                // Run entry Settings > General manages needs no rights.
                println!("{err:#}");
                println!(
                    "Scheduled task not created; registering a per-user login entry instead (HKCU Run \"{}\").",
                    LoginItem::Engine.registry_value_name()
                );
                zeron_ui::autostart::set_enabled(LoginItem::Engine, true)
                    .map_err(anyhow::Error::msg)?;
                spawn_hidden_headless(&exe)?;
                println!(
                    "Registered the engine to start at sign-in and started it now. If a Zeron window is already running its own engine, the background engine takes over at the next sign-in."
                );
            }
        }
    } else {
        bail!("zeron daemon is only supported on macOS (launchd), Linux (systemd), and Windows (Scheduled Tasks)");
    }
    println!(
        "Without a saved account the engine stays local-only; sign-in and restart are optional for sync."
    );
    println!(
        "Logs: {}",
        if cfg!(target_os = "macos") {
            format!("{}", data_dir.join("daemon.log").display())
        } else if cfg!(target_os = "windows") {
            // No service manager captures stdout/stderr for a Scheduled Task
            // (unlike launchd's StandardOutPath or systemd's journald), so
            // this points at the internal rotating log `main::open_log_file`
            // already writes for every long-running mode regardless of
            // console attachment; no separate `daemon.log` redirection needed.
            format!(
                "{}",
                data_dir.join("logs").join("zeron-headless.log").display()
            )
        } else {
            format!("journalctl --user -u {SYSTEMD_UNIT}")
        }
    );
    Ok(())
}

/// `<data_dir>\env`: a plain `KEY=VALUE` per line file, the Scheduled Task
/// equivalent of systemd's `EnvironmentFile=`. Windows only (macOS/Linux use
/// their service manager's native environment mechanism), but kept
/// unconditional here since it is pure `std::fs`/`String` handling. `main`'s
/// `apply_env_file_from_data_dir` reads it back before `zeron headless`
/// builds its engine config.
pub(crate) fn env_file_path(data_dir: &Path) -> PathBuf {
    data_dir.join("env")
}

fn render_env_file(env: &[(String, String)]) -> String {
    let mut out = String::new();
    for (key, value) in env {
        out.push_str(key);
        out.push('=');
        out.push_str(value);
        out.push('\n');
    }
    out
}

/// Parses the `env_file_path` format: `KEY=VALUE` per line, blank lines and
/// `#`-prefixed comment lines skipped, everything after the first `=` kept
/// verbatim as the value (no quoting/escaping needed: values are written by
/// `render_env_file` from `std::env::var`, which never contains a newline).
pub(crate) fn parse_env_file(content: &str) -> Vec<(String, String)> {
    content
        .lines()
        .filter_map(|line| {
            let line = line.trim_end_matches('\r');
            let trimmed = line.trim();
            if trimmed.is_empty() || trimmed.starts_with('#') {
                return None;
            }
            let (key, value) = line.split_once('=')?;
            let key = key.trim();
            if key.is_empty() {
                return None;
            }
            Some((key.to_string(), value.to_string()))
        })
        .collect()
}

pub fn uninstall() -> anyhow::Result<()> {
    if cfg!(target_os = "macos") {
        let _ = run_quiet("launchctl", &["bootout", &launchd_service_target()?]);
        let plist = launchd_plist_path()?;
        match std::fs::remove_file(&plist) {
            Ok(()) => println!("Removed {}.", plist.display()),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                println!("Not installed.")
            }
            Err(err) => return Err(err.into()),
        }
    } else if cfg!(target_os = "linux") {
        let _ = run_quiet("systemctl", &["--user", "disable", "--now", SYSTEMD_UNIT]);
        let unit = systemd_unit_path()?;
        match std::fs::remove_file(&unit) {
            Ok(()) => {
                run("systemctl", &["--user", "daemon-reload"])?;
                println!("Removed {}.", unit.display());
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                println!("Not installed.")
            }
            Err(err) => return Err(err.into()),
        }
    } else if cfg!(target_os = "windows") {
        let _ = run_quiet("schtasks", &["/End", "/TN", SCHEDULED_TASK_NAME]);
        let task_removed =
            run_quiet("schtasks", &["/Delete", "/F", "/TN", SCHEDULED_TASK_NAME]).is_ok();
        if task_removed {
            println!("Removed the '{SCHEDULED_TASK_NAME}' scheduled task.");
        }
        // The fallback login entry `install` registers without elevation.
        let entry_removed = windows_login_entry_registered()
            && zeron_ui::autostart::set_enabled(LoginItem::Engine, false).is_ok();
        if entry_removed {
            println!(
                "Removed the '{}' login entry.",
                LoginItem::Engine.registry_value_name()
            );
        }
        if !task_removed && !entry_removed {
            println!("Not installed.");
        }
        if let Ok(data_dir) = windows_data_dir() {
            // Best-effort: absent is fine, and a locked/in-use file isn't worth failing over.
            let _ = std::fs::remove_file(env_file_path(&data_dir));
        }
    } else {
        bail!("zeron daemon is only supported on macOS (launchd), Linux (systemd), and Windows (Scheduled Tasks)");
    }
    Ok(())
}

pub fn start() -> anyhow::Result<()> {
    if cfg!(target_os = "macos") {
        let plist = launchd_plist_path()?;
        if !plist.exists() {
            bail!("not installed — run `zeron daemon install` first");
        }
        // `stop` boots the job out of the domain, so start = bootstrap; already
        // loaded is fine, then kickstart guarantees a running process either way.
        let _ = run_quiet(
            "launchctl",
            &["bootstrap", &launchd_domain()?, &plist.to_string_lossy()],
        );
        run("launchctl", &["kickstart", &launchd_service_target()?])?;
    } else if cfg!(target_os = "linux") {
        run("systemctl", &["--user", "start", SYSTEMD_UNIT])?;
    } else if cfg!(target_os = "windows") {
        if windows_task_installed() {
            run("schtasks", &["/Run", "/TN", SCHEDULED_TASK_NAME])?;
        } else if windows_login_entry_registered() {
            let exe = std::env::current_exe().context("resolving the zeron executable path")?;
            spawn_hidden_headless(&exe)?;
        } else {
            bail!("not installed, run `zeron daemon install` first");
        }
    } else {
        bail!("zeron daemon is only supported on macOS (launchd), Linux (systemd), and Windows (Scheduled Tasks)");
    }
    println!("Started.");
    Ok(())
}

pub fn stop() -> anyhow::Result<()> {
    if cfg!(target_os = "macos") {
        // bootout (not `kill`): with KeepAlive the job would otherwise respawn.
        run("launchctl", &["bootout", &launchd_service_target()?])?;
    } else if cfg!(target_os = "linux") {
        run("systemctl", &["--user", "stop", SYSTEMD_UNIT])?;
    } else if cfg!(target_os = "windows") {
        run("schtasks", &["/End", "/TN", SCHEDULED_TASK_NAME])?;
    } else {
        bail!("zeron daemon is only supported on macOS (launchd), Linux (systemd), and Windows (Scheduled Tasks)");
    }
    println!("Stopped.");
    Ok(())
}

pub fn restart() -> anyhow::Result<()> {
    if cfg!(target_os = "macos") {
        if run_quiet(
            "launchctl",
            &["kickstart", "-k", &launchd_service_target()?],
        )
        .is_err()
        {
            // Not loaded (e.g. after `stop`) — fall through to a plain start.
            return start();
        }
        println!("Restarted.");
        Ok(())
    } else if cfg!(target_os = "linux") {
        run("systemctl", &["--user", "restart", SYSTEMD_UNIT])?;
        println!("Restarted.");
        Ok(())
    } else if cfg!(target_os = "windows") {
        if !windows_task_installed() {
            bail!("not installed, run `zeron daemon install` first");
        }
        let _ = run_quiet("schtasks", &["/End", "/TN", SCHEDULED_TASK_NAME]);
        run("schtasks", &["/Run", "/TN", SCHEDULED_TASK_NAME])?;
        println!("Restarted.");
        Ok(())
    } else {
        bail!("zeron daemon is only supported on macOS (launchd), Linux (systemd), and Windows (Scheduled Tasks)");
    }
}

pub fn status() -> anyhow::Result<()> {
    if cfg!(target_os = "macos") {
        let output = Command::new("launchctl")
            .args(["print", &launchd_service_target()?])
            .output()
            .context("running launchctl")?;
        if !output.status.success() {
            println!(
                "{LAUNCHD_LABEL}: not loaded{}",
                if launchd_plist_path()?.exists() {
                    " (installed — `zeron daemon start`)"
                } else {
                    " (not installed — `zeron daemon install`)"
                }
            );
            return Ok(());
        }
        // `launchctl print` is pages long; surface just the liveness lines.
        let text = String::from_utf8_lossy(&output.stdout);
        println!("{LAUNCHD_LABEL}: loaded");
        for line in text.lines() {
            let trimmed = line.trim();
            if trimmed.starts_with("state = ")
                || trimmed.starts_with("pid = ")
                || trimmed.starts_with("last exit code = ")
            {
                println!("  {trimmed}");
            }
        }
        Ok(())
    } else if cfg!(target_os = "linux") {
        // Passthrough; `status` exits nonzero for inactive units, which is not an
        // error for us to report — the output already says it.
        let _ = Command::new("systemctl")
            .args(["--user", "--no-pager", "status", SYSTEMD_UNIT])
            .status()
            .context("running systemctl")?;
        Ok(())
    } else if cfg!(target_os = "windows") {
        let output = Command::new("schtasks")
            .args(["/Query", "/TN", SCHEDULED_TASK_NAME, "/FO", "LIST", "/V"])
            .output()
            .context("running schtasks")?;
        println!(
            "Login entry: {}",
            if windows_login_entry_registered() {
                "registered (HKCU Run, starts the engine at sign-in)"
            } else {
                "not registered"
            }
        );
        if !output.status.success() {
            println!("{SCHEDULED_TASK_NAME}: not installed (run `zeron daemon install`)");
        } else {
            // `schtasks` output is localized (German here); field names are
            // matched case-insensitively for display only. A locale where
            // neither field matches falls back to the whole (short) block
            // rather than silently showing nothing.
            let text = String::from_utf8_lossy(&output.stdout);
            println!("{SCHEDULED_TASK_NAME}: installed");
            let status_line = schtasks_field(&text, "status");
            let last_result = schtasks_field(&text, "last result");
            if status_line.is_none() && last_result.is_none() {
                for line in text.lines() {
                    let trimmed = line.trim();
                    if !trimmed.is_empty() {
                        println!("  {trimmed}");
                    }
                }
            } else {
                if let Some(status) = status_line {
                    println!("  Status: {status}");
                }
                if let Some(last_result) = last_result {
                    println!("  Last Result: {last_result}");
                }
            }
        }
        // Engine liveness beyond "the task ran": reuses the same probe
        // `zeron status` (auth_cli.rs) uses for the running engine's IPC port.
        let ipc_port = std::env::var("ZERON_IPC_PORT")
            .ok()
            .and_then(|p| p.parse().ok())
            .unwrap_or(27654u16);
        let addr = std::net::SocketAddr::from(([127, 0, 0, 1], ipc_port));
        let reachable =
            std::net::TcpStream::connect_timeout(&addr, std::time::Duration::from_millis(500))
                .is_ok();
        println!(
            "IPC:      {} 127.0.0.1:{ipc_port}",
            if reachable {
                "listening on"
            } else {
                "not listening on"
            }
        );
        Ok(())
    } else {
        bail!("zeron daemon is only supported on macOS (launchd), Linux (systemd), and Windows (Scheduled Tasks)");
    }
}

/// Extracts a `Field:   value` line from `schtasks /FO LIST /V` output,
/// matching the field name (e.g. "status", "last result") case-insensitively
/// so this never depends on English wording for control flow, only for
/// which lines `status()` chooses to surface.
fn schtasks_field<'a>(text: &'a str, field_lower: &str) -> Option<&'a str> {
    text.lines().find_map(|line| {
        let (name, value) = line.split_once(':')?;
        if name.trim().to_lowercase() == field_lower {
            Some(value.trim())
        } else {
            None
        }
    })
}

/// Whether the Scheduled Task exists, checked by exit status alone (`0` =
/// found), never by matching schtasks's localized text.
fn windows_task_installed() -> bool {
    Command::new("schtasks")
        .args(["/Query", "/TN", SCHEDULED_TASK_NAME])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Whether the per-user "Zeron Engine" Run entry exists (the fallback
/// `install` registers, also managed by Settings > General).
fn windows_login_entry_registered() -> bool {
    zeron_ui::autostart::read_status().is_ok_and(|status| status.engine.registered)
}

/// Start `zeron headless` now, detached, with a hidden console of its own
/// (`CREATE_NO_WINDOW`), so it outlives the terminal this CLI runs in.
#[cfg(windows)]
fn spawn_hidden_headless(exe: &Path) -> anyhow::Result<()> {
    use std::os::windows::process::CommandExt;
    const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    Command::new(exe)
        .arg("headless")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .creation_flags(CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW)
        .spawn()
        .context("starting zeron headless")?;
    Ok(())
}

#[cfg(not(windows))]
fn spawn_hidden_headless(_exe: &Path) -> anyhow::Result<()> {
    bail!("starting a hidden engine is only implemented on Windows")
}

/// `<data_dir>` resolution used by `uninstall` to find the env file to
/// remove. Mirrors `main::dirs_data_dir`'s `ZERON_DATA_DIR` override without
/// its one-shot `.comet-native` migration, which is irrelevant to a delete.
fn windows_data_dir() -> anyhow::Result<PathBuf> {
    Ok(std::env::var_os("ZERON_DATA_DIR")
        .map(PathBuf::from)
        .unwrap_or(home_dir()?.join(".zeron")))
}

// ---------------------------------------------------------------------------
// Unit rendering (pure — unit-tested below)
// ---------------------------------------------------------------------------

fn captured_env() -> Vec<(String, String)> {
    CAPTURED_ENV
        .iter()
        .filter_map(|key| std::env::var(key).ok().map(|v| (key.to_string(), v)))
        .collect()
}

fn render_systemd_unit(exe: &Path, env: &[(String, String)]) -> String {
    let mut unit = String::from(
        "[Unit]\nDescription=Zeron headless engine\nAfter=network-online.target\nStartLimitIntervalSec=60\nStartLimitBurst=5\n\n[Service]\n",
    );
    for (key, value) in env {
        // systemd unquotes the value; escape the characters it treats specially.
        let value = value.replace('\\', "\\\\").replace('"', "\\\"");
        unit.push_str(&format!("Environment=\"{key}={value}\"\n"));
    }
    unit.push_str(&format!(
        "ExecStart={} headless\nRestart=on-failure\nRestartSec=5\nEnvironmentFile=-%h/.zeron/env\n\n[Install]\nWantedBy=default.target\n",
        systemd_exec_path(exe)
    ));
    unit
}

/// The ExecStart binary path. An exe under `~/.zeron/app/` came from the
/// curl|sh installer, whose upgrades relink `app/current` — point the unit at
/// the symlink (as the installer's own unit does) so it never pins one version.
/// (`current_exe` resolves symlinks, so the versioned dir is what we see here.)
fn systemd_exec_path(exe: &Path) -> String {
    exec_path_for(exe, std::env::var_os("HOME").map(PathBuf::from).as_deref())
}

fn exec_path_for(exe: &Path, home: Option<&Path>) -> String {
    let installed = home
        .map(|home| home.join(".zeron/app"))
        .is_some_and(|app_root| exe.starts_with(app_root));
    if installed {
        "%h/.zeron/app/current/zeron".to_string()
    } else {
        format!("{}", exe.display())
    }
}

fn render_launchd_plist(exe: &Path, env: &[(String, String)], log: &Path) -> String {
    let mut env_dict = String::new();
    for (key, value) in env {
        env_dict.push_str(&format!(
            "      <key>{}</key><string>{}</string>\n",
            xml_escape(key),
            xml_escape(value)
        ));
    }
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
  <dict>
    <key>Label</key><string>{label}</string>
    <key>ProgramArguments</key>
    <array>
      <string>{exe}</string>
      <string>headless</string>
    </array>
    <key>EnvironmentVariables</key>
    <dict>
{env_dict}    </dict>
    <key>RunAtLoad</key><true/>
    <key>KeepAlive</key>
    <dict>
      <key>SuccessfulExit</key><false/>
    </dict>
    <key>ThrottleInterval</key><integer>30</integer>
    <key>StandardOutPath</key><string>{log}</string>
    <key>StandardErrorPath</key><string>{log}</string>
  </dict>
</plist>
"#,
        label = LAUNCHD_LABEL,
        exe = xml_escape(&exe.to_string_lossy()),
        env_dict = env_dict,
        log = xml_escape(&log.to_string_lossy()),
    )
}

fn xml_escape(input: &str) -> String {
    input
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

// ---------------------------------------------------------------------------
// Paths + process helpers
// ---------------------------------------------------------------------------

fn home_dir() -> anyhow::Result<PathBuf> {
    #[cfg(windows)]
    {
        if let Some(profile) = std::env::var_os("USERPROFILE") {
            if !profile.is_empty() {
                return Ok(PathBuf::from(profile));
            }
        }
        let drive = std::env::var("HOMEDRIVE").unwrap_or_default();
        let path = std::env::var("HOMEPATH").unwrap_or_default();
        if !drive.is_empty() && !path.is_empty() {
            return Ok(PathBuf::from(format!("{drive}{path}")));
        }
        bail!("USERPROFILE not set and HOMEDRIVE/HOMEPATH not set");
    }
    #[cfg(not(windows))]
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .context("HOME not set")
}

fn launchd_plist_path() -> anyhow::Result<PathBuf> {
    Ok(home_dir()?
        .join("Library/LaunchAgents")
        .join(format!("{LAUNCHD_LABEL}.plist")))
}

fn systemd_unit_path() -> anyhow::Result<PathBuf> {
    let config = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or(home_dir()?.join(".config"));
    Ok(config.join("systemd/user").join(SYSTEMD_UNIT))
}

fn launchd_domain() -> anyhow::Result<String> {
    let output = Command::new("id").arg("-u").output().context("id -u")?;
    let uid = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if uid.is_empty() {
        bail!("could not determine the current uid");
    }
    Ok(format!("gui/{uid}"))
}

fn launchd_service_target() -> anyhow::Result<String> {
    Ok(format!("{}/{LAUNCHD_LABEL}", launchd_domain()?))
}

/// Run a command echoing it first; error (with stderr) on nonzero exit.
fn run(program: &str, args: &[&str]) -> anyhow::Result<()> {
    println!("$ {program} {}", args.join(" "));
    let output = Command::new(program)
        .args(args)
        .output()
        .with_context(|| format!("running {program}"))?;
    if !output.status.success() {
        bail!(
            "{program} {} failed ({}): {}",
            args.join(" "),
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}

/// Run without echoing; used where failure is an expected branch.
fn run_quiet(program: &str, args: &[&str]) -> anyhow::Result<()> {
    let output = Command::new(program)
        .args(args)
        .output()
        .with_context(|| format!("running {program}"))?;
    if !output.status.success() {
        bail!("{program} failed ({})", output.status);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn systemd_unit_shape() {
        let unit = render_systemd_unit(
            Path::new("/usr/local/bin/zeron"),
            &[
                ("PATH".into(), "/usr/bin:/bin".into()),
                ("ZERON_EDGE_URL".into(), "https://edge.example".into()),
                ("RUST_LOG".into(), "info,zeron=\"debug\"".into()),
            ],
        );
        assert!(unit.contains("ExecStart=/usr/local/bin/zeron headless\n"));
        assert!(unit.contains("Environment=\"PATH=/usr/bin:/bin\"\n"));
        assert!(unit.contains("Environment=\"ZERON_EDGE_URL=https://edge.example\"\n"));
        // Inner quotes escaped so systemd re-parses the value verbatim.
        assert!(unit.contains("Environment=\"RUST_LOG=info,zeron=\\\"debug\\\"\"\n"));
        assert!(unit.contains("StartLimitIntervalSec=60\n"));
        assert!(unit.contains("StartLimitBurst=5\n"));
        assert!(unit.contains("Restart=on-failure"));
        assert!(!unit.contains("session.json"));
        assert!(!unit.contains("ConditionPathExists"));
        assert!(unit.contains("EnvironmentFile=-%h/.zeron/env"));
        assert!(unit.contains("WantedBy=default.target"));
    }

    #[test]
    fn curl_installer_always_starts_the_local_capable_service() {
        // Normalize line endings: a Windows checkout of this Linux-only shell
        // script commonly comes through with CRLF (core.autocrlf), which
        // would otherwise break every "...\n" substring check below without
        // saying anything about the installer's actual content.
        let installer = include_str!("../../../edge/src/install.sh").replace("\r\n", "\n");
        assert!(!installer.contains("session.json"));
        assert!(installer.contains("StartLimitIntervalSec=60\n"));
        assert!(installer.contains("StartLimitBurst=5\n"));
        assert!(installer.contains("systemctl --user enable zeron"));
        assert!(installer.contains("systemctl --user restart zeron"));
    }

    #[test]
    fn installed_exe_uses_the_current_symlink() {
        // Installer-managed binary (current_exe resolves the `current` symlink to
        // the versioned dir): the unit must point back at the symlink.
        assert_eq!(
            exec_path_for(
                Path::new("/home/u/.zeron/app/0.3.0/zeron"),
                Some(Path::new("/home/u")),
            ),
            "%h/.zeron/app/current/zeron"
        );
        // Source build: literal path.
        assert_eq!(
            exec_path_for(
                Path::new("/src/target/debug/zeron"),
                Some(Path::new("/home/u"))
            ),
            "/src/target/debug/zeron"
        );
    }

    #[test]
    fn launchd_plist_shape() {
        let plist = render_launchd_plist(
            Path::new("/Users/x/zeron & co/zeron"),
            &[("ZERON_EDGE_URL".into(), "https://e?a=1&b=2".into())],
            Path::new("/Users/x/.zeron/daemon.log"),
        );
        assert!(plist.contains("<key>Label</key><string>sh.zeron.app</string>"));
        // XML-escaped exe path and env value.
        assert!(plist.contains("<string>/Users/x/zeron &amp; co/zeron</string>"));
        assert!(plist.contains("<string>https://e?a=1&amp;b=2</string>"));
        assert!(plist.contains("<string>headless</string>"));
        assert!(plist.contains("<key>SuccessfulExit</key><false/>"));
        assert!(
            plist.contains("<key>StandardOutPath</key><string>/Users/x/.zeron/daemon.log</string>")
        );
    }

    #[test]
    fn env_file_round_trips() {
        let env = vec![
            ("PATH".to_string(), "C:\\Windows\\system32".to_string()),
            ("ZERON_EDGE_URL".to_string(), "https://edge.example".to_string()),
        ];
        let rendered = render_env_file(&env);
        assert_eq!(rendered, "PATH=C:\\Windows\\system32\nZERON_EDGE_URL=https://edge.example\n");
        assert_eq!(parse_env_file(&rendered), env);
    }

    #[test]
    fn env_file_parsing_skips_blank_and_comment_lines() {
        let content = "\r\n# a comment\r\nPATH=C:\\a;C:\\b\r\n\r\nZERON_ORG_ID=org-1\n";
        assert_eq!(
            parse_env_file(content),
            vec![
                ("PATH".to_string(), "C:\\a;C:\\b".to_string()),
                ("ZERON_ORG_ID".to_string(), "org-1".to_string()),
            ]
        );
    }

    #[test]
    fn env_file_parsing_keeps_everything_after_the_first_equals() {
        // A value containing '=' (e.g. a query string) must not be truncated.
        assert_eq!(
            parse_env_file("ZERON_EDGE_URL=https://e.example?a=1&b=2"),
            vec![(
                "ZERON_EDGE_URL".to_string(),
                "https://e.example?a=1&b=2".to_string()
            )]
        );
    }

    #[test]
    fn schtasks_field_matches_case_insensitively() {
        let text = "HostName:                             DESKTOP\r\nTaskName:                             \\Zeron\r\nStatus:                               Wird ausgef\u{fc}hrt\r\nLast Result:                          0\r\n";
        assert_eq!(schtasks_field(text, "status"), Some("Wird ausgef\u{fc}hrt"));
        assert_eq!(schtasks_field(text, "last result"), Some("0"));
        assert_eq!(schtasks_field(text, "nonexistent"), None);
    }
}
