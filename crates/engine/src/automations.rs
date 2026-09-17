//! Local schedules use normal session/doc execution with durable reservations.
//! Offline and overlapping slots are skipped, never replayed as a backlog.
use crate::{DocHost, EngineError, SessionsEngine, WorkspaceHost, new_id, now_ms};
use std::{
    collections::HashSet,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};
use zeron_proto::*;

#[derive(Clone)]
pub struct Automations {
    inner: Arc<Inner>,
}
struct Inner {
    inbox: crate::inbox::Inbox,
    reviews: tokio::sync::Mutex<std::collections::HashMap<String, (String, tokio::process::Child)>>,
    path: PathBuf,
    device_id: String,
    rows: Mutex<Vec<Automation>>,
    worker: Mutex<Option<tokio::task::JoinHandle<()>>>,
    sessions: SessionsEngine,
    docs: DocHost,
    workspace: WorkspaceHost,
}
fn err(message: impl Into<String>) -> EngineError {
    EngineError::Other(message.into())
}
fn plain_slot(from: i64, minutes: u32) -> i64 {
    from.saturating_add(i64::from(minutes) * 60_000)
}
/// Resolve a local wall-clock stamp, tolerating the DST gap and fold.
fn local_ms(naive: chrono::NaiveDateTime) -> Option<i64> {
    use chrono::TimeZone as _;
    let mapped = chrono::Local.from_local_datetime(&naive);
    mapped
        .single()
        .or_else(|| mapped.earliest())
        .or_else(|| mapped.latest())
        .map(|stamp| stamp.timestamp_millis())
}
/// The first local occurrence of `time_of_day` (minutes since midnight) on an
/// allowed `weekday` (0 = Monday) at or after `target`.
fn anchored(target: i64, time_of_day: u16, weekday: Option<u8>) -> i64 {
    anchored_days(target, time_of_day, weekday, None)
}
fn anchored_days(target: i64, time_of_day: u16, weekday: Option<u8>, days: Option<&[u8]>) -> i64 {
    use chrono::Datelike as _;
    let Some(stamp) = chrono::DateTime::from_timestamp_millis(target) else {
        return target;
    };
    let minutes = i64::from(time_of_day.min(1439));
    let time = chrono::NaiveTime::from_num_seconds_from_midnight_opt(minutes as u32 * 60, 0)
        .unwrap_or_default();
    let mut date = stamp.with_timezone(&chrono::Local).date_naive();
    // One extra day beyond the week: today's time may already have passed.
    for _ in 0..9 {
        let today = date.weekday().num_days_from_monday() as u8;
        let matches = days.map_or_else(|| weekday.is_none_or(|want| today == want), |days| days.contains(&today));
        if matches {
            let at = local_ms(date.and_time(time)).or_else(|| {
                // The wall-clock time itself does not exist (spring forward).
                local_ms(date.and_hms_opt(0, 0, 0)?).map(|midnight| midnight + minutes * 60_000)
            });
            if let Some(at) = at
                && at >= target
            {
                return at;
            }
        }
        let Some(next) = date.succ_opt() else {
            break;
        };
        date = next;
    }
    target
}
/// The next run strictly after `now`: the anchored local time when the spec has
/// one, else one interval from now.
fn next_slot(now: i64, spec: &AutomationSpec) -> i64 {
    match spec.time_of_day {
        Some(time) => anchored_days(now.saturating_add(1), time, spec.weekday, spec.weekdays.as_deref()),
        None => plain_slot(now, spec.interval_minutes),
    }
}
/// The slot following a run that started at `from`: one interval later, snapped
/// forward onto the anchor again, so "every 2 days at 08:00" stays at 08:00.
/// A sub-day interval only *starts* at its anchor and then strides plainly,
/// otherwise every schedule would collapse to once a day.
fn slot_after(from: i64, spec: &AutomationSpec) -> i64 {
    if spec.weekdays.is_some() { return next_slot(from, spec); }
    let plain = plain_slot(from, spec.interval_minutes);
    match spec.time_of_day {
        Some(time) if spec.interval_minutes % 1440 == 0 => anchored(plain, time, spec.weekday),
        _ => plain,
    }
}
/// Whether the optional anchor fields are in range.
fn anchor_is_valid(spec: &AutomationSpec) -> bool {
    spec.time_of_day.is_none_or(|time| time <= 1439)
        && spec.weekday.is_none_or(|day| day <= 6)
        && (spec.weekday.is_none() || spec.time_of_day.is_some())
        && spec.weekdays.as_ref().is_none_or(|days| !days.is_empty() && days.len() <= 7
            && days.iter().all(|day| *day < 7) && days.iter().collect::<std::collections::HashSet<_>>().len() == days.len()
            && spec.time_of_day.is_some() && spec.interval_minutes == 1440 && spec.weekday.is_none())
}
fn recover(rows: &mut [Automation], now: i64) {
    for row in rows {
        for run in &mut row.history {
            if run.status.is_active() {
                run.status = AutomationRunStatus::Interrupted;
                run.finished_at = Some(now);
                run.error =
                    Some("Engine stopped before completion. This run was not retried.".into());
            }
        }
        if row.spec.paused {
            row.next_run_at = None;
        } else if row.next_run_at.is_none_or(|time| time <= now) {
            row.next_run_at = Some(next_slot(now, &row.spec));
        }
    }
}
impl Automations {
    pub fn open(
        path: &Path,
        device_id: &str,
        sessions: SessionsEngine,
        docs: DocHost,
        workspace: WorkspaceHost,
        inbox: crate::inbox::Inbox,
    ) -> Result<Self, EngineError> {
        let mut rows: Vec<Automation> = match std::fs::read(path) {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .map_err(|e| err(format!("Cannot read automations: {e}")))?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => vec![],
            Err(e) => return Err(e.into()),
        };
        // A copied configuration must never execute on a different machine.
        for row in &mut rows {
            if row.device_id != device_id
                || !(1..=525600).contains(&row.spec.interval_minutes)
                || !anchor_is_valid(&row.spec)
            {
                row.spec.paused = true;
            }
        }
        recover(&mut rows, now_ms());
        let this = Self {
            inner: Arc::new(Inner {
                inbox,
                reviews: tokio::sync::Mutex::new(std::collections::HashMap::new()),
                path: path.into(),
                device_id: device_id.into(),
                rows: Mutex::new(rows),
                worker: Mutex::new(None),
                sessions,
                docs,
                workspace,
            }),
        };
        this.persist(&this.list())?;
        this.sync_inbox()?;
        // Backfill existing local runs once; identities survive deletion/history pruning.
        let chats = this.inner.workspace.read_chats()?;
        for job in this.list().into_iter().filter(|job| job.device_id == device_id) {
            for run in &job.history {
                if chats.iter().any(|chat| chat.id == run.chat_id && chat.automation.is_none()) {
                    this.inner.workspace.set_chat_automation(&run.chat_id, &zeron_proto::ChatAutomation { id: job.id.clone(), avatar: job.spec.avatar.clone() })?;
                }
            }
        }
        Ok(this)
    }
    fn sync_inbox(&self) -> Result<(), EngineError> {
        for job in self.list() {
            for run in &job.history {
                self.inner.inbox.automation(&job, run, true)?;
            }
        }
        Ok(())
    }
    /// Called before session recovery; scheduled runs never enter auto-resume.
    pub fn chat_ids(&self) -> HashSet<String> {
        self.list()
            .iter()
            .flat_map(|r| r.history.iter().map(|run| run.chat_id.clone()))
            .collect()
    }
    pub fn start(&self) {
        let mut slot = self.inner.worker.lock().unwrap();
        if slot.is_some() {
            return;
        }
        let weak = Arc::downgrade(&self.inner);
        *slot = Some(tokio::spawn(async move {
            let mut timer = tokio::time::interval(std::time::Duration::from_secs(5));
            timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                timer.tick().await;
                let Some(inner) = weak.upgrade() else {
                    break;
                };
                if let Err(error) = (Self { inner }).tick().await {
                    tracing::error!(%error, "automation scheduler tick failed");
                }
            }
        }));
    }
    fn persist(&self, rows: &[Automation]) -> Result<(), EngineError> {
        let bytes = serde_json::to_vec_pretty(rows).map_err(|e| err(e.to_string()))?;
        crate::agent_accounts::write_file_atomic(&self.inner.path, &bytes, true)
    }
    fn change<T>(
        &self,
        change: impl FnOnce(&mut Vec<Automation>) -> Result<T, EngineError>,
    ) -> Result<T, EngineError> {
        let mut guard = self.inner.rows.lock().unwrap();
        let mut rows = guard.clone();
        let result = change(&mut rows)?;
        self.persist(&rows)?;
        *guard = rows;
        Ok(result)
    }
    pub fn list(&self) -> Vec<Automation> {
        self.inner.rows.lock().unwrap().clone()
    }
    pub fn save(&self, params: SaveAutomationParams) -> Result<Automation, EngineError> {
        let mut spec = params.spec;
        spec.name = spec.name.trim().to_string();
        if spec.name.is_empty() || spec.request.prompt.trim().is_empty() {
            return Err(err("Name and task are required"));
        }
        if !(1..=525600).contains(&spec.interval_minutes) {
            return Err(err("Interval must be between 1 minute and 1 year"));
        }
        if !anchor_is_valid(&spec) {
            return Err(err("Time of day must be 00:00-23:59, and a weekday needs one"));
        }
        if spec.request.harness.is_none() {
            return Err(err("Choose an agent"));
        }
        if let Some(template) = &spec.result_manifest {
            if !template.contains("{{run_id}}") {
                return Err(err("Result manifest must contain {{run_id}}"));
            }
            crate::inbox::relative_path(&template.replace("{{run_id}}", "run"))?;
        }
        if let Some(url) = &spec.review_url {
            crate::inbox::local_review_url(&url.replace("{{run_id}}", "run"))?;
        }
        if let Some(command) = &spec.review_command {
            if spec.review_url.is_none()
                || command.is_empty()
                || command[0].trim().is_empty()
                || command.iter().any(|arg| arg.contains('\0'))
            {
                return Err(err(
                    "Review command requires a local review URL and a nonempty argv array",
                ));
            }
        }
        let cwd = Path::new(&spec.request.cwd);
        if !cwd.is_absolute() || !cwd.is_dir() {
            return Err(err(
                "Working folder must be an existing absolute directory on the execution device",
            ));
        }
        if spec.request.worktree.is_some() || spec.request.resume.is_some() {
            return Err(err("Automations start fresh chats in the selected folder"));
        }
        if let Some(space_id) = &spec.space_id {
            let space = self
                .inner
                .workspace
                .space(space_id)?
                .ok_or_else(|| err("Project no longer exists"))?;
            if space.device_id != self.inner.device_id {
                return Err(err("Project belongs to another execution device"));
            }
            if std::fs::canonicalize(&space.path)? != std::fs::canonicalize(&spec.request.cwd)? {
                return Err(err("Working folder must match the selected project folder"));
            }
        }
        self.change(|rows| {
            if let Some(id) = params.id {
                let row = rows
                    .iter_mut()
                    .find(|r| r.id == id)
                    .ok_or_else(|| err("Automation not found"))?;
                let mut previous = row.spec.clone();
                previous.paused = spec.paused;
                previous.name = spec.name.clone();
                if row.history.iter().any(|r| r.status.is_active()) && previous != spec {
                    return Err(err(
                        "Wait for the active run to finish before changing its configuration",
                    ));
                }
                if row.device_id != self.inner.device_id {
                    return Err(err("Automation belongs to another execution device"));
                }
                if row.spec.paused != spec.paused
                    || row.spec.interval_minutes != spec.interval_minutes
                    || row.spec.time_of_day != spec.time_of_day
                    || row.spec.weekday != spec.weekday
                    || row.spec.weekdays != spec.weekdays
                {
                    row.next_run_at = (!spec.paused).then(|| next_slot(now_ms(), &spec));
                }
                row.spec = spec;
                Ok(row.clone())
            } else {
                // Enabling is a separate explicit update, never a side effect of creation.
                spec.paused = true;
                let row = Automation {
                    id: new_id(),
                    device_id: self.inner.device_id.clone(),
                    spec,
                    next_run_at: None,
                    history: vec![],
                };
                rows.push(row.clone());
                Ok(row)
            }
        })
    }
    pub fn delete(&self, id: &str) -> Result<(), EngineError> {
        self.change(|rows| {
            let row = rows
                .iter()
                .find(|r| r.id == id)
                .ok_or_else(|| err("Automation not found"))?;
            if row.history.iter().any(|r| r.status.is_active()) {
                return Err(err("Wait for the current run to finish before deleting"));
            }
            rows.retain(|r| r.id != id);
            Ok(())
        })
    }
    async fn tick(&self) -> Result<(), EngineError> {
        for row in self.list() {
            if let Some(run) = row.history.iter().find(|r| r.status.is_active()) {
                // A run over its wall-clock budget is stopped, not left hanging.
                if let Some(limit) = row.spec.max_run_minutes.filter(|m| *m > 0)
                    && now_ms().saturating_sub(run.started_at) > i64::from(limit) * 60_000
                {
                    self.inner.sessions.interrupt(&run.chat_id).await.ok();
                    self.finish(
                        &row.id,
                        &run.id,
                        AutomationRunStatus::Interrupted,
                        Some(format!("Stopped after {limit} minutes")),
                    )?;
                    continue;
                }
                let session = self.inner.sessions.session_status(&run.chat_id);
                let status = match session.as_ref().map(|s| s.status) {
                    Some(SessionStatus::Working) => AutomationRunStatus::Running,
                    Some(SessionStatus::AwaitingInput) => AutomationRunStatus::AwaitingInput,
                    Some(SessionStatus::Idle)
                        if session
                            .as_ref()
                            .is_some_and(|s| s.last_completed_turn.is_some()) =>
                    {
                        AutomationRunStatus::Succeeded
                    }
                    Some(SessionStatus::Errored) => AutomationRunStatus::Failed,
                    _ => AutomationRunStatus::Interrupted,
                };
                if status != run.status {
                    self.finish(
                        &row.id,
                        &run.id,
                        status,
                        match status {
                            AutomationRunStatus::Failed => {
                                Some("Agent run failed. Open the run chat for details.".into())
                            }
                            AutomationRunStatus::Interrupted => {
                                Some("Run stopped before a completed turn.".into())
                            }
                            _ => None,
                        },
                    )?;
                }
                continue;
            }
            if row.spec.paused || row.next_run_at.is_none_or(|t| t > now_ms()) {
                continue;
            }
            self.start_run(&row, false).await?;
        }
        Ok(())
    }
    /// Reserve and dispatch one run of `row`. `on_demand` is a Test run: it
    /// ignores the schedule and the paused flag, but never the no-overlap rule.
    /// Returns whether a run was actually claimed.
    async fn start_run(&self, row: &Automation, on_demand: bool) -> Result<bool, EngineError> {
        let run = AutomationRun {
            id: new_id(),
            chat_id: new_id(),
            started_at: now_ms(),
            finished_at: None,
            status: AutomationRunStatus::Running,
            error: None,
        };
        // Persist before dispatch, so a crash cannot duplicate this run.
        let claimed = self.change(|rows| {
            let Some(current) = rows.iter_mut().find(|r| r.id == row.id) else {
                return Ok(false);
            };
            if current.spec != row.spec
                || (!on_demand && current.spec.paused)
                || current.history.iter().any(|r| r.status.is_active())
            {
                return Ok(false);
            }
            // A test run does not consume the scheduled slot.
            if !on_demand {
                current.next_run_at = Some(slot_after(run.started_at, &current.spec));
            }
            current.history.insert(0, run.clone());
            current.history.truncate(100);
            Ok(true)
        })?;
        if !claimed {
            return Ok(false);
        }
        if self.delivers_to_inbox(&row.spec)
            && let Err(error) = self.inner.inbox.automation(row, &run, false)
        {
            self.finish(
                &row.id,
                &run.id,
                AutomationRunStatus::Failed,
                Some(format!("Cannot record inbox item: {error}")),
            )?;
            return Ok(true);
        }
        if let Err(error) = self.launch(row, &run).await {
            self.finish(
                &row.id,
                &run.id,
                AutomationRunStatus::Failed,
                Some(error.to_string()),
            )?;
        }
        Ok(true)
    }
    /// Whether finished runs of this automation show up in the Inbox.
    fn delivers_to_inbox(&self, spec: &AutomationSpec) -> bool {
        spec.deliver.unwrap_or_default().inbox
    }
    /// A Test run started from the UI: one run immediately, whatever the
    /// schedule says and even while the automation is paused.
    pub async fn run_now(&self, id: &str) -> Result<Automation, EngineError> {
        let row = self
            .list()
            .into_iter()
            .find(|r| r.id == id)
            .ok_or_else(|| err("Automation not found"))?;
        if row.device_id != self.inner.device_id {
            return Err(err("Automation belongs to another execution device"));
        }
        if row.history.iter().any(|r| r.status.is_active()) {
            return Err(err("This automation is already running"));
        }
        if !self.start_run(&row, true).await? {
            return Err(err("This automation is already running"));
        }
        self.list()
            .into_iter()
            .find(|r| r.id == id)
            .ok_or_else(|| err("Automation not found"))
    }
    async fn launch(&self, row: &Automation, run: &AutomationRun) -> Result<(), EngineError> {
        let mut request = row.spec.request.clone();
        request.prompt = request.prompt.replace("{{run_id}}", &run.id);
        let harness = request.harness.ok_or_else(|| err("Agent is missing"))?;
        if !Path::new(&request.cwd).is_dir() {
            return Err(err("Working folder is unavailable"));
        }
        // Revalidate project ownership at execution time, not only at save time.
        if let Some(id) = &row.spec.space_id {
            let space = self
                .inner
                .workspace
                .space(id)?
                .ok_or_else(|| err("Project no longer exists"))?;
            if space.device_id != self.inner.device_id {
                return Err(err("Project moved to another device"));
            }
            if std::fs::canonicalize(&space.path)? != std::fs::canonicalize(&request.cwd)? {
                return Err(err("Working folder no longer matches the selected project"));
            }
        }
        self.inner.workspace.create_chat(
            &run.chat_id,
            row.spec.space_id.as_deref(),
            Some(&self.inner.device_id),
            Some(ChatConfig {
                harness,
                model: request.model.clone(),
                reasoning: request.reasoning,
                model_options: request.model_options.clone(),
                sandbox: request.sandbox,
            }),
            Some(request.cwd.clone()),
        )?;
        self.inner.workspace.set_chat_automation(&run.chat_id, &zeron_proto::ChatAutomation { id: row.id.clone(), avatar: row.spec.avatar.clone() })?;
        self.inner
            .workspace
            .rename_chat(&run.chat_id, &row.spec.name)?;
        self.inner
            .docs
            .dispatch_with_source_context(
                &self.inner.sessions,
                &run.chat_id,
                harness,
                request.clone(),
                Some(run.id.clone()),
            )
            .await?;
        Ok(())
    }
    fn finish(
        &self,
        id: &str,
        run_id: &str,
        status: AutomationRunStatus,
        error: Option<String>,
    ) -> Result<(), EngineError> {
        let updated = self.change(|rows| {
            if let Some(row) = rows.iter_mut().find(|r| r.id == id) {
                if let Some(run) = row.history.iter_mut().find(|r| r.id == run_id) {
                    run.status = status;
                    run.error = error;
                    if !status.is_active() {
                        run.finished_at = Some(now_ms());
                    }
                }
                if !status.is_active()
                    && !row.spec.paused
                    && row.next_run_at.is_some_and(|t| t <= now_ms())
                {
                    row.next_run_at = Some(next_slot(now_ms(), &row.spec));
                }
            }
            Ok(rows.iter().find(|r| r.id == id).cloned())
        })?;
        if let Some(job) = updated
            && self.delivers_to_inbox(&job.spec)
            && let Some(run) = job.history.iter().find(|r| r.id == run_id)
        {
            self.inner.inbox.automation(&job, run, false)?;
        }
        Ok(())
    }
    pub async fn open_review(&self, id: &str) -> Result<OpenInboxReviewResult, EngineError> {
        let item = self.inner.inbox.get(id)?;
        if item.source != InboxSource::Automation || item.device_id != self.inner.device_id {
            return Err(err("This item has no local automation review"));
        }
        let job = self
            .list()
            .into_iter()
            .find(|r| r.id == item.source_id)
            .ok_or_else(|| err("Automation no longer exists"))?;
        let run_id = item
            .id
            .strip_prefix("automation:")
            .ok_or_else(|| err("Invalid automation item"))?;
        uuid::Uuid::parse_str(run_id).map_err(|_| err("Invalid run ID"))?;
        let fingerprint = crate::inbox::review_config_hash(&job.spec);
        if item.review_config_hash.as_deref() != Some(&fingerprint) {
            return Err(err(
                "Review configuration has changed since this run. Open the run chat or a newer result.",
            ));
        }
        let url = crate::inbox::local_review_url(
            item.review_url
                .as_deref()
                .ok_or_else(|| err("No review URL configured"))?,
        )?;
        let client = reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(std::time::Duration::from_millis(450))
            .build()
            .map_err(|e| err(e.to_string()))?;
        // Serialize probe and spawn across clicks, so one job owns at most one server.
        let mut children = self.inner.reviews.lock().await;
        if children
            .get(&job.id)
            .is_some_and(|(old, _)| old != &fingerprint)
        {
            if let Some((_, mut child)) = children.remove(&job.id) {
                let _ = child.kill().await;
            }
        }
        if review_ready(&client, &url).await {
            return Ok(OpenInboxReviewResult { url: url.into() });
        }
        let argv = job
            .spec
            .review_command
            .as_ref()
            .filter(|args| !args.is_empty())
            .ok_or_else(|| {
                err("Review is unavailable. Start its server or configure a review command.")
            })?;
        if let Some((_, child)) = children.get_mut(&job.id) {
            if child.try_wait()?.is_some() {
                children.remove(&job.id);
            }
        }
        let mut spawned = false;
        if !children.contains_key(&job.id) {
            let mut command = tokio::process::Command::new(&argv[0]);
            command
                .args(&argv[1..])
                .current_dir(&job.spec.request.cwd)
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .kill_on_drop(true);
            #[cfg(windows)]
            command.creation_flags(0x08000000); // CREATE_NO_WINDOW
            children.insert(
                job.id.clone(),
                (
                    fingerprint.clone(),
                    command
                        .spawn()
                        .map_err(|e| err(format!("Review could not start: {e}")))?,
                ),
            );
            spawned = true;
        }
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
        while tokio::time::Instant::now() < deadline {
            if review_ready(&client, &url).await {
                return Ok(OpenInboxReviewResult { url: url.into() });
            }
            if let Some((_, child)) = children.get_mut(&job.id) {
                if let Some(status) = child.try_wait()? {
                    children.remove(&job.id);
                    return Err(err(format!(
                        "Review server exited ({status}) before becoming available"
                    )));
                }
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
        if spawned {
            if let Some((_, mut child)) = children.remove(&job.id) {
                let _ = child.kill().await;
            }
        }
        Err(err(
            "Review server did not become available within 5 seconds",
        ))
    }
    pub async fn shutdown(&self) {
        let worker = self.inner.worker.lock().unwrap().take();
        if let Some(worker) = worker {
            worker.abort();
            let _ = worker.await;
        }
        let result = self.change(|rows| {
            recover(rows, now_ms());
            Ok(())
        });
        let result = result.and_then(|_| self.sync_inbox());
        if let Err(error) = result {
            tracing::error!(%error, "automation shutdown persistence failed");
        }
        let mut children = self.inner.reviews.lock().await;
        for (_, (_, mut child)) in children.drain() {
            let _ = child.kill().await;
        }
    }
}

async fn review_ready(client: &reqwest::Client, url: &reqwest::Url) -> bool {
    client
        .get(url.clone())
        .send()
        .await
        .is_ok_and(|response| response.status().is_success())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn row() -> Automation {
        serde_json::from_value(serde_json::json!({
            "id":"job", "deviceId":"local", "nextRunAt":1, "history":[],
            "spec": { "name":"Example", "spaceId":null, "intervalMinutes":60, "paused":false,
                "request": { "prompt":"Do work", "harness":"claude-code", "model":null,
                    "reasoning":null, "cwd":"/work", "sandbox":"workspace-write", "resume":null }
            }
        }))
        .unwrap()
    }
    /// A local wall clock stamp, so the anchor tests hold in any timezone.
    fn at(y: i32, m: u32, d: u32, hour: u32, minute: u32) -> i64 {
        local_ms(
            chrono::NaiveDate::from_ymd_opt(y, m, d)
                .unwrap()
                .and_hms_opt(hour, minute, 0)
                .unwrap(),
        )
        .unwrap()
    }
    fn anchored_spec(minutes: u32, time_of_day: Option<u16>, weekday: Option<u8>) -> AutomationSpec {
        let mut spec = row().spec;
        spec.interval_minutes = minutes;
        spec.time_of_day = time_of_day;
        spec.weekday = weekday;
        spec
    }
    #[test]
    fn selected_calendar_days_skip_weekends_and_keep_local_time() {
        let mut spec = anchored_spec(1440, Some(540), None);
        spec.weekdays = Some(vec![0, 1, 2, 3, 4]);
        assert!(anchor_is_valid(&spec));
        assert_eq!(next_slot(at(2026, 9, 18, 8, 0), &spec), at(2026, 9, 18, 9, 0));
        assert_eq!(slot_after(at(2026, 9, 18, 9, 0), &spec), at(2026, 9, 21, 9, 0));
        spec.weekdays = Some(vec![0, 2]);
        assert_eq!(next_slot(at(2026, 9, 16, 9, 0), &spec), at(2026, 9, 21, 9, 0));
        spec.weekdays = Some((0..7).collect());
        assert_eq!(slot_after(at(2026, 10, 24, 9, 0), &spec), at(2026, 10, 25, 9, 0));
        assert_eq!(slot_after(at(2026, 3, 28, 9, 0), &spec), at(2026, 3, 29, 9, 0));
        for days in [vec![], vec![7], vec![0, 0]] {
            spec.weekdays = Some(days);
            assert!(!anchor_is_valid(&spec));
        }
    }

    #[test]
    fn without_an_anchor_the_next_slot_is_one_interval_from_now() {
        let spec = anchored_spec(60, None, None);
        assert_eq!(next_slot(1_000, &spec), 3_601_000);
        assert_eq!(slot_after(1_000, &spec), 3_601_000);
        assert!(anchor_is_valid(&spec));
    }
    #[test]
    fn a_time_of_day_runs_at_the_next_local_occurrence() {
        // 2026-09-16 is a Wednesday.
        let spec = anchored_spec(1440, Some(8 * 60), None);
        // Before today's slot: today.
        assert_eq!(
            next_slot(at(2026, 9, 16, 6, 30), &spec),
            at(2026, 9, 16, 8, 0)
        );
        // At and after it: tomorrow.
        assert_eq!(
            next_slot(at(2026, 9, 16, 8, 0), &spec),
            at(2026, 9, 17, 8, 0)
        );
        assert_eq!(
            next_slot(at(2026, 9, 16, 23, 59), &spec),
            at(2026, 9, 17, 8, 0)
        );
    }
    #[test]
    fn every_two_days_at_eight_stays_at_eight() {
        let spec = anchored_spec(2 * 1440, Some(8 * 60), None);
        let first = next_slot(at(2026, 9, 16, 6, 30), &spec);
        assert_eq!(first, at(2026, 9, 16, 8, 0));
        let second = slot_after(first, &spec);
        assert_eq!(second, at(2026, 9, 18, 8, 0));
        assert_eq!(slot_after(second, &spec), at(2026, 9, 20, 8, 0));
    }
    #[test]
    fn a_weekly_anchor_picks_the_weekday_and_keeps_the_stride() {
        // Monday 09:00, asked on Wednesday.
        let spec = anchored_spec(7 * 1440, Some(9 * 60), Some(0));
        let first = next_slot(at(2026, 9, 16, 12, 0), &spec);
        assert_eq!(first, at(2026, 9, 21, 9, 0));
        assert_eq!(slot_after(first, &spec), at(2026, 9, 28, 9, 0));
        // Sunday (6) from the same Wednesday is four days out.
        let sunday = anchored_spec(7 * 1440, Some(9 * 60), Some(6));
        assert_eq!(
            next_slot(at(2026, 9, 16, 12, 0), &sunday),
            at(2026, 9, 20, 9, 0)
        );
    }
    #[test]
    fn an_hourly_anchor_only_sets_the_start_then_strides_plainly() {
        let spec = anchored_spec(6 * 60, Some(30), None);
        let first = next_slot(at(2026, 9, 16, 1, 0), &spec);
        assert_eq!(first, at(2026, 9, 17, 0, 30));
        assert_eq!(slot_after(first, &spec), at(2026, 9, 17, 6, 30));
        assert_eq!(
            slot_after(slot_after(first, &spec), &spec),
            at(2026, 9, 17, 12, 30)
        );
    }
    #[test]
    fn out_of_range_anchors_are_rejected() {
        assert!(!anchor_is_valid(&anchored_spec(1440, Some(1440), None)));
        assert!(anchor_is_valid(&anchored_spec(1440, Some(1439), None)));
        assert!(!anchor_is_valid(&anchored_spec(10080, Some(540), Some(7))));
        // A weekday without a time has nothing to run at.
        assert!(!anchor_is_valid(&anchored_spec(10080, None, Some(0))));
    }
    #[test]
    fn rows_saved_before_avatars_load_with_the_new_fields_absent() {
        // An automations.json written by an older build: none of the new keys.
        let mut value = serde_json::to_value(vec![row()]).unwrap();
        let spec = value[0]["spec"].as_object_mut().unwrap();
        for key in ["avatar", "role", "deliver", "maxRunMinutes"] {
            spec.remove(key);
        }
        let rows: Vec<Automation> = serde_json::from_value(value).unwrap();
        let spec = &rows[0].spec;
        assert_eq!(spec.avatar, None);
        assert_eq!(spec.role, None);
        assert_eq!(spec.deliver, None);
        assert_eq!(spec.max_run_minutes, None);
        // An absent delivery target still means inbox plus notification.
        assert_eq!(
            spec.deliver.unwrap_or_default(),
            Deliver {
                inbox: true,
                desktop_notification: true
            }
        );
        // And the new fields round-trip when they are set.
        let mut spec = rows[0].spec.clone();
        spec.avatar = Some(AgentAvatar {
            shape: "shield".into(),
            color: "#3b82f6".into(),
        });
        spec.role = Some("CEO".into());
        spec.max_run_minutes = Some(30);
        let wire = serde_json::to_string(&spec).unwrap();
        assert!(wire.contains(r#""maxRunMinutes":30"#));
        assert_eq!(serde_json::from_str::<AutomationSpec>(&wire).unwrap(), spec);
    }

    #[test]
    fn saved_automations_without_the_new_fields_keep_the_old_behaviour() {
        let mut value = serde_json::to_value(row().spec).unwrap();
        let object = value.as_object_mut().unwrap();
        object.remove("timeOfDay");
        object.remove("weekday");
        let spec: AutomationSpec = serde_json::from_value(value).unwrap();
        assert_eq!(spec.time_of_day, None);
        assert_eq!(spec.weekday, None);
        assert_eq!(next_slot(1_000, &spec), 3_601_000);
    }
    #[test]
    fn restart_skips_missed_slots_and_preserves_future_deadline() {
        let mut rows = vec![row()];
        recover(&mut rows, 1000);
        assert_eq!(rows[0].next_run_at, Some(3_601_000));
        recover(&mut rows, 2000);
        assert_eq!(rows[0].next_run_at, Some(3_601_000));
    }
    #[test]
    fn restart_marks_inflight_interrupted_without_retry() {
        let mut job = row();
        job.history.push(AutomationRun {
            id: "run".into(),
            chat_id: "chat".into(),
            started_at: 0,
            finished_at: None,
            status: AutomationRunStatus::AwaitingInput,
            error: None,
        });
        recover(std::slice::from_mut(&mut job), 1000);
        assert_eq!(job.history[0].status, AutomationRunStatus::Interrupted);
        assert_eq!(job.history[0].finished_at, Some(1000));
        assert!(job.history[0].error.is_some());
    }
    #[test]
    fn paused_jobs_have_no_deadline_and_waiting_jobs_block_overlap() {
        let mut job = row();
        job.spec.paused = true;
        recover(std::slice::from_mut(&mut job), 1000);
        assert_eq!(job.next_run_at, None);
        assert!(AutomationRunStatus::AwaitingInput.is_active());
        assert!(AutomationRunStatus::Running.is_active());
        assert!(!AutomationRunStatus::Succeeded.is_active());
    }
    #[test]
    fn missing_paused_flag_defaults_to_paused() {
        let mut value = serde_json::to_value(row().spec).unwrap();
        value.as_object_mut().unwrap().remove("paused");
        let spec: AutomationSpec = serde_json::from_value(value).unwrap();
        assert!(spec.paused);
    }
    #[tokio::test]
    async fn create_forces_pause_and_persists_without_dispatch() {
        let temp = tempfile::tempdir().unwrap();
        let core = crate::EngineCore::assemble(
            temp.path(),
            Arc::new(crate::HarnessRegistry::new()),
            HarnessId::ClaudeCode,
            None,
        )
        .unwrap();
        let mut spec = row().spec;
        spec.request.cwd = temp.path().to_string_lossy().into_owned();
        spec.paused = false;
        let created = core
            .automations
            .save(SaveAutomationParams { id: None, spec })
            .unwrap();
        assert!(created.spec.paused);
        assert_eq!(created.next_run_at, None);
        assert!(created.history.is_empty());
        assert!(!core.sessions.any_active());
        let path = core.automations.inner.path.clone();
        let stored: Vec<Automation> =
            serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
        assert_eq!(stored, core.automations.list());
        core.automations.shutdown().await;
        let reopened = Automations::open(
            &core.automations.inner.path,
            &core.device_id,
            core.sessions.clone(),
            core.doc_host.clone(),
            core.workspace.clone(),
            core.inbox.clone(),
        )
        .unwrap();
        assert_eq!(reopened.list(), stored);
        core.shutdown().await;
    }
    #[tokio::test]
    async fn invalid_update_leaves_persisted_schedule_unchanged() {
        let temp = tempfile::tempdir().unwrap();
        let core = crate::EngineCore::assemble(
            temp.path(),
            Arc::new(crate::HarnessRegistry::new()),
            HarnessId::ClaudeCode,
            None,
        )
        .unwrap();
        let mut spec = row().spec;
        spec.request.cwd = temp.path().to_string_lossy().into_owned();
        let created = core
            .automations
            .save(SaveAutomationParams { id: None, spec })
            .unwrap();
        let before = std::fs::read(&core.automations.inner.path).unwrap();
        let mut invalid = created.spec.clone();
        invalid.interval_minutes = 0;
        assert!(
            core.automations
                .save(SaveAutomationParams {
                    id: Some(created.id),
                    spec: invalid
                })
                .is_err()
        );
        assert_eq!(std::fs::read(&core.automations.inner.path).unwrap(), before);
        assert!(!core.sessions.any_active());
        core.shutdown().await;
    }
    struct ControlledHarness {
        release: Arc<tokio::sync::Notify>,
    }
    #[async_trait::async_trait]
    impl zeron_harness::Harness for ControlledHarness {
        fn id(&self) -> HarnessId {
            HarnessId::Mock
        }
        fn display_name(&self) -> &str {
            "Automation test"
        }
        fn supports_steering(&self) -> bool {
            false
        }
        fn steering_mode(&self) -> SteeringMode {
            SteeringMode::TurnBoundary
        }
        fn reasoning_levels(&self) -> &[ReasoningLevel] {
            &[]
        }
        async fn models(&self) -> Result<Vec<Model>, zeron_harness::HarnessError> {
            Ok(vec![])
        }
        async fn run(
            &self,
            request: RunRequest,
            _controls: zeron_harness::RunControls,
        ) -> Result<
            futures::stream::BoxStream<'static, Result<AgentEvent, zeron_harness::HarnessError>>,
            zeron_harness::HarnessError,
        > {
            use futures::StreamExt;
            let release = self.release.clone();
            let started = AgentEvent::SessionStarted {
                harness: HarnessId::Mock,
                model: "test".into(),
                tools: vec![],
                cwd: request.cwd,
                session_id: "automation-test".into(),
                assistant_message_id: new_id(),
            };
            Ok(futures::stream::iter([Ok(started)])
                .chain(futures::stream::once(async move {
                    release.notified().await;
                    Ok(AgentEvent::Done {
                        status: DoneStatus::Completed,
                        result: Some("Completed".into()),
                        error: None,
                        session_id: None,
                    })
                }))
                .boxed())
        }
    }
    #[tokio::test]
    async fn scheduler_runs_through_sessions_and_prevents_overlap() {
        let temp = tempfile::tempdir().unwrap();
        let registry = Arc::new(crate::HarnessRegistry::new());
        let release = Arc::new(tokio::sync::Notify::new());
        registry.register(Arc::new(ControlledHarness {
            release: release.clone(),
        }));
        let core =
            crate::EngineCore::assemble(temp.path(), registry, HarnessId::Mock, None).unwrap();
        // Drive the production tick deterministically, without a wall-clock timer.
        core.automations.shutdown().await;
        let mut spec = row().spec;
        spec.request.cwd = temp.path().to_string_lossy().into_owned();
        spec.request.harness = Some(HarnessId::Mock);
        spec.request.prompt = "Work for {{run_id}}".into();
        let created = core
            .automations
            .save(SaveAutomationParams { id: None, spec })
            .unwrap();
        let mut enabled = created.spec;
        enabled.paused = false;
        core.automations
            .save(SaveAutomationParams {
                id: Some(created.id),
                spec: enabled,
            })
            .unwrap();
        core.automations
            .change(|rows| {
                rows[0].next_run_at = Some(0);
                Ok(())
            })
            .unwrap();
        core.automations.tick().await.unwrap();
        let chat_id = core.automations.list()[0].history[0].chat_id.clone();
        assert!(core.workspace.chat(&chat_id).unwrap().is_some());
        core.automations
            .change(|rows| {
                rows[0].next_run_at = Some(0);
                Ok(())
            })
            .unwrap();
        core.automations.tick().await.unwrap();
        assert_eq!(core.automations.list()[0].history.len(), 1);
        assert!(core.automations.list()[0].history[0].status.is_active());
        release.notify_one();
        tokio::time::timeout(std::time::Duration::from_secs(10), async {
            loop {
                if core
                    .sessions
                    .session_status(&chat_id)
                    .is_some_and(|s| s.last_completed_turn.is_some())
                {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        core.automations.tick().await.unwrap();
        let jobs = core.automations.list();
        assert_eq!(jobs[0].history.len(), 1);
        assert_eq!(jobs[0].history[0].status, AutomationRunStatus::Succeeded);
        let items = core.inbox.list();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].status, InboxStatus::Succeeded);
        assert_eq!(items[0].chat_id, chat_id);
        assert_eq!(items[0].summary.as_deref(), Some("Completed"));
        assert_eq!(
            core.sessions.last_request(&chat_id).unwrap().prompt,
            format!("Work for {}", jobs[0].history[0].id)
        );
        assert!(jobs[0].next_run_at.unwrap() > now_ms());
        core.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_test_run_ignores_pause_and_a_run_budget_stops_a_long_run() {
        let temp = tempfile::tempdir().unwrap();
        let registry = Arc::new(crate::HarnessRegistry::new());
        let release = Arc::new(tokio::sync::Notify::new());
        registry.register(Arc::new(ControlledHarness {
            release: release.clone(),
        }));
        let core =
            crate::EngineCore::assemble(temp.path(), registry, HarnessId::Mock, None).unwrap();
        core.automations.shutdown().await;
        let mut spec = row().spec;
        spec.request.cwd = temp.path().to_string_lossy().into_owned();
        spec.request.harness = Some(HarnessId::Mock);
        spec.max_run_minutes = Some(1);
        let created = core
            .automations
            .save(SaveAutomationParams { id: None, spec })
            .unwrap();
        // Created paused, with no deadline - and a Test run starts anyway.
        assert!(created.spec.paused);
        assert_eq!(created.next_run_at, None);
        let started = core.automations.run_now(&created.id).await.unwrap();
        assert_eq!(started.history.len(), 1);
        let chat = core.workspace.chat(&started.history[0].chat_id).unwrap().unwrap();
        assert_eq!(chat.automation.as_ref().map(|identity| identity.id.as_str()), Some(created.id.as_str()));
        assert!(started.history[0].status.is_active());
        // A test run does not invent a schedule for a paused automation.
        assert_eq!(started.next_run_at, None);
        // No overlap, not even on demand.
        assert!(core.automations.run_now(&created.id).await.is_err());
        // Over its budget, the run is interrupted with a plain reason.
        core.automations
            .change(|rows| {
                rows[0].history[0].started_at = now_ms() - 5 * 60_000;
                Ok(())
            })
            .unwrap();
        core.automations.tick().await.unwrap();
        let run = core.automations.list()[0].history[0].clone();
        assert_eq!(run.status, AutomationRunStatus::Interrupted);
        assert_eq!(run.error.as_deref(), Some("Stopped after 1 minutes"));
        assert!(run.finished_at.is_some());
        core.automations.delete(&created.id).unwrap();
        assert_eq!(core.workspace.chat(&run.chat_id).unwrap().unwrap().automation.unwrap().id, created.id);
        release.notify_waiters();
        core.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn an_automation_that_delivers_nowhere_records_no_inbox_item() {
        let temp = tempfile::tempdir().unwrap();
        let registry = Arc::new(crate::HarnessRegistry::new());
        let release = Arc::new(tokio::sync::Notify::new());
        registry.register(Arc::new(ControlledHarness {
            release: release.clone(),
        }));
        let core =
            crate::EngineCore::assemble(temp.path(), registry, HarnessId::Mock, None).unwrap();
        core.automations.shutdown().await;
        let mut spec = row().spec;
        spec.request.cwd = temp.path().to_string_lossy().into_owned();
        spec.request.harness = Some(HarnessId::Mock);
        spec.deliver = Some(Deliver {
            inbox: false,
            desktop_notification: false,
        });
        let created = core
            .automations
            .save(SaveAutomationParams { id: None, spec })
            .unwrap();
        core.automations.run_now(&created.id).await.unwrap();
        assert!(core.inbox.list().is_empty());
        // The run itself is still in the automation's own history.
        assert_eq!(core.automations.list()[0].history.len(), 1);
        release.notify_waiters();
        core.shutdown().await;
    }
    #[tokio::test]
    async fn project_folder_mismatch_is_rejected_at_save_and_launch() {
        let temp = tempfile::tempdir().unwrap();
        let project = temp.path().join("project");
        std::fs::create_dir(&project).unwrap();
        let core = crate::EngineCore::assemble(
            temp.path(),
            Arc::new(crate::HarnessRegistry::new()),
            HarnessId::Mock,
            None,
        )
        .unwrap();
        core.automations.shutdown().await;
        core.workspace
            .create_space(
                "project",
                &core.device_id,
                &project.to_string_lossy(),
                None,
                false,
            )
            .unwrap();
        let mut spec = row().spec;
        spec.space_id = Some("project".into());
        spec.request.cwd = temp.path().to_string_lossy().into_owned();
        assert!(
            core.automations
                .save(SaveAutomationParams {
                    id: None,
                    spec: spec.clone()
                })
                .is_err()
        );
        spec.request.cwd = project.join(".").to_string_lossy().into_owned();
        let mut saved = core
            .automations
            .save(SaveAutomationParams { id: None, spec })
            .unwrap();
        saved.spec.request.cwd = temp.path().to_string_lossy().into_owned();
        let run = AutomationRun {
            id: new_id(),
            chat_id: new_id(),
            started_at: now_ms(),
            finished_at: None,
            status: AutomationRunStatus::Running,
            error: None,
        };
        assert!(core.automations.launch(&saved, &run).await.is_err());
        assert!(core.workspace.chat(&run.chat_id).unwrap().is_none());
        core.shutdown().await;
    }
    /// Child-only local HTTP fixture, launched by the regression test below.
    #[test]
    #[ignore = "spawned only by review_start_is_explicit_singleflight_and_stopped_on_shutdown"]
    fn review_server_fixture() {
        use std::io::{Read, Write};
        let port: u16 = std::fs::read_to_string("review-port.txt")
            .unwrap()
            .parse()
            .unwrap();
        let listener = std::net::TcpListener::bind(("127.0.0.1", port)).unwrap();
        listener.set_nonblocking(true).unwrap();
        let mut marker = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open("review-starts.txt")
            .unwrap();
        writeln!(marker, "started").unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
        while std::time::Instant::now() < deadline {
            match listener.accept() {
                Ok((mut stream, _)) => {
                    stream
                        .set_read_timeout(Some(std::time::Duration::from_secs(1)))
                        .unwrap();
                    let mut buffer = [0; 4096];
                    let _ = stream.read(&mut buffer);
                    let _ = stream.write_all(
                        b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nOK",
                    );
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(std::time::Duration::from_millis(10))
                }
                Err(e) => panic!("{e}"),
            }
        }
    }
    #[tokio::test]
    async fn review_start_is_explicit_singleflight_and_stopped_on_shutdown() {
        let temp = tempfile::tempdir().unwrap();
        let reserve = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = reserve.local_addr().unwrap().port();
        drop(reserve);
        std::fs::write(temp.path().join("review-port.txt"), port.to_string()).unwrap();
        let core = crate::EngineCore::assemble(
            temp.path(),
            Arc::new(crate::HarnessRegistry::new()),
            HarnessId::Mock,
            None,
        )
        .unwrap();
        core.automations.shutdown().await;
        let mut spec = row().spec;
        spec.request.cwd = temp.path().to_string_lossy().into_owned();
        spec.review_url = Some(format!("http://127.0.0.1:{port}/?run_id={{{{run_id}}}}"));
        spec.review_command = Some(vec![
            std::env::current_exe()
                .unwrap()
                .to_string_lossy()
                .into_owned(),
            "--exact".into(),
            "automations::tests::review_server_fixture".into(),
            "--ignored".into(),
            "--nocapture".into(),
        ]);
        let job = core
            .automations
            .save(SaveAutomationParams { id: None, spec })
            .unwrap();
        let run = AutomationRun {
            id: new_id(),
            chat_id: new_id(),
            started_at: now_ms(),
            finished_at: Some(now_ms()),
            status: AutomationRunStatus::Succeeded,
            error: None,
        };
        core.inbox.automation(&job, &run, false).unwrap();
        assert!(!temp.path().join("review-starts.txt").exists());
        let id = format!("automation:{}", run.id);
        let (first, second) = tokio::join!(
            core.automations.open_review(&id),
            core.automations.open_review(&id)
        );
        assert_eq!(first.unwrap().url, second.unwrap().url);
        assert_eq!(
            std::fs::read_to_string(temp.path().join("review-starts.txt"))
                .unwrap()
                .lines()
                .count(),
            1
        );
        assert_eq!(core.automations.inner.reviews.lock().await.len(), 1);
        let mut changed = job.spec;
        changed.review_command = Some(vec!["another-program".into()]);
        core.automations
            .save(SaveAutomationParams {
                id: Some(job.id),
                spec: changed,
            })
            .unwrap();
        let error = core
            .automations
            .open_review(&id)
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("changed since this run"));
        core.shutdown().await;
        assert!(core.automations.inner.reviews.lock().await.is_empty());
        assert!(std::net::TcpStream::connect(("127.0.0.1", port)).is_err());
    }
}
