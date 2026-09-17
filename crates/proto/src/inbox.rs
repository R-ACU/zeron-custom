//! Persistent, device-local work notifications and references to results.
use serde::{Deserialize, Serialize};
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum InboxSource {
    Automation,
    Session,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum InboxStatus {
    Running,
    AwaitingInput,
    Succeeded,
    Failed,
    Interrupted,
    Resolved,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum InboxLinkType {
    File,
    Url,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InboxLink {
    pub label: String,
    #[serde(rename = "type")]
    pub kind: InboxLinkType,
    pub target: String,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InboxItem {
    pub id: String,
    pub device_id: String,
    pub source: InboxSource,
    /// Automation ID, or the session's request ID.
    pub source_id: String,
    pub chat_id: String,
    pub title: String,
    pub summary: Option<String>,
    pub error: Option<String>,
    #[serde(default)]
    pub links: Vec<InboxLink>,
    pub status: InboxStatus,
    pub read: bool,
    pub done: bool,
    pub created_at: i64,
    pub updated_at: i64,
    #[serde(default)]
    pub review_url: Option<String>,
    /// Fingerprint of the user-saved review configuration, never supplied by manifests.
    #[serde(default)]
    pub review_config_hash: Option<String>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UpdateInboxParams {
    pub id: String,
    pub read: Option<bool>,
    pub done: Option<bool>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OpenInboxReviewParams {
    pub id: String,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OpenInboxReviewResult {
    pub url: String,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ResultManifest {
    pub run_id: String,
    pub summary: String,
    #[serde(default)]
    pub links: Vec<InboxLink>,
}
