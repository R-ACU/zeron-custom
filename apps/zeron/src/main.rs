//! zeron — headed by default; `zeron headless` runs the engine alone. Both start
//! local-only without credentials. `zeron login` and `zeron logout` select the
//! profile used by the next engine start without mutating a live runtime.

mod auth_cli;
mod daemon;
#[cfg(windows)]
mod single_instance;
mod update_cli;

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "zeron", about = "Multi-device controller for coding agents")]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
    /// Open a Zeron conversation URL (also accepted as `--open-url`, which
    /// is how a registered `zeron://` deep link invokes the exe).
    #[arg(value_name = "URL")]
    open_url: Option<String>,
    /// Same as the bare `[URL]` positional; the registered `zeron://`
    /// scheme handler invokes the exe with this flag.
    #[arg(long = "open-url", value_name = "URL")]
    open_url_flag: Option<String>,
    /// Set by the start-at-login registration (Settings > General,
    /// `zeron_ui::autostart::AUTOSTART_FLAG`): give a background engine
    /// starting at the same sign-in time to come up before the window
    /// decides between attaching and embedding.
    #[arg(long = "autostart", hide = true)]
    autostart: bool,
}

#[derive(Subcommand)]
enum Command {
    /// Run the engine without a UI (local-only unless a saved session enables sync).
    Headless,
    /// Sign in and enable sync on the next engine start.
    Login,
    /// Remove the saved session and return to local-only on the next start.
    Logout,
    /// Show workspace mode, optional auth, and engine status.
    Status,
    /// Live sync introspection from the running engine: per-room connection
    /// state, last pushed-frame/ack ages, rejoin/probe/resync counters.
    Sync,
    #[cfg(target_os = "linux")]
    /// Trigger an Appshot in the running headed instance (desktop shortcut fallback).
    Appshot,
    /// Manage `zeron headless` as a background service (launchd / systemd --user).
    Daemon {
        #[command(subcommand)]
        command: DaemonCommand,
    },
    /// Check for a newer release and apply it (download → verify → swap →
    /// service restart). `--check` only reports (exits 1 when one is available).
    Update {
        #[arg(long)]
        check: bool,
    },
}

#[derive(Subcommand)]
enum DaemonCommand {
    /// Install, enable, and start the service (captures ZERON_* env).
    Install,
    /// Stop and remove the service.
    Uninstall,
    /// Start the installed service.
    Start,
    /// Stop the service.
    Stop,
    /// Restart the service.
    Restart,
    /// Show the service manager's view of the daemon.
    Status,
}

/// Production edge (Cloudflare Worker + Durable Objects on the zeron.sh zone).
/// `ZERON_EDGE_URL` overrides (local dev / self-hosting).
const DEFAULT_EDGE_URL: &str = "https://edge.zeron.sh";

/// Production WorkOS AuthKit client id — public knowledge (it appears in every
/// authorize URL), so baking it in is safe. Overridden by `ZERON_WORKOS_CLIENT_ID`;
/// set it to the empty string — or set a dev bearer via `ZERON_EDGE_TOKEN` — to
/// force dev-mode auth instead.
const DEFAULT_WORKOS_CLIENT_ID: &str = "client_01KWD0EAKZKD50YCQJNYSRE4BY";

fn edge_url_from_env() -> String {
    std::env::var("ZERON_EDGE_URL")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| DEFAULT_EDGE_URL.into())
}

/// WorkOS client id resolution: explicit env wins (empty string = dev mode);
/// otherwise a `ZERON_EDGE_TOKEN` dev bearer keeps dev mode (smoke tests,
/// local wrangler); otherwise the baked production client id makes optional
/// sync available while a bare start remains local-only.
fn workos_client_id_from_env(edge_token: &Option<String>) -> Option<String> {
    match std::env::var("ZERON_WORKOS_CLIENT_ID") {
        Ok(v) if v.trim().is_empty() => None,
        Ok(v) => Some(v),
        Err(_) if edge_token.is_some() => None,
        Err(_) => Some(DEFAULT_WORKOS_CLIENT_ID.into()),
    }
}

/// mimalloc, macOS only: libmalloc never returns the streaming churn's
/// high-water pages, so transient allocation became permanent RSS
/// (docs/memory-plan.md §1). Pinned to mimalloc v2 in the workspace manifest —
/// the crate's default v3 has the same pathology (churn retained as permanent
/// RSS, ~6x glibc's growth on identical workloads, no idle recovery). Linux
/// measured flat on glibc, so it keeps the system allocator.
#[cfg(target_os = "macos")]
#[global_allocator]
static ALLOC: mimalloc::MiMalloc = mimalloc::MiMalloc;

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    // Windows only: a Scheduled Task (`zeron daemon install`'s equivalent of
    // launchd's EnvironmentVariables / systemd's EnvironmentFile=) has no way
    // to bake in an environment block itself, so `daemon::install` writes
    // `<data_dir>\env` and headless applies it here, before anything below
    // reads `ZERON_*` or `PATH`.
    #[cfg(windows)]
    if matches!(cli.command, Some(Command::Headless)) {
        apply_env_file_from_data_dir();
    }

    // Long-running modes log at info, one-shot CLI commands at warn (RUST_LOG
    // overrides either).
    // loro's internal block-encode diagnostics log at info and flood
    // journald on every snapshot export — enough to fill a disk on a
    // long-running headless host. Quiet them by default (RUST_LOG still
    // overrides the whole filter).
    let long_running = matches!(&cli.command, None | Some(Command::Headless));
    let default_filter = if long_running {
        "info,loro_internal=warn,loro=warn"
    } else {
        "warn"
    };
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| default_filter.into());
    // Long-running modes mirror stdout logging to {data_dir}/logs — a headed
    // app launched from Finder has no visible stdout, which left every sync
    // wedge report ("stale until restart") with zero diagnostics even though
    // the engine logs the exact failure line. One file per launch, previous
    // launch kept as `.old`.
    let log_file = if long_running {
        let mode = if cli.command.is_some() {
            "headless"
        } else {
            "headed"
        };
        open_log_file(mode)
    } else {
        None
    };
    {
        use tracing_subscriber::layer::SubscriberExt;
        use tracing_subscriber::util::SubscriberInitExt;
        let registry = tracing_subscriber::registry()
            .with(filter)
            .with(tracing_subscriber::fmt::layer());
        match log_file {
            Some(file) => registry
                .with(
                    tracing_subscriber::fmt::layer()
                        .with_ansi(false)
                        .with_writer(std::sync::Arc::new(file)),
                )
                .init(),
            None => registry.init(),
        }
    }

    if long_running {
        // Finder launches have no visible stderr. Mirror the panic location
        // and backtrace into the same rotating log as engine diagnostics.
        let default_hook = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            tracing::error!(panic = %info,
                backtrace = %std::backtrace::Backtrace::force_capture(),
                "application panic");
            default_hook(info);
        }));
    }

    match cli.command {
        Some(Command::Headless) => {
            let runtime = tokio::runtime::Runtime::new()?;
            runtime.block_on(async {
                let engine = zeron_engine::Engine::new(engine_config_from_env());
                engine.run().await
            })
        }
        Some(Command::Login) => {
            let runtime = tokio::runtime::Runtime::new()?;
            runtime.block_on(auth_cli::login(engine_config_from_env()))
        }
        Some(Command::Logout) => {
            let runtime = tokio::runtime::Runtime::new()?;
            runtime.block_on(auth_cli::logout(engine_config_from_env()))
        }
        Some(Command::Status) => {
            let runtime = tokio::runtime::Runtime::new()?;
            runtime.block_on(auth_cli::status(engine_config_from_env()))
        }
        Some(Command::Sync) => {
            let runtime = tokio::runtime::Runtime::new()?;
            runtime.block_on(sync_cli(engine_config_from_env().ipc_port))
        }
        #[cfg(target_os = "linux")]
        Some(Command::Appshot) => {
            zeron_ui::appshots::request_running_appshot(&engine_config_from_env().data_dir)
                .map_err(anyhow::Error::msg)
        }
        Some(Command::Update { check }) => {
            let runtime = tokio::runtime::Runtime::new()?;
            runtime.block_on(update_cli::update(&edge_url_from_env(), check))
        }
        Some(Command::Daemon { command }) => match command {
            DaemonCommand::Install => daemon::install(&engine_config_from_env().data_dir),
            DaemonCommand::Uninstall => daemon::uninstall(),
            DaemonCommand::Start => daemon::start(),
            DaemonCommand::Stop => daemon::stop(),
            DaemonCommand::Restart => daemon::restart(),
            DaemonCommand::Status => daemon::status(),
        },
        None => {
            // Either form of the deep link wins: a registered `zeron://`
            // handler always uses `--open-url`, a manual/shortcut launch may
            // pass the bare positional instead.
            let open_url = cli.open_url.or(cli.open_url_flag);

            // Windows only: a second headed launch hands its URL (if any) to
            // the already-running instance's window and exits instead of
            // opening a second UI. `zeron headless`/`status`/`daemon`/etc.
            // never reach this branch, so they are unaffected.
            // `ZERON_ALLOW_SECOND_INSTANCE=1` skips the guard: a development
            // build (own `ZERON_DATA_DIR` / `ZERON_IPC_PORT`) can then run
            // beside the installed app instead of handing its launch over to
            // it. Never set in normal use — two UIs on one data dir fight.
            #[cfg(windows)]
            if std::env::var("ZERON_ALLOW_SECOND_INSTANCE").as_deref() != Ok("1")
                && single_instance::guard(open_url.as_deref())
                    == single_instance::SingleInstance::ForwardedToRunningInstance
            {
                return Ok(());
            }

            let ipc_port = std::env::var("ZERON_IPC_PORT")
                .ok()
                .and_then(|p| p.parse().ok())
                .unwrap_or(27654);
            if cli.autostart {
                wait_for_login_engine(ipc_port);
            }

            let edge_token = std::env::var("ZERON_EDGE_TOKEN").ok();
            // Headed: the UI probes ZERON_IPC_PORT and connects to a running
            // daemon, or embeds the engine in-process (ARCHITECTURE §1).
            zeron_ui::run_app(zeron_ui::UiConfig {
                data_dir: std::env::var_os("ZERON_DATA_DIR")
                    .map(std::path::PathBuf::from)
                    .unwrap_or_else(dirs_data_dir),
                ipc_port,
                edge_url: edge_url_from_env(),
                workos_client_id: workos_client_id_from_env(&edge_token),
                edge_token,
                org_id: std::env::var("ZERON_ORG_ID").ok(),
                default_harness: zeron_ui::HarnessId::ClaudeCode,
                initial_url: open_url,
            });
            Ok(())
        }
    }
}

/// Upper bound for [`wait_for_login_engine`]: sign-in is when disks are busiest,
/// but a window that never appears is worse than an embedded engine.
const LOGIN_ENGINE_WAIT: std::time::Duration = std::time::Duration::from_secs(20);

/// A login launch of the window races the background engine registered for
/// the same sign-in (Settings > General, or `zeron daemon install`). If the
/// window wins it embeds the engine and takes the data-dir lock, the
/// background engine exits on the lock, and automations stop as soon as the
/// window is closed. So when a background engine is registered, wait (bounded)
/// for its IPC listener first; the UI bootstrap then attaches to it.
fn wait_for_login_engine(ipc_port: u16) {
    let engine_registered =
        zeron_ui::autostart::read_status().is_ok_and(|status| status.engine_starts_at_login());
    if !engine_registered {
        return;
    }
    let addr = std::net::SocketAddr::from(([127, 0, 0, 1], ipc_port));
    let deadline = std::time::Instant::now() + LOGIN_ENGINE_WAIT;
    while std::time::Instant::now() < deadline {
        if std::net::TcpStream::connect_timeout(&addr, std::time::Duration::from_millis(250))
            .is_ok()
        {
            tracing::info!(port = ipc_port, "background engine is up; attaching");
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(250));
    }
    tracing::warn!(
        port = ipc_port,
        "background engine did not start listening in time; the window embeds its own engine"
    );
}

/// The env-resolved engine configuration shared by `headless`, `login`,
/// `logout`, and `status` — one resolution so the CLI auth commands always
/// operate on the exact session the daemon will load.
fn engine_config_from_env() -> zeron_engine::EngineConfig {
    // Dev-mode bearer (no WorkOS): an explicit token enables sync.
    let edge_token = std::env::var("ZERON_EDGE_TOKEN").ok();
    zeron_engine::EngineConfig {
        data_dir: std::env::var_os("ZERON_DATA_DIR")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(dirs_data_dir),
        edge_url: edge_url_from_env(),
        ipc_port: std::env::var("ZERON_IPC_PORT")
            .ok()
            .and_then(|p| p.parse().ok())
            .unwrap_or(27654),
        default_harness: harness_from_env(),
        // WorkOS mode: the signed-in session's org wins; ZERON_ORG_ID (dev
        // default "dev-org") scopes the workspace room otherwise.
        org_id: std::env::var("ZERON_ORG_ID").ok(),
        // Real auth against production by default; see
        // `workos_client_id_from_env` for the dev-mode escape hatches.
        workos_client_id: workos_client_id_from_env(&edge_token),
        edge_token,
    }
}

/// `ZERON_HARNESS` (kebab-case id) picks the default harness for chats without a
/// config row — `mock` powers the e2e smoke; default `claude-code`.
fn harness_from_env() -> zeron_engine::HarnessId {
    match std::env::var("ZERON_HARNESS").as_deref().map(str::trim) {
        Ok("mock") => zeron_engine::HarnessId::Mock,
        Ok("codex") => zeron_engine::HarnessId::Codex,
        Ok("cursor") => zeron_engine::HarnessId::Cursor,
        Ok("devin") => zeron_engine::HarnessId::Devin,
        Ok("grok") => zeron_engine::HarnessId::Grok,
        Ok("hermes") => zeron_engine::HarnessId::Hermes,
        Ok("pi") => zeron_engine::HarnessId::Pi,
        _ => zeron_engine::HarnessId::ClaudeCode,
    }
}

/// Resolves `$HOME` (unix/macOS) or `%USERPROFILE%` (Windows, falling back to
/// `%HOMEDRIVE%%HOMEPATH%` for the rare profile that only sets those).
fn home_dir() -> std::path::PathBuf {
    #[cfg(windows)]
    {
        if let Some(profile) = std::env::var_os("USERPROFILE") {
            if !profile.is_empty() {
                return std::path::PathBuf::from(profile);
            }
        }
        let drive = std::env::var("HOMEDRIVE").unwrap_or_default();
        let path = std::env::var("HOMEPATH").unwrap_or_default();
        if !drive.is_empty() && !path.is_empty() {
            return std::path::PathBuf::from(format!("{drive}{path}"));
        }
        panic!("USERPROFILE not set and HOMEDRIVE/HOMEPATH not set");
    }
    #[cfg(not(windows))]
    std::path::PathBuf::from(std::env::var_os("HOME").expect("HOME not set"))
}

fn dirs_data_dir() -> std::path::PathBuf {
    let home = home_dir();
    let dir = home.join(".zeron");
    // One-shot 0.2.0 migration: adopt the pre-rename data dir (sign-in,
    // device identity, prefs) instead of starting fresh.
    if !dir.exists() {
        let old = home.join(".comet-native");
        if old.exists() && std::fs::rename(&old, &dir).is_ok() {
            eprintln!("migrated data dir {} -> {}", old.display(), dir.display());
        }
    }
    dir
}

/// Windows only: applies `<data_dir>\env` (written by `daemon::install`) to the
/// current process environment before anything else reads `ZERON_*`/`PATH`.
/// This is the Scheduled Task equivalent of systemd's `EnvironmentFile=-%h/.zeron/env`.
/// A missing file is fine (not installed as a service, or a plain manual
/// `zeron headless` run); a var already set in the process environment is
/// never overridden, matching systemd's precedence.
#[cfg(windows)]
fn apply_env_file_from_data_dir() {
    let data_dir = std::env::var_os("ZERON_DATA_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(dirs_data_dir);
    let path = daemon::env_file_path(&data_dir);
    let Ok(content) = std::fs::read_to_string(&path) else {
        return;
    };
    for (key, value) in daemon::parse_env_file(&content) {
        if std::env::var_os(&key).is_none() {
            // SAFETY: called once, single-threaded, before the tokio runtime
            // (or any other thread) starts.
            unsafe { std::env::set_var(&key, &value) };
        }
    }
}

/// `zeron sync`: dial the running engine's IPC and print per-room sync state.
/// The introspection surface every 2026-08 incident was missing — "is this
/// device's workspace room actually receiving?" as a one-liner.
async fn sync_cli(ipc_port: u16) -> anyhow::Result<()> {
    let client = zeron_rpc::connect_ws(&format!("ws://127.0.0.1:{ipc_port}"))
        .await
        .map_err(|e| {
            anyhow::anyhow!("no engine listening on 127.0.0.1:{ipc_port} ({e}) — is zeron running?")
        })?;
    let status = client
        .call(zeron_rpc::methods::SYNC_STATUS, serde_json::json!({}))
        .await
        .map_err(|e| anyhow::anyhow!("SyncStatus failed: {e}"))?;
    let now = status.get("nowMs").and_then(|v| v.as_i64()).unwrap_or(0);
    let age = |ms: i64| -> String {
        if ms <= 0 {
            return "never".into();
        }
        let s = (now - ms).max(0) / 1000;
        if s >= 3600 {
            format!("{}h{}m ago", s / 3600, (s % 3600) / 60)
        } else if s >= 60 {
            format!("{}m{}s ago", s / 60, s % 60)
        } else {
            format!("{s}s ago")
        }
    };
    let room_line = |room: Option<&serde_json::Value>| -> String {
        let Some(room) = room else {
            return "no room (dialing or edge-less)".into();
        };
        let get = |k: &str| room.get(k).and_then(|v| v.as_i64()).unwrap_or(0);
        // REJECTED is loud and only shown when nonzero: rejected writes with
        // a fresh-looking room is exactly the latched-session wedge
        // (2026-08-04) this readout previously masked.
        let rejected = get("rejected");
        format!(
            "{} pushed {} · acked {} · rejoins {} probes {} resyncs {} drops {}{}",
            if room.get("connected").and_then(|v| v.as_bool()) == Some(true) {
                "connected ·"
            } else {
                "DISCONNECTED ·"
            },
            age(get("lastPushedMs")),
            age(get("lastAckMs")),
            get("rejoins"),
            get("probes"),
            get("fullResyncs"),
            get("disconnects"),
            if rejected > 0 {
                format!(" REJECTED {rejected}")
            } else {
                String::new()
            },
        )
    };
    println!(
        "Device:    {}",
        status
            .get("deviceId")
            .and_then(|v| v.as_str())
            .unwrap_or("?")
    );
    println!(
        "Workspace: {}",
        room_line(status.get("workspace").filter(|v| !v.is_null()))
    );
    let chats = status
        .get("chats")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    if chats.is_empty() {
        println!("Chats:     none open");
    }
    // Chat rooms speak chat2: cursor/head tell "am I caught up?", pending
    // tells "did my writes leave?", resets/rejected are the loud tells.
    let chat_line = |room: Option<&serde_json::Value>| -> String {
        let Some(room) = room else {
            return "no room (dialing or edge-less)".into();
        };
        let get = |k: &str| room.get(k).and_then(|v| v.as_u64()).unwrap_or(0);
        let resets = get("serverResets");
        let rejected = get("rejected");
        format!(
            "{} cursor {}/{} · pending {} · rows {} ({}KB) · rejoins {} drops {}{}{}",
            if room.get("connected").and_then(|v| v.as_bool()) == Some(true) {
                "connected ·"
            } else {
                "DISCONNECTED ·"
            },
            get("cursor"),
            get("headSeq"),
            get("pendingPushes"),
            get("rowCount"),
            get("rowBytes") / 1024,
            get("rejoins"),
            get("disconnects"),
            if resets > 0 {
                format!(" RESETS {resets}")
            } else {
                String::new()
            },
            if rejected > 0 {
                format!(" REJECTED {rejected}")
            } else {
                String::new()
            },
        )
    };
    for chat in &chats {
        println!(
            "Chat {}: {}",
            chat.get("chatId")
                .and_then(|v| v.as_str())
                .map(|s| &s[..s.len().min(8)])
                .unwrap_or("?"),
            chat_line(chat.get("room").filter(|v| !v.is_null()))
        );
    }
    Ok(())
}

/// `{data_dir}/logs/zeron-{mode}.log`, previous launch preserved as `.old`.
/// Headed and headless are separate files so an embedded-engine app and a
/// daemon on the same machine never interleave writes.
///
/// The returned file holds an exclusive `flock` for the process lifetime:
/// rotate-on-launch is only safe when nothing is still WRITING the current
/// file. On 2026-08-04 a dev build launched twice next to the running
/// installed app — the first rename put the daemon's live log at `.old`, the
/// second unlinked it entirely, and the daemon spent the rest of the incident
/// logging to an orphaned inode (an entire day of sync diagnostics gone at
/// the exact moment they were needed). A launch that finds the canonical file
/// locked logs to `zeron-{mode}.{pid}.log` instead; the next lock-holding
/// launch sweeps pid-suffixed files older than a week.
fn open_log_file(mode: &str) -> Option<std::fs::File> {
    let dir = std::env::var_os("ZERON_DATA_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(dirs_data_dir)
        .join("logs");
    open_log_file_in(&dir, mode)
}

/// Dir-parameterized body of [`open_log_file`] (unit-testable without env).
fn open_log_file_in(dir: &std::path::Path, mode: &str) -> Option<std::fs::File> {
    std::fs::create_dir_all(dir).ok()?;
    let path = dir.join(format!("zeron-{mode}.log"));
    #[cfg(unix)]
    {
        use std::os::unix::io::AsRawFd;
        // Probe the CURRENT inode for a live writer before touching it.
        let preexisting = path.exists();
        let existing = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
            .ok()?;
        let rc = unsafe { libc::flock(existing.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        if rc != 0 {
            // A live process owns the canonical log — leave it alone.
            return std::fs::File::create(
                dir.join(format!("zeron-{mode}.{}.log", std::process::id())),
            )
            .ok();
        }
        // No live writer: rotate, create fresh, and lock it as ours. (The
        // probe's flock dies with `existing`; a first-ever launch has nothing
        // to rotate — the probe itself created the empty file.)
        drop(existing);
        if preexisting {
            let _ = std::fs::rename(&path, dir.join(format!("zeron-{mode}.log.old")));
        }
        let file = std::fs::File::create(&path).ok()?;
        unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        sweep_stale_pid_logs(dir, mode);
        Some(file)
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        // Probe the CURRENT inode for a live writer before touching it, same
        // shape as the unix branch above but via exclusive sharing instead of
        // flock: `share_mode(0)` denies any other open (read, write, or
        // delete) of the file while we hold it, so a second instance's own
        // `share_mode(0)` open fails immediately (surfaced as
        // `PermissionDenied`) instead of silently succeeding the way a
        // default (shared) open would.
        let preexisting = path.exists();
        let probe = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .share_mode(0)
            .open(&path);
        let probe = match probe {
            Ok(file) => file,
            Err(_) => {
                // A live process owns the canonical log; leave it alone.
                return std::fs::File::create(
                    dir.join(format!("zeron-{mode}.{}.log", std::process::id())),
                )
                .ok();
            }
        };
        // No live writer: rotate, create fresh, and hold it open exclusively
        // for the rest of this process's lifetime (this becomes the next
        // launch's contention check, mirroring the unix flock handoff).
        drop(probe);
        if preexisting {
            let _ = std::fs::rename(&path, dir.join(format!("zeron-{mode}.log.old")));
        }
        let file = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .share_mode(0)
            .open(&path)
            .ok()?;
        sweep_stale_pid_logs(dir, mode);
        Some(file)
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = std::fs::rename(&path, dir.join(format!("zeron-{mode}.log.old")));
        std::fs::File::create(&path).ok()
    }
}

#[cfg(test)]
mod cli_tests {
    use clap::Parser;

    use super::Cli;

    #[test]
    fn login_registration_flag_parses() {
        let cli = Cli::try_parse_from(["zeron", zeron_ui::autostart::AUTOSTART_FLAG]).unwrap();
        assert!(cli.autostart);
        assert!(cli.command.is_none());
        assert!(cli.open_url.is_none());
        let cli = Cli::try_parse_from(["zeron"]).unwrap();
        assert!(!cli.autostart);
    }
}

#[cfg(all(test, any(unix, windows)))]
mod log_file_tests {
    use super::open_log_file_in;

    #[test]
    fn second_launch_never_rotates_a_live_processes_log() {
        let dir = tempfile::tempdir().unwrap();
        let dir = dir.path();
        // First launch owns the canonical file and keeps writing.
        let first = open_log_file_in(dir, "headed").expect("first log");
        assert!(dir.join("zeron-headed.log").is_file());
        // Second launch while the first is alive: canonical file untouched,
        // pid-suffixed overflow file instead (the 2026-08-04 clobber).
        let second = open_log_file_in(dir, "headed").expect("second log");
        let pid_path = dir.join(format!("zeron-headed.{}.log", std::process::id()));
        assert!(pid_path.is_file(), "expected pid-suffixed overflow log");
        assert!(
            !dir.join("zeron-headed.log.old").exists(),
            "live canonical log must not be rotated away"
        );
        drop(second);
        // After the owner exits, a fresh launch rotates normally.
        drop(first);
        let third = open_log_file_in(dir, "headed").expect("third log");
        assert!(
            dir.join("zeron-headed.log.old").is_file(),
            "rotation resumes"
        );
        drop(third);
    }
}

/// Delete `zeron-{mode}.{pid}.log` overflow files older than a week — they
/// only exist when a second instance raced a live one for the canonical log.
/// Pure `std::fs`, so it runs on every platform that has a pid-suffixed
/// overflow path (unix flock contention, windows sharing-violation contention).
fn sweep_stale_pid_logs(dir: &std::path::Path, mode: &str) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let prefix = format!("zeron-{mode}.");
    let week = std::time::Duration::from_secs(7 * 24 * 60 * 60);
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        let Some(middle) = name
            .strip_prefix(&prefix)
            .and_then(|rest| rest.strip_suffix(".log"))
        else {
            continue;
        };
        if !middle.chars().all(|c| c.is_ascii_digit()) {
            continue;
        }
        let stale = entry
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.elapsed().ok())
            .is_some_and(|age| age > week);
        if stale {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}
