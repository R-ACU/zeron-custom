//! Generic durable inbox. Entries reference chats/files; no draft files are copied.
use crate::{EngineError, now_ms};
use std::{
    path::{Component, Path, PathBuf},
    sync::{Arc, Mutex},
};
use zeron_proto::*;
#[derive(Clone)]
pub struct Inbox {
    inner: Arc<Inner>,
}
struct Inner {
    path: PathBuf,
    device_id: String,
    rows: Mutex<Vec<InboxItem>>,
}
fn err(text: impl Into<String>) -> EngineError {
    EngineError::Other(text.into())
}
fn short(text: &str) -> String {
    text.chars().take(2000).collect()
}
pub(crate) fn review_config_hash(spec: &AutomationSpec) -> String {
    use sha2::{Digest, Sha256};
    let bytes = serde_json::to_vec(&(&spec.request.cwd, &spec.review_url, &spec.review_command))
        .unwrap_or_default();
    format!("{:x}", Sha256::digest(bytes))
}
pub(crate) fn relative_path(path: &str) -> Result<(), EngineError> {
    if path.is_empty()
        || !Path::new(path)
            .components()
            .all(|c| matches!(c, Component::Normal(_) | Component::CurDir))
    {
        return Err(err("Result paths must stay relative to the working folder"));
    }
    Ok(())
}
pub(crate) fn local_review_url(value: &str) -> Result<reqwest::Url, EngineError> {
    let url = reqwest::Url::parse(value).map_err(|_| err("Invalid review URL"))?;
    if !matches!(url.scheme(), "http" | "https")
        || !matches!(
            url.host_str(),
            Some("localhost" | "127.0.0.1" | "[::1]" | "::1")
        )
        || !url.username().is_empty()
        || url.password().is_some()
    {
        return Err(err(
            "Review URL must use localhost, 127.0.0.1 or ::1 over HTTP(S)",
        ));
    }
    Ok(url)
}
pub(crate) fn read_manifest(
    cwd: &str,
    template: &str,
    run_id: &str,
) -> Result<ResultManifest, EngineError> {
    if !template.contains("{{run_id}}") {
        return Err(err("Manifest path must contain {{run_id}}"));
    }
    let relative = template.replace("{{run_id}}", run_id);
    relative_path(&relative)?;
    let root = std::fs::canonicalize(cwd)?;
    let file = std::fs::canonicalize(root.join(relative))?;
    if !file.starts_with(&root) || !file.is_file() {
        return Err(err("Manifest is outside the working folder"));
    }
    // Limit the read itself, including races where a writer grows the file.
    use std::io::Read;
    let mut bytes = Vec::new();
    std::fs::File::open(file)?
        .take(65_537)
        .read_to_end(&mut bytes)?;
    if bytes.len() > 65_536 {
        return Err(err("Result manifest exceeds 64 KiB"));
    }
    let mut manifest: ResultManifest =
        serde_json::from_slice(&bytes).map_err(|e| err(format!("Invalid result manifest: {e}")))?;
    if manifest.run_id != run_id {
        return Err(err("Result manifest belongs to a different run"));
    }
    if manifest.links.len() > 128 {
        return Err(err("Result manifest contains too many links"));
    }
    manifest.summary = short(&manifest.summary);
    for link in &mut manifest.links {
        link.label = link.label.chars().take(160).collect();
        if link.target.len() > 4096 {
            return Err(err("Result link is too long"));
        }
        match link.kind {
            InboxLinkType::File => {
                relative_path(&link.target)?;
                let target = std::fs::canonicalize(root.join(&link.target))?;
                if !target.starts_with(&root) || !target.is_file() {
                    return Err(err("Result file is outside the working folder"));
                }
                link.target = target.to_string_lossy().into_owned();
            }
            InboxLinkType::Url => {
                let url =
                    reqwest::Url::parse(&link.target).map_err(|_| err("Invalid result URL"))?;
                if !matches!(url.scheme(), "http" | "https")
                    || !url.username().is_empty()
                    || url.password().is_some()
                {
                    return Err(err("Result links must use HTTP(S) without credentials"));
                }
            }
        }
    }
    Ok(manifest)
}
impl Inbox {
    pub fn open(path: &Path, device_id: &str) -> Result<Self, EngineError> {
        let mut rows: Vec<InboxItem> = match std::fs::read(path) {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .map_err(|e| err(format!("Cannot read inbox: {e}")))?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => vec![],
            Err(e) => return Err(e.into()),
        };
        // Pending callbacks no longer exist after restart. Never present a dead question as actionable.
        for item in &mut rows {
            if item.source == InboxSource::Session && item.status == InboxStatus::AwaitingInput {
                item.status = InboxStatus::Interrupted;
                item.error = Some("Session restarted before this request was answered. Open its chat for current status.".into());
                item.updated_at = now_ms();
                item.read = false;
                item.done = false;
            }
        }
        let this = Self {
            inner: Arc::new(Inner {
                path: path.into(),
                device_id: device_id.into(),
                rows: Mutex::new(rows),
            }),
        };
        this.persist(&this.list())?;
        Ok(this)
    }
    fn persist(&self, rows: &[InboxItem]) -> Result<(), EngineError> {
        let data = serde_json::to_vec_pretty(rows).map_err(|e| err(e.to_string()))?;
        crate::agent_accounts::write_file_atomic(&self.inner.path, &data, true)
    }
    fn change<T>(
        &self,
        f: impl FnOnce(&mut Vec<InboxItem>) -> Result<T, EngineError>,
    ) -> Result<T, EngineError> {
        let mut guard = self.inner.rows.lock().unwrap();
        let mut rows = guard.clone();
        let result = f(&mut rows)?;
        if rows != *guard {
            self.persist(&rows)?;
            *guard = rows;
        }
        Ok(result)
    }
    pub fn list(&self) -> Vec<InboxItem> {
        let mut rows = self.inner.rows.lock().unwrap().clone();
        rows.sort_by(|a, b| {
            b.updated_at
                .cmp(&a.updated_at)
                .then_with(|| a.id.cmp(&b.id))
        });
        rows
    }
    pub fn get(&self, id: &str) -> Result<InboxItem, EngineError> {
        self.inner
            .rows
            .lock()
            .unwrap()
            .iter()
            .find(|r| r.id == id)
            .cloned()
            .ok_or_else(|| err("Inbox item not found"))
    }
    pub fn update(&self, params: UpdateInboxParams) -> Result<InboxItem, EngineError> {
        self.change(|rows| {
            let row = rows
                .iter_mut()
                .find(|r| r.id == params.id)
                .ok_or_else(|| err("Inbox item not found"))?;
            if let Some(read) = params.read {
                row.read = read;
            }
            if let Some(done) = params.done {
                row.done = done;
                if done {
                    row.read = true;
                }
            }
            Ok(row.clone())
        })
    }
    /// Upsert with stable identity. Only a substantive event reopens an acknowledged item.
    fn upsert(&self, mut item: InboxItem) -> Result<(), EngineError> {
        self.change(|rows| {
            if let Some(old) = rows.iter_mut().find(|r| r.id == item.id) {
                let changed = old.status != item.status
                    || old.summary != item.summary
                    || old.error != item.error
                    || old.links != item.links;
                item.created_at = old.created_at;
                item.read = if changed { false } else { old.read };
                item.done = if changed { false } else { old.done };
                item.updated_at = if changed { now_ms() } else { old.updated_at };
                *old = item;
            } else {
                rows.push(item);
            }
            Ok(())
        })
    }
    pub(crate) fn automation(
        &self,
        job: &Automation,
        run: &AutomationRun,
        historical: bool,
    ) -> Result<(), EngineError> {
        let id = format!("automation:{}", run.id);
        let old = self.get(&id).ok();
        let status = match run.status {
            AutomationRunStatus::Running => InboxStatus::Running,
            AutomationRunStatus::AwaitingInput => InboxStatus::AwaitingInput,
            AutomationRunStatus::Succeeded => InboxStatus::Succeeded,
            AutomationRunStatus::Failed => InboxStatus::Failed,
            AutomationRunStatus::Interrupted => InboxStatus::Interrupted,
        };
        let terminal = !run.status.is_active();
        let summary = old
            .as_ref()
            .and_then(|r| {
                if terminal && matches!(r.status, InboxStatus::AwaitingInput | InboxStatus::Running)
                {
                    None
                } else {
                    r.summary.clone()
                }
            })
            .or_else(|| {
                terminal.then(|| match run.status {
                    AutomationRunStatus::Succeeded => {
                        "Run completed. Open its chat for details.".into()
                    }
                    AutomationRunStatus::Failed => "Run failed. Open its chat for details.".into(),
                    _ => "Run interrupted.".into(),
                })
            });
        let mut item = InboxItem {
            id,
            device_id: job.device_id.clone(),
            source: InboxSource::Automation,
            source_id: job.id.clone(),
            chat_id: run.chat_id.clone(),
            title: job.spec.name.clone(),
            summary,
            error: run
                .error
                .clone()
                .or_else(|| old.as_ref().and_then(|r| r.error.clone())),
            links: old.as_ref().map(|r| r.links.clone()).unwrap_or_default(),
            status,
            read: historical,
            done: false,
            created_at: run.started_at,
            updated_at: run.finished_at.unwrap_or(run.started_at),
            review_url: old
                .as_ref()
                .map(|r| r.review_url.clone())
                .unwrap_or_else(|| {
                    if historical {
                        None
                    } else {
                        job.spec
                            .review_url
                            .as_ref()
                            .map(|url| url.replace("{{run_id}}", &run.id))
                    }
                }),
            review_config_hash: old
                .as_ref()
                .map(|r| r.review_config_hash.clone())
                .unwrap_or_else(|| (!historical).then(|| review_config_hash(&job.spec))),
        };
        // Historical discovery does not read old artifacts or manufacture a fresh result.
        if !historical && run.status == AutomationRunStatus::Succeeded {
            if let Some(template) = &job.spec.result_manifest {
                match read_manifest(&job.spec.request.cwd, template, &run.id) {
                    Ok(manifest) => {
                        item.summary = Some(manifest.summary);
                        item.links = manifest.links;
                    }
                    Err(error) => {
                        item.error = Some(format!("Result manifest: {error}"));
                    }
                }
            }
        }
        self.upsert(item)
    }
    pub(crate) fn event(
        &self,
        chat_id: &str,
        event: &AgentEvent,
        sequence: u64,
    ) -> Result<(), EngineError> {
        let automation = self.list().into_iter().find(|r| {
            r.source == InboxSource::Automation
                && r.chat_id == chat_id
                && matches!(r.status, InboxStatus::Running | InboxStatus::AwaitingInput)
        });
        match event {
            AgentEvent::InputRequested {
                request_id,
                questions,
            } => {
                let summary = short(
                    &questions
                        .iter()
                        .map(|q| q.question.as_str())
                        .collect::<Vec<_>>()
                        .join("\n"),
                );
                let mut item = automation.unwrap_or_else(|| InboxItem {
                    id: format!("session:{chat_id}:{request_id}"),
                    device_id: self.inner.device_id.clone(),
                    source: InboxSource::Session,
                    source_id: request_id.clone(),
                    chat_id: chat_id.into(),
                    title: "Input required".into(),
                    summary: None,
                    error: None,
                    links: vec![],
                    status: InboxStatus::AwaitingInput,
                    read: false,
                    done: false,
                    created_at: now_ms(),
                    updated_at: now_ms(),
                    review_url: None,
                    review_config_hash: None,
                });
                item.status = InboxStatus::AwaitingInput;
                item.summary = Some(summary);
                item.error = None;
                self.upsert(item)
            }
            AgentEvent::InputResolved { request_id } => {
                let mut item = match automation {
                    Some(item) => item,
                    None => match self.get(&format!("session:{chat_id}:{request_id}")) {
                        Ok(item) => item,
                        Err(_) => return Ok(()),
                    },
                };
                item.status = if item.source == InboxSource::Automation {
                    InboxStatus::Running
                } else {
                    InboxStatus::Resolved
                };
                item.summary = None;
                self.upsert(item)
            }
            AgentEvent::Done {
                status,
                result,
                error,
                ..
            } => {
                let entries = self.list();
                let has_active = entries.iter().any(|r| {
                    r.chat_id == chat_id
                        && matches!(r.status, InboxStatus::Running | InboxStatus::AwaitingInput)
                });
                if !has_active && *status == DoneStatus::Errored {
                    self.upsert(InboxItem {
                        id: format!("session:{chat_id}:done:{sequence}"),
                        device_id: self.inner.device_id.clone(),
                        source: InboxSource::Session,
                        source_id: format!("done:{sequence}"),
                        chat_id: chat_id.into(),
                        title: "Run failed".into(),
                        summary: result.as_deref().map(short),
                        error: error.as_deref().map(short),
                        links: vec![],
                        status: InboxStatus::Failed,
                        read: false,
                        done: false,
                        created_at: now_ms(),
                        updated_at: now_ms(),
                        review_url: None,
                        review_config_hash: None,
                    })?;
                }
                for mut item in entries.into_iter().filter(|r| {
                    r.chat_id == chat_id
                        && matches!(r.status, InboxStatus::Running | InboxStatus::AwaitingInput)
                }) {
                    item.status = match status {
                        DoneStatus::Completed => InboxStatus::Succeeded,
                        DoneStatus::Errored => InboxStatus::Failed,
                        _ => InboxStatus::Interrupted,
                    };
                    item.summary = result.as_deref().map(short);
                    item.error = error.as_deref().map(short);
                    self.upsert(item)?;
                }
                Ok(())
            }
            _ => Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn job() -> Automation {
        serde_json::from_value(serde_json::json!({
            "id":"job", "deviceId":"local", "nextRunAt":null, "history":[],
            "spec":{"name":"Example", "spaceId":null, "intervalMinutes":60,"paused":true,
                "request":{"prompt":"Work {{run_id}}", "harness":"mock", "model":null,
                    "reasoning":null, "cwd":"/work", "sandbox":"workspace-write", "resume":null}}
        }))
        .unwrap()
    }
    fn run() -> AutomationRun {
        AutomationRun {
            id: "run-1".into(),
            chat_id: "chat".into(),
            started_at: 1,
            finished_at: None,
            status: AutomationRunStatus::Running,
            error: None,
        }
    }
    fn question() -> AgentEvent {
        AgentEvent::InputRequested {
            request_id: "req-1".into(),
            questions: vec![
                serde_json::from_value(
                    serde_json::json!({"id":"q","header":"Question","question":"Continue?",
                "options":["Yes","No"],"multiSelect":false}),
                )
                .unwrap(),
            ],
        }
    }
    #[test]
    fn automation_lifecycle_updates_one_entry_and_persists_flags() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("inbox.json");
        let inbox = Inbox::open(&path, "local").unwrap();
        let job = job();
        let mut run = run();
        inbox.automation(&job, &run, false).unwrap();
        inbox.event(&run.chat_id, &question(), 1).unwrap();
        assert_eq!(inbox.list().len(), 1);
        assert_eq!(inbox.list()[0].status, InboxStatus::AwaitingInput);
        run.status = AutomationRunStatus::Succeeded;
        run.finished_at = Some(2);
        inbox.automation(&job, &run, false).unwrap();
        let item = inbox.list().remove(0);
        assert_ne!(item.summary.as_deref(), Some("Continue?"));
        assert_eq!(item.status, InboxStatus::Succeeded);
        inbox
            .update(UpdateInboxParams {
                id: item.id.clone(),
                read: Some(true),
                done: Some(true),
            })
            .unwrap();
        inbox.automation(&job, &run, true).unwrap();
        let reopened = Inbox::open(&path, "local").unwrap();
        assert_eq!(reopened.list().len(), 1);
        assert!(reopened.get(&item.id).unwrap().done);
        assert!(reopened.get(&item.id).unwrap().read);
        // Closing an already completed idle session must not rewrite its successful result.
        reopened
            .event(
                &run.chat_id,
                &AgentEvent::Done {
                    status: DoneStatus::Interrupted,
                    result: None,
                    error: None,
                    session_id: None,
                },
                2,
            )
            .unwrap();
        assert_eq!(
            reopened.get(&item.id).unwrap().status,
            InboxStatus::Succeeded
        );
    }
    #[test]
    fn genuine_request_id_deduplicates_question_and_resolves_it() {
        let temp = tempfile::tempdir().unwrap();
        let inbox = Inbox::open(&temp.path().join("inbox.json"), "local").unwrap();
        inbox.event("chat", &question(), 1).unwrap();
        inbox.event("chat", &question(), 1).unwrap();
        assert_eq!(inbox.list().len(), 1);
        assert_eq!(inbox.list()[0].id, "session:chat:req-1");
        inbox
            .event(
                "chat",
                &AgentEvent::InputResolved {
                    request_id: "req-1".into(),
                },
                2,
            )
            .unwrap();
        assert_eq!(inbox.list()[0].status, InboxStatus::Resolved);
        assert_eq!(inbox.list()[0].chat_id, "chat");
    }
    #[test]
    fn pending_session_request_is_not_actionable_after_restart() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("inbox.json");
        let inbox = Inbox::open(&path, "local").unwrap();
        inbox.event("chat", &question(), 1).unwrap();
        let reopened = Inbox::open(&path, "local").unwrap();
        assert_eq!(reopened.list()[0].status, InboxStatus::Interrupted);
    }
    #[test]
    fn manifest_is_bound_to_run_and_normalizes_safe_file_links() {
        let temp = tempfile::tempdir().unwrap();
        let cwd = temp.path().to_string_lossy();
        std::fs::write(temp.path().join("draft.txt"), "PRIVATE FILE CONTENT").unwrap();
        let file = temp.path().join("run-1.json");
        let data = serde_json::json!({"runId":"run-1","summary":"Two references", "links":[
            {"label":"Draft","type":"file","target":"draft.txt"},
            {"label":"Review","type":"url","target":"http://127.0.0.1:8793/?run_id=run-1"}]});
        std::fs::write(&file, serde_json::to_vec(&data).unwrap()).unwrap();
        let manifest = read_manifest(&cwd, "{{run_id}}.json", "run-1").unwrap();
        assert!(Path::new(&manifest.links[0].target).is_absolute());
        assert!(
            !serde_json::to_string(&manifest)
                .unwrap()
                .contains("PRIVATE FILE CONTENT")
        );
        let mut wrong = data;
        wrong["runId"] = "older-run".into();
        std::fs::write(&file, serde_json::to_vec(&wrong).unwrap()).unwrap();
        assert!(read_manifest(&cwd, "{{run_id}}.json", "run-1").is_err());
        assert!(read_manifest(&cwd, "../{{run_id}}.json", "run-1").is_err());
        assert!(read_manifest(&cwd, "run-1.json", "run-1").is_err());
    }
    #[test]
    fn manifest_accepts_bulk_results_but_caps_links_and_rejects_unsafe_targets() {
        let temp = tempfile::tempdir().unwrap();
        let cwd = temp.path().to_string_lossy();
        let file = temp.path().join("run-1.json");
        let link = serde_json::json!({"label":"Result","type":"url","target":"https://example.test/result"});
        let data = |links| serde_json::json!({"runId":"run-1","summary":"Bulk", "links":links});
        std::fs::write(
            &file,
            serde_json::to_vec(&data(vec![link.clone(); 128])).unwrap(),
        )
        .unwrap();
        assert_eq!(
            read_manifest(&cwd, "{{run_id}}.json", "run-1")
                .unwrap()
                .links
                .len(),
            128
        );
        std::fs::write(&file, serde_json::to_vec(&data(vec![link; 129])).unwrap()).unwrap();
        assert!(read_manifest(&cwd, "{{run_id}}.json", "run-1").is_err());
        for link in [
            serde_json::json!({"label":"bad","type":"url","target":"javascript:alert(1)"}),
            serde_json::json!({"label":"bad","type":"file","target":"../secret.txt"}),
        ] {
            std::fs::write(&file, serde_json::to_vec(&data(vec![link])).unwrap()).unwrap();
            assert!(read_manifest(&cwd, "{{run_id}}.json", "run-1").is_err());
        }
    }
    #[test]
    fn review_is_restricted_to_explicit_loopback_http_urls() {
        for value in [
            "http://127.0.0.1:8793/",
            "http://localhost:8793/",
            "http://[::1]:8793/",
        ] {
            assert!(local_review_url(value).is_ok(), "{value}");
        }
        for value in [
            "https://example.com",
            "file:///tmp/file",
            "http://localhost.evil.test",
            "http://user@localhost/",
            "http://127.0.0.2/",
        ] {
            assert!(local_review_url(value).is_err(), "{value}");
        }
    }
    #[test]
    fn followup_question_does_not_reopen_historical_automation_and_errors_have_stable_ids() {
        let temp = tempfile::tempdir().unwrap();
        let inbox = Inbox::open(&temp.path().join("inbox.json"), "local").unwrap();
        let mut run = run();
        run.status = AutomationRunStatus::Succeeded;
        inbox.automation(&job(), &run, false).unwrap();
        let id = format!("automation:{}", run.id);
        inbox
            .update(UpdateInboxParams {
                id: id.clone(),
                read: Some(true),
                done: Some(true),
            })
            .unwrap();
        inbox.event("chat", &question(), 10).unwrap();
        assert_eq!(inbox.list().len(), 2);
        assert!(inbox.get(&id).unwrap().done);
        assert_eq!(inbox.get(&id).unwrap().status, InboxStatus::Succeeded);
        let error = AgentEvent::Done {
            status: DoneStatus::Errored,
            result: None,
            error: Some("Failure".into()),
            session_id: None,
        };
        inbox.event("other-chat", &error, 42).unwrap();
        let error_id = "session:other-chat:done:42";
        inbox
            .update(UpdateInboxParams {
                id: error_id.into(),
                read: Some(true),
                done: Some(true),
            })
            .unwrap();
        inbox.event("other-chat", &error, 42).unwrap();
        assert!(inbox.get(error_id).unwrap().done);
        assert!(inbox.get(error_id).unwrap().read);
        assert_eq!(inbox.list().len(), 3);
    }
}
