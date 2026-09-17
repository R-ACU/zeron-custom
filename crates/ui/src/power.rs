//! "Keep laptop awake while agents are running" (the `keepAwakeWhileRunning`
//! ui-setting): a sleep veto that is held only while an agent run is in
//! progress on this device, plus a short grace period after the last run
//! ends so back-to-back turns do not flap the veto.
//!
//! Two independent mechanisms are needed on Windows:
//!
//! - `SetThreadExecutionState(ES_CONTINUOUS | ES_SYSTEM_REQUIRED)` vetoes the
//!   idle-sleep transition. On Modern Standby (S0 Low Power Idle) machines a
//!   lid close is that same transition, so this is what actually keeps the
//!   laptop working with the lid shut. The request is bound to the calling
//!   thread and lapses when that thread exits, so a dedicated long-lived
//!   thread owns it (and owns the grace-period state machine).
//! - The power plan's lid-close action (`powercfg ... LIDACTION`), which on
//!   S3 machines fires Sleep regardless of any per-app request. While the veto
//!   is held it is set to 0 ("Do nothing") and the previous AC/DC values are
//!   restored on release. The previous values live in a sidecar file
//!   (`{data_dir}/lid-restore.json`): its mere presence means "we are
//!   overriding", and [`init`] restores it after a crash.
//!
//! Every platform other than Windows compiles the pure policy only; the
//! public entry points are no-ops there.

use std::path::PathBuf;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use zeron_proto::Session;
use zeron_proto::view::{Indicator, effective_indicator};

/// How long the veto outlives the last run: covers the gap between queued
/// turns and a quick follow-up prompt without waking/sleeping in between.
pub const GRACE_PERIOD: Duration = Duration::from_secs(180);

/// Sidecar file name under the data directory.
pub const LID_RESTORE_FILE: &str = "lid-restore.json";

// ---------------------------------------------------------------------------
// Run-activity signal (pure)
// ---------------------------------------------------------------------------

/// Whether any session on this device is live and working. Sessions of other
/// devices (synced through the edge) never keep this laptop awake; an unknown
/// local id (engine not up yet) counts every session, which is harmless since
/// there are none before the engine serves them. Staleness follows the
/// sidebar's 45s window, so a crashed backend releases the veto on its own.
pub fn any_local_run_active(
    sessions: &[Session],
    local_device_id: Option<&str>,
    now: DateTime<Utc>,
) -> bool {
    sessions
        .iter()
        .filter(|s| local_device_id.is_none_or(|id| s.device_id == id))
        .any(|s| effective_indicator(Some(s), now) == Indicator::Working)
}

// ---------------------------------------------------------------------------
// Grace-period state machine (pure)
// ---------------------------------------------------------------------------

/// A veto transition the driver has to act on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Transition {
    Hold,
    Release,
}

/// Decides when the sleep veto is held: while the setting is on and a run is
/// active, and for [`GRACE_PERIOD`] after the last run ended. Turning the
/// setting off releases immediately.
#[derive(Debug)]
pub struct KeepAwakePolicy {
    enabled: bool,
    running: bool,
    /// When the last run ended, while the grace period may still be pending.
    idle_since: Option<Instant>,
    held: bool,
}

impl Default for KeepAwakePolicy {
    fn default() -> Self {
        Self::new()
    }
}

impl KeepAwakePolicy {
    pub fn new() -> Self {
        Self {
            enabled: false,
            running: false,
            idle_since: None,
            held: false,
        }
    }

    pub fn held(&self) -> bool {
        self.held
    }

    pub fn set_enabled(&mut self, enabled: bool, now: Instant) -> Option<Transition> {
        self.enabled = enabled;
        self.settle(now)
    }

    pub fn set_running(&mut self, running: bool, now: Instant) -> Option<Transition> {
        if running {
            self.idle_since = None;
        } else if self.running {
            self.idle_since = Some(now);
        }
        self.running = running;
        self.settle(now)
    }

    /// Re-evaluate at `now` (the grace period may have elapsed).
    pub fn tick(&mut self, now: Instant) -> Option<Transition> {
        self.settle(now)
    }

    /// When the driver must call [`Self::tick`] next; `None` when nothing is
    /// pending.
    pub fn next_deadline(&self) -> Option<Instant> {
        if self.held && !self.running {
            self.idle_since.map(|at| at + GRACE_PERIOD)
        } else {
            None
        }
    }

    fn desired(&self, now: Instant) -> bool {
        if !self.enabled {
            return false;
        }
        self.running
            || self
                .idle_since
                .is_some_and(|at| now.duration_since(at) < GRACE_PERIOD)
    }

    fn settle(&mut self, now: Instant) -> Option<Transition> {
        let want = self.desired(now);
        if want == self.held {
            return None;
        }
        self.held = want;
        if !want {
            self.idle_since = None;
        }
        Some(if want {
            Transition::Hold
        } else {
            Transition::Release
        })
    }
}

// ---------------------------------------------------------------------------
// Public entry points
// ---------------------------------------------------------------------------

static ENABLED: AtomicBool = AtomicBool::new(false);
static RUNNING: AtomicBool = AtomicBool::new(false);
static DATA_DIR: OnceLock<PathBuf> = OnceLock::new();

/// Remember the data directory (sidecar location) and undo a lid-action
/// override a crashed previous instance left behind. Call once at boot.
pub fn init(data_dir: PathBuf) {
    let _ = DATA_DIR.set(data_dir);
    #[cfg(windows)]
    win::restore_after_crash();
}

/// The persisted setting changed (or was loaded at boot).
pub fn set_enabled(enabled: bool) {
    if ENABLED.swap(enabled, Ordering::Relaxed) != enabled {
        tracing::info!(enabled, "keep-awake setting changed");
        #[cfg(windows)]
        win::send(win::Msg::Enabled(enabled));
    }
}

/// The run-activity signal; cheap to call every second, forwards on change.
pub fn set_agents_running(running: bool) {
    if RUNNING.swap(running, Ordering::Relaxed) != running {
        #[cfg(windows)]
        win::send(win::Msg::Running(running));
    }
}

/// Release the veto and restore the lid action synchronously. Call on app
/// quit; the execution-state request dies with the process anyway, but the
/// power plan does not.
pub fn shutdown() {
    #[cfg(windows)]
    win::shutdown();
}

#[cfg_attr(not(windows), allow(dead_code))]
fn sidecar_path() -> Option<PathBuf> {
    DATA_DIR.get().map(|dir| dir.join(LID_RESTORE_FILE))
}

// ---------------------------------------------------------------------------
// Windows driver
// ---------------------------------------------------------------------------

#[cfg(windows)]
mod win {
    use super::{KeepAwakePolicy, Transition, sidecar_path};
    use std::process::{Command, Stdio};
    use std::sync::mpsc::{RecvTimeoutError, Sender, SyncSender, sync_channel};
    use std::sync::{Mutex, OnceLock};
    use std::time::{Duration, Instant};

    use serde::{Deserialize, Serialize};
    use windows_sys::Win32::System::Power::{
        ES_CONTINUOUS, ES_SYSTEM_REQUIRED, SetThreadExecutionState,
    };

    pub(super) enum Msg {
        Enabled(bool),
        Running(bool),
        /// Release now and acknowledge on the channel.
        Shutdown(SyncSender<()>),
    }

    static TX: OnceLock<Sender<Msg>> = OnceLock::new();
    /// Serializes powercfg access between the driver thread and the quit path.
    static LID_LOCK: Mutex<()> = Mutex::new(());

    /// Re-assert a held request this often; belt and braces against anything
    /// that could drop it.
    const REASSERT_EVERY: Duration = Duration::from_secs(60);
    const POWERCFG_TIMEOUT: Duration = Duration::from_secs(4);
    /// Don't pop a console window when shelling out to powercfg.
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;

    pub(super) fn send(msg: Msg) {
        let tx = TX.get_or_init(spawn_driver);
        let _ = tx.send(msg);
    }

    pub(super) fn shutdown() {
        let Some(tx) = TX.get() else {
            // Never held anything; nothing to undo beyond a stale sidecar.
            restore_lid_action();
            return;
        };
        let (ack_tx, ack_rx) = sync_channel(1);
        if tx.send(Msg::Shutdown(ack_tx)).is_ok() {
            let _ = ack_rx.recv_timeout(Duration::from_secs(6));
        }
    }

    pub(super) fn restore_after_crash() {
        let Some(path) = sidecar_path() else {
            return;
        };
        if path.exists() {
            tracing::warn!(
                path = %path.display(),
                "lid-action override left behind by a previous instance; restoring"
            );
            std::thread::Builder::new()
                .name("zeron-power-restore".into())
                .spawn(restore_lid_action)
                .ok();
        }
    }

    fn spawn_driver() -> Sender<Msg> {
        let (tx, rx) = std::sync::mpsc::channel::<Msg>();
        let result = std::thread::Builder::new()
            .name("zeron-power".into())
            .spawn(move || {
                let mut policy = KeepAwakePolicy::new();
                loop {
                    let now = Instant::now();
                    let wait = match policy.next_deadline() {
                        Some(deadline) => deadline.saturating_duration_since(now),
                        None if policy.held() => REASSERT_EVERY,
                        None => Duration::from_secs(3600),
                    }
                    .min(REASSERT_EVERY);
                    let transition = match rx.recv_timeout(wait) {
                        Ok(Msg::Enabled(enabled)) => policy.set_enabled(enabled, Instant::now()),
                        Ok(Msg::Running(running)) => policy.set_running(running, Instant::now()),
                        Ok(Msg::Shutdown(ack)) => {
                            if policy.held() {
                                apply(Transition::Release);
                            }
                            let _ = ack.send(());
                            return;
                        }
                        Err(RecvTimeoutError::Timeout) => {
                            let t = policy.tick(Instant::now());
                            if t.is_none() && policy.held() {
                                // Periodic re-assert of the continuous request.
                                unsafe { SetThreadExecutionState(ES_CONTINUOUS | ES_SYSTEM_REQUIRED) };
                            }
                            t
                        }
                        Err(RecvTimeoutError::Disconnected) => {
                            if policy.held() {
                                apply(Transition::Release);
                            }
                            return;
                        }
                    };
                    if let Some(transition) = transition {
                        apply(transition);
                    }
                }
            });
        if let Err(err) = result {
            tracing::error!(error = %err, "could not spawn the keep-awake thread");
        }
        tx
    }

    /// Runs on the driver thread, which therefore owns the execution-state
    /// request.
    fn apply(transition: Transition) {
        match transition {
            Transition::Hold => {
                let prev = unsafe { SetThreadExecutionState(ES_CONTINUOUS | ES_SYSTEM_REQUIRED) };
                tracing::info!(prev, "keep-awake veto held (ES_SYSTEM_REQUIRED)");
                override_lid_action();
            }
            Transition::Release => {
                let prev = unsafe { SetThreadExecutionState(ES_CONTINUOUS) };
                tracing::info!(prev, "keep-awake veto released");
                restore_lid_action();
            }
        }
    }

    /// The lid-close action indexes (AC = plugged in, DC = on battery) as the
    /// raw powercfg strings (`0x00000001`) so they round-trip without parsing.
    #[derive(Serialize, Deserialize)]
    struct LidActions {
        ac: String,
        dc: String,
    }

    /// Set the lid-close action to "Do nothing", saving the current values
    /// first. Idempotent: an existing sidecar means the override is already
    /// in place and must not be overwritten with the zeroed values.
    fn override_lid_action() {
        let _guard = LID_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let Some(sidecar) = sidecar_path() else {
            return;
        };
        if sidecar.exists() {
            return;
        }
        // Modern Standby machines hide the lid action by default, and a hidden
        // setting reports no value. Unhiding works without elevation and
        // persists harmlessly; failures (already visible) are ignored.
        let _ = powercfg(&["/attributes", "SUB_BUTTONS", "LIDACTION", "-ATTRIB_HIDE"]);
        let (ac, dc) = match read_lid_actions() {
            Ok(v) => v,
            Err(err) => {
                tracing::warn!(error = %err, "lid action not readable; idle veto only");
                return;
            }
        };
        let saved = match serde_json::to_string(&LidActions {
            ac: ac.clone(),
            dc: dc.clone(),
        }) {
            Ok(s) => s,
            Err(_) => return,
        };
        if let Some(dir) = sidecar.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        if let Err(err) = std::fs::write(&sidecar, saved) {
            tracing::warn!(error = %err, "could not write lid-restore sidecar; leaving lid action alone");
            return;
        }
        match set_lid_actions("0", "0") {
            Ok(()) => tracing::info!(ac = %ac, dc = %dc, "lid-close action overridden to Do nothing"),
            Err(err) => {
                tracing::warn!(error = %err, "could not override the lid-close action");
                let _ = std::fs::remove_file(&sidecar);
            }
        }
    }

    /// Restore the saved lid-close action and remove the sidecar. A missing
    /// sidecar means there is nothing to restore.
    fn restore_lid_action() {
        let _guard = LID_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let Some(sidecar) = sidecar_path() else {
            return;
        };
        let Ok(text) = std::fs::read_to_string(&sidecar) else {
            return;
        };
        match serde_json::from_str::<LidActions>(&text) {
            Ok(saved) => match set_lid_actions(&saved.ac, &saved.dc) {
                Ok(()) => tracing::info!(ac = %saved.ac, dc = %saved.dc, "lid-close action restored"),
                Err(err) => tracing::warn!(error = %err, "could not restore the lid-close action"),
            },
            Err(err) => tracing::warn!(error = %err, "unreadable lid-restore sidecar; discarding"),
        }
        let _ = std::fs::remove_file(&sidecar);
    }

    fn read_lid_actions() -> Result<(String, String), String> {
        let out = powercfg(&["/query", "SCHEME_CURRENT", "SUB_BUTTONS", "LIDACTION"])?;
        let mut ac = None;
        let mut dc = None;
        for line in out.lines() {
            let l = line.trim();
            if let Some(v) = l.strip_prefix("Current AC Power Setting Index:") {
                ac = Some(v.trim().to_string());
            } else if let Some(v) = l.strip_prefix("Current DC Power Setting Index:") {
                dc = Some(v.trim().to_string());
            }
        }
        match (ac, dc) {
            (Some(ac), Some(dc)) => Ok((ac, dc)),
            _ => Err("could not read the current lid-close action".to_string()),
        }
    }

    fn set_lid_actions(ac: &str, dc: &str) -> Result<(), String> {
        powercfg(&["/setacvalueindex", "SCHEME_CURRENT", "SUB_BUTTONS", "LIDACTION", ac])?;
        powercfg(&["/setdcvalueindex", "SCHEME_CURRENT", "SUB_BUTTONS", "LIDACTION", dc])?;
        powercfg(&["/setactive", "SCHEME_CURRENT"])?;
        Ok(())
    }

    /// Run powercfg without a console window and return stdout.
    fn powercfg(args: &[&str]) -> Result<String, String> {
        use std::os::windows::process::CommandExt as _;
        let mut child = Command::new("powercfg")
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .creation_flags(CREATE_NO_WINDOW)
            .spawn()
            .map_err(|e| e.to_string())?;
        let start = Instant::now();
        loop {
            match child.try_wait().map_err(|e| e.to_string())? {
                Some(_) => break,
                None if start.elapsed() >= POWERCFG_TIMEOUT => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err("powercfg timed out".to_string());
                }
                None => std::thread::sleep(Duration::from_millis(25)),
            }
        }
        let out = child.wait_with_output().map_err(|e| e.to_string())?;
        if !out.status.success() {
            let err = String::from_utf8_lossy(&out.stderr);
            return Err(format!("powercfg failed: {}", err.trim()));
        }
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use zeron_proto::SessionStatus;

    fn at(base: Instant, secs: u64) -> Instant {
        base + Duration::from_secs(secs)
    }

    #[test]
    fn veto_follows_setting_and_activity_with_grace() {
        let t0 = Instant::now();
        let mut p = KeepAwakePolicy::new();
        // Activity without the setting does nothing.
        assert_eq!(p.set_running(true, t0), None);
        assert!(!p.held());
        // Enabling mid-run holds immediately.
        assert_eq!(p.set_enabled(true, at(t0, 1)), Some(Transition::Hold));
        assert_eq!(p.next_deadline(), None);
        // Run ends: still held, deadline = end + grace.
        assert_eq!(p.set_running(false, at(t0, 10)), None);
        assert!(p.held());
        assert_eq!(p.next_deadline(), Some(at(t0, 10) + GRACE_PERIOD));
        // Inside the grace period nothing changes.
        assert_eq!(p.tick(at(t0, 100)), None);
        // A new run inside the grace period cancels the pending release.
        assert_eq!(p.set_running(true, at(t0, 120)), None);
        assert_eq!(p.next_deadline(), None);
        assert_eq!(p.set_running(false, at(t0, 130)), None);
        assert_eq!(p.tick(at(t0, 130) + GRACE_PERIOD - Duration::from_secs(1)), None);
        assert_eq!(
            p.tick(at(t0, 130) + GRACE_PERIOD),
            Some(Transition::Release)
        );
        assert!(!p.held());
        assert_eq!(p.next_deadline(), None);
    }

    #[test]
    fn disabling_releases_immediately_and_idle_enable_holds_nothing() {
        let t0 = Instant::now();
        let mut p = KeepAwakePolicy::new();
        assert_eq!(p.set_enabled(true, t0), None);
        assert_eq!(p.set_running(true, t0), Some(Transition::Hold));
        assert_eq!(p.set_enabled(false, at(t0, 5)), Some(Transition::Release));
        // Re-enabling while the run is still going re-holds; a later stop and
        // grace expiry release once.
        assert_eq!(p.set_enabled(true, at(t0, 6)), Some(Transition::Hold));
        assert_eq!(p.set_running(false, at(t0, 7)), None);
        assert_eq!(p.tick(at(t0, 7) + GRACE_PERIOD), Some(Transition::Release));
        assert_eq!(p.tick(at(t0, 1000)), None);
    }

    fn session(device: &str, status: SessionStatus, updated_at: DateTime<Utc>) -> Session {
        Session {
            last_completed_turn: None,
            chat_id: "c".into(),
            device_id: device.into(),
            status,
            started_at: None,
            updated_at,
        }
    }

    #[test]
    fn only_live_local_working_sessions_count() {
        let now = Utc::now();
        let stale = now - chrono::Duration::seconds(60);
        let local = session("me", SessionStatus::Working, now);
        let remote = session("other", SessionStatus::Working, now);
        let dead = session("me", SessionStatus::Working, stale);
        let waiting = session("me", SessionStatus::AwaitingInput, now);
        assert!(any_local_run_active(&[local.clone()], Some("me"), now));
        assert!(!any_local_run_active(&[remote.clone()], Some("me"), now));
        assert!(!any_local_run_active(&[dead], Some("me"), now));
        assert!(!any_local_run_active(&[waiting], Some("me"), now));
        // Unknown local id: every session counts.
        assert!(any_local_run_active(&[remote], None, now));
        assert!(!any_local_run_active(&[], Some("me"), now));
    }
}
