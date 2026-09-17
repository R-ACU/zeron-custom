//! Device-owned recurring work. Timestamps use epoch milliseconds.
use crate::RunRequest;
use serde::{Deserialize, Serialize};
fn paused_default() -> bool {
    true
}
fn bool_true() -> bool {
    true
}
/// The agent's face: one of the built-in shapes plus a palette colour
/// (`#rrggbb`). The UI owns the catalog; the engine only carries it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentAvatar {
    pub shape: String,
    pub color: String,
}
/// Where a finished run reports to. Absent means both, as before.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Deliver {
    #[serde(default = "bool_true")]
    pub inbox: bool,
    #[serde(default = "bool_true")]
    pub desktop_notification: bool,
}
impl Default for Deliver {
    fn default() -> Self {
        Self {
            inbox: true,
            desktop_notification: true,
        }
    }
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AutomationSpec {
    pub name: String,
    pub space_id: Option<String>,
    pub request: RunRequest,
    pub interval_minutes: u32,
    /// Minutes since local midnight the run is anchored to (0..=1439). Set for
    /// day-scale schedules; absent means "one interval from now".
    #[serde(default)]
    pub time_of_day: Option<u16>,
    /// Weekday the run is anchored to, 0 = Monday. Only for weekly schedules.
    #[serde(default)]
    pub weekday: Option<u8>,
    /// Selected local calendar days, Monday=0. None retains legacy intervals.
    #[serde(default)]
    pub weekdays: Option<Vec<u8>>,
    #[serde(default = "paused_default")]
    pub paused: bool,
    /// The agent's face. Absent on automations saved before avatars existed.
    #[serde(default)]
    pub avatar: Option<AgentAvatar>,
    /// The agent's role, the way a teammate introduces themselves ("CEO").
    #[serde(default)]
    pub role: Option<String>,
    /// Where a finished run reports to; absent behaves like both.
    #[serde(default)]
    pub deliver: Option<Deliver>,
    /// Wall-clock budget per run. Exceeding it interrupts the run. Absent means
    /// no limit, which is how every automation saved before this behaved.
    #[serde(default)]
    pub max_run_minutes: Option<u32>,
    /// Project-relative JSON path, containing {{run_id}}.
    #[serde(default)]
    pub result_manifest: Option<String>,
    /// Local review endpoint, optionally containing {{run_id}}.
    #[serde(default)]
    pub review_url: Option<String>,
    /// Explicitly configured argv; executed only by an OpenInboxReview request.
    #[serde(default)]
    pub review_command: Option<Vec<String>>,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Automation {
    pub id: String,
    pub device_id: String,
    pub spec: AutomationSpec,
    pub next_run_at: Option<i64>,
    pub history: Vec<AutomationRun>,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AutomationRun {
    pub id: String,
    pub chat_id: String,
    pub started_at: i64,
    pub finished_at: Option<i64>,
    pub status: AutomationRunStatus,
    pub error: Option<String>,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum AutomationRunStatus {
    Running,
    AwaitingInput,
    Succeeded,
    Failed,
    Interrupted,
}
impl AutomationRunStatus {
    pub fn is_active(self) -> bool {
        matches!(self, Self::Running | Self::AwaitingInput)
    }
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SaveAutomationParams {
    pub id: Option<String>,
    pub spec: AutomationSpec,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeleteAutomationParams {
    pub id: String,
}
/// A "Test run": one run right now, ignoring the schedule and the paused flag.
/// The no-overlap rule still applies.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunAutomationNowParams {
    pub id: String,
}
