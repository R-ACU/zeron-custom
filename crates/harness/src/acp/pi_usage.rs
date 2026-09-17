//! pi-acp omits usage on its ACP response. Read only the associated Pi journal;
//! stable message IDs let the engine persist and deduplicate backfilled costs.
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

use serde_json::Value;
use tokio::sync::mpsc;
use zeron_proto::{AgentEvent, HarnessId};

use crate::HarnessError;

pub(super) async fn emit(
    harness: HarnessId,
    session_id: &str,
    tx: &mpsc::Sender<Result<AgentEvent, HarnessError>>,
) {
    if harness != HarnessId::Pi {
        return;
    }
    let Some(home) = crate::home_dir() else {
        return;
    };
    let agent_dir = std::env::var_os("PI_CODING_AGENT_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(".pi/agent"));
    let session_id = session_id.to_owned();
    let events = tokio::task::spawn_blocking(move || read_costs(&home, &agent_dir, &session_id))
        .await
        .unwrap_or_default();
    for event in events {
        if tx.send(Ok(event)).await.is_err() {
            break;
        }
    }
}

fn read_costs(home: &Path, agent_dir: &Path, session_id: &str) -> Vec<AgentEvent> {
    // pi-acp's mapping is always under ~/.pi/pi-acp, independently of
    // PI_CODING_AGENT_DIR. Honor custom session paths recorded by the adapter.
    if let Ok(raw) = std::fs::read(home.join(".pi/pi-acp/session-map.json"))
        && let Ok(map) = serde_json::from_slice::<Value>(&raw)
        && map["version"] == 1
        && let Some(path) = map["sessions"][session_id]["sessionFile"].as_str()
        && let Some(events) = read_journal(Path::new(path), session_id)
    {
        return events;
    }
    // Legacy/resumed sessions may not have a map entry yet. Match filenames
    // before opening anything; never scan unrelated transcript contents.
    if uuid::Uuid::parse_str(session_id).is_err() {
        return Vec::new();
    }
    find_journal(&agent_dir.join("sessions"), session_id, 2).unwrap_or_default()
}

fn find_journal(dir: &Path, session_id: &str, depth: usize) -> Option<Vec<AgentEvent>> {
    let suffix = format!("_{session_id}.jsonl");
    for entry in std::fs::read_dir(dir).ok()?.flatten() {
        let kind = entry.file_type().ok()?;
        if kind.is_dir() && depth > 0 {
            if let Some(events) = find_journal(&entry.path(), session_id, depth - 1) {
                return Some(events);
            }
        } else if kind.is_file()
            && entry.file_name().to_string_lossy().ends_with(&suffix)
            && let Some(events) = read_journal(&entry.path(), session_id)
        {
            return Some(events);
        }
    }
    None
}

fn read_journal(path: &Path, session_id: &str) -> Option<Vec<AgentEvent>> {
    let mut lines = BufReader::new(std::fs::File::open(path).ok()?).lines();
    let header: Value = serde_json::from_str(&lines.next()?.ok()?).ok()?;
    if header["type"] != "session" || header["id"].as_str() != Some(session_id) {
        return None;
    }
    let mut events = Vec::new();
    for line in lines.map_while(Result::ok) {
        // A partially written last line is retried on the next settlement.
        let Ok(entry) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        if entry["type"] != "message" || entry["message"]["role"] != "assistant" {
            continue;
        }
        let Some(id) = entry["id"].as_str().filter(|id| !id.is_empty()) else {
            continue;
        };
        let Some(usd) = entry["message"]["usage"]["cost"]["total"].as_f64() else {
            continue;
        };
        if usd.is_finite() && usd >= 0.0 {
            events.push(AgentEvent::Cost {
                id: format!("pi/{session_id}/{id}"),
                usd,
            });
        }
    }
    Some(events)
}

#[cfg(test)]
mod tests {
    use super::*;
    const SESSION: &str = "01a0ac1e-6e87-76e5-9e3d-f6cb88893fab";

    fn journal(path: &Path) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, format!(r#"{{"type":"session","id":"{SESSION}"}}
{{"type":"message","id":"a","message":{{"role":"assistant","usage":{{"cost":{{"total":0.125}}}}}}}}
{{"type":"message","id":"b","message":{{"role":"assistant","usage":{{"cost":{{"total":0}}}}}}}}
{{"type":"message","id":"bad","message":{{"role":"user","usage":{{"cost":{{"total":99}}}}}}}}
{{"type":"message","id":"negative","message":{{"role":"assistant","usage":{{"cost":{{"total":-1}}}}}}}}
{{"type":"message","id":"partial"
"#)).unwrap();
    }

    #[test]
    fn extracts_only_valid_assistant_costs_with_stable_ids() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("session.jsonl");
        journal(&path);
        let events = read_journal(&path, SESSION).unwrap();
        assert_eq!(events.len(), 2);
        assert!(
            matches!(&events[0], AgentEvent::Cost { id, usd } if id == &format!("pi/{SESSION}/a") && *usd == 0.125)
        );
        assert!(matches!(&events[1], AgentEvent::Cost { usd, .. } if *usd == 0.0));
        assert!(read_journal(&path, "unrelated").is_none());
        assert_eq!(
            serde_json::to_value(&events).unwrap(),
            serde_json::to_value(read_journal(&path, SESSION).unwrap()).unwrap()
        );
    }

    #[test]
    fn honors_adapter_mapping_and_custom_agent_directory_fallback() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        let agent = tmp.path().join("custom-agent");
        let mapped = tmp.path().join("custom-session.jsonl");
        journal(&mapped);
        std::fs::create_dir_all(home.join(".pi/pi-acp")).unwrap();
        let map = home.join(".pi/pi-acp/session-map.json");
        std::fs::write(
            &map,
            serde_json::json!({"version":1,"sessions":{SESSION:{"sessionFile":mapped}}})
                .to_string(),
        )
        .unwrap();
        assert_eq!(read_costs(&home, &agent, SESSION).len(), 2);
        std::fs::remove_file(mapped).unwrap();
        journal(
            &agent
                .join("sessions/workspace")
                .join(format!("timestamp_{SESSION}.jsonl")),
        );
        assert_eq!(read_costs(&home, &agent, SESSION).len(), 2);
        assert!(read_costs(&home, &agent, "../unrelated").is_empty());
    }
}
