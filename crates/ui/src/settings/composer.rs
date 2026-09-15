//! Sticky composer defaults — the new-chat "remember my last picks" store
//! (zeron parity: localStorage `zeron.composer.defaults:v1`, defaults.ts).
//!
//! A small JSON file beside `ui-settings.json` (that file is the shell's and
//! is saved debounced from its own boot-time copy, so the composer keeps its
//! own file rather than racing it): last harness, last model per harness
//! (id + label, so the chip names the pick before the model list loads),
//! last reasoning level, and last model option picks per harness. Written
//! synchronously on every pick (picks are rare); corrupt or missing files fall
//! back to defaults.
//!
//! [`PermissionDefaults`] follows the same rules in its own file
//! (`composer-permissions.json`) for the footer's permission picker: the last
//! pick is the next chat's default, and a chat that was changed keeps its own.

use std::collections::HashMap;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use zeron_proto::{HarnessId, PermissionMode, ReasoningLevel, SandboxLevel};

const FILE_NAME: &str = "composer-defaults.json";
const PERMISSIONS_FILE_NAME: &str = "composer-permissions.json";

/// Temp file + rename, so a crash mid-write never leaves a half file behind.
/// Each writer owns its temporary file; overlapping windows must not truncate
/// or rename one another's in-progress writes.
fn write_atomic(path: &Path, data_dir: &Path, json: &[u8]) -> io::Result<()> {
    std::fs::create_dir_all(data_dir)?;
    let tmp = path.with_extension(format!("json.{}.tmp", uuid::Uuid::new_v4()));
    let result = (|| {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp)?;
        file.write_all(json)?;
        file.sync_all()?;
        std::fs::rename(&tmp, path)?;
        #[cfg(unix)]
        std::fs::File::open(data_dir)?.sync_all()?;
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

/// Model option picks: option id → choice id (the `ChatConfig` shape).
pub type ModelOptions = serde_json::Map<String, serde_json::Value>;

/// Remembered model per harness — id plus display label, mirroring zeron's
/// `modelByHarness` storing the full `Model` object "so the pill never flashes
/// a raw id or 'Default'".
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RememberedModel {
    pub id: String,
    pub label: String,
}

/// One starred model in the picker (t3code client-settings `favorites`,
/// keyed `provider:model`) — harness + model id, insertion-ordered.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FavoriteModel {
    pub harness: HarnessId,
    pub model: String,
}

/// One chat's permission pick: how much the agent may do unasked, and the
/// sandbox the harnesses that understand one should run in.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PermissionChoice {
    #[serde(default)]
    pub mode: PermissionMode,
    #[serde(default = "default_sandbox")]
    pub sandbox: SandboxLevel,
}

fn default_sandbox() -> SandboxLevel {
    SandboxLevel::WorkspaceWrite
}

impl Default for PermissionChoice {
    fn default() -> Self {
        Self {
            mode: PermissionMode::Ask,
            sandbox: default_sandbox(),
        }
    }
}

impl PermissionChoice {
    /// Whether this is the shipped default (Ask + workspace write). Picks
    /// that ARE the default are never stored per chat, so the map stays
    /// small however many chats exist.
    pub fn is_default(&self) -> bool {
        *self == Self::default()
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct ComposerDefaults {
    /// Last harness picked on the new-chat canvas.
    pub harness: Option<HarnessId>,
    /// Last model picked, per harness (restored on harness switch).
    pub model_by_harness: HashMap<HarnessId, RememberedModel>,
    /// Last reasoning level picked (global, like zeron's `reasoning` key).
    /// Legacy: effort is per MODEL now ([`Self::effort_by_model`]); this is
    /// only kept so an existing file still reads and round-trips.
    pub reasoning: Option<ReasoningLevel>,
    /// Effort level per MODEL, keyed `"<harness>/<model id>"` (see
    /// [`effort_key`]). A level is chosen for the model it was chosen on:
    /// switching models restores that model's own effort instead of dragging
    /// the last pick along (user request).
    pub effort_by_model: HashMap<String, ReasoningLevel>,
    /// Last non-default model option picks (option id → choice id), per
    /// harness and model id. Model-scoped because each pick was validated
    /// against that model's catalog row, so it stays safe to send before the
    /// catalog reloads (the Claude harness appends `[1m]` to any model id).
    pub model_options_by_model: HashMap<HarnessId, HashMap<String, ModelOptions>>,
    /// Every model label ever seen (id → label), fed from catalog loads.
    /// The chip's fallback while a harness's list is still loading — a
    /// session whose configured model differs from the remembered pick
    /// would otherwise flash the raw id on switch.
    pub model_labels: HashMap<String, String>,
    /// Last device picked for new sessions (the composer's device selector).
    pub device: Option<String>,
    /// Last project picked for new sessions; `None` + `no_project` = the
    /// remembered "Don't work in a project" state.
    pub project: Option<String>,
    /// Remembered "Don't work in a project" opt-out.
    pub no_project: bool,
    /// Starred models (the picker's favorites rail), in starring order.
    pub favorites: Vec<FavoriteModel>,
}

/// The permission picker's sticky store — same shape and same rules as
/// [`ComposerDefaults`] (last pick becomes the new-chat default), in its own
/// file. Separate because [`ComposerDefaults`] has a single long-lived owner
/// that saves its whole in-memory copy: a second writer sharing that file
/// would have its fields overwritten by the first owner's next save.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct PermissionDefaults {
    /// Last permission pick, the default every NEW chat starts on (harness +
    /// model parity: the last pick is the next chat's starting point).
    pub permission: Option<PermissionChoice>,
    /// Per-chat permission picks, keyed by chat id. Only NON-default picks
    /// are stored, so the map stays proportional to the chats the user
    /// actually changed rather than to every chat ever opened.
    pub permission_by_chat: HashMap<String, PermissionChoice>,
}

impl PermissionDefaults {
    /// Load from `{data_dir}/composer-permissions.json`; defaults on any failure.
    pub fn load(data_dir: &Path) -> Self {
        match std::fs::read_to_string(Self::path(data_dir)) {
            Ok(text) => serde_json::from_str::<Self>(&text).unwrap_or_else(|err| {
                tracing::warn!(error = %err, "composer-permissions corrupt; using defaults");
                Self::default()
            }),
            Err(_) => Self::default(),
        }
    }

    /// Write atomically (temp file + rename), like [`ComposerDefaults::save`].
    pub fn save(&self, data_dir: &Path) -> io::Result<()> {
        write_atomic(
            &Self::path(data_dir),
            data_dir,
            &serde_json::to_vec_pretty(self)
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?,
        )
    }

    pub fn path(data_dir: &Path) -> PathBuf {
        data_dir.join(PERMISSIONS_FILE_NAME)
    }

    /// The permission pick in force for a chat: its own stored pick, else
    /// the sticky default, else Ask + workspace write. `None` = the new-chat
    /// canvas, which always reads the sticky default.
    pub fn permission_for(&self, chat_id: Option<&str>) -> PermissionChoice {
        chat_id
            .and_then(|id| self.permission_by_chat.get(id))
            .copied()
            .or(self.permission)
            .unwrap_or_default()
    }

    /// Record a pick. It becomes the sticky default for new chats either way;
    /// with a chat id it is also pinned to that chat (a pick that IS the
    /// default drops the chat's row instead of storing it).
    pub fn remember_permission(&mut self, chat_id: Option<&str>, choice: PermissionChoice) {
        self.permission = Some(choice);
        if let Some(id) = chat_id {
            if choice.is_default() {
                self.permission_by_chat.remove(id);
            } else {
                self.permission_by_chat.insert(id.to_string(), choice);
            }
        }
    }

    /// Carry the new-chat canvas's pick onto the chat id the composer just
    /// minted, so the chat keeps what it was sent with.
    pub fn adopt_permission(&mut self, chat_id: &str) {
        let choice = self.permission_for(None);
        if !choice.is_default() {
            self.permission_by_chat.insert(chat_id.to_string(), choice);
        }
    }
}

/// The `effortByModel` key for one model: the harness's wire name and the
/// model id, e.g. `"claude-code/claude-haiku-4-5"`. Derived from the serde
/// representation so a newly added harness needs no change here.
pub fn effort_key(harness: HarnessId, model: &str) -> String {
    let harness = serde_json::to_value(harness)
        .ok()
        .and_then(|value| value.as_str().map(str::to_owned))
        .unwrap_or_else(|| "unknown".to_string());
    format!("{harness}/{model}")
}

impl ComposerDefaults {
    /// Load from `{data_dir}/composer-defaults.json`; defaults on any failure.
    pub fn load(data_dir: &Path) -> Self {
        match std::fs::read_to_string(Self::path(data_dir)) {
            Ok(text) => match serde_json::from_str::<ComposerDefaults>(&text) {
                Ok(defaults) => defaults,
                Err(err) => {
                    tracing::warn!(error = %err, "composer-defaults corrupt; using defaults");
                    Self::default()
                }
            },
            Err(_) => Self::default(),
        }
    }

    /// Write atomically (temp file + rename) so a crash mid-write never corrupts.
    pub fn save(&self, data_dir: &Path) -> io::Result<()> {
        write_atomic(
            &Self::path(data_dir),
            data_dir,
            &serde_json::to_vec_pretty(self)
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?,
        )
    }

    pub fn path(data_dir: &Path) -> PathBuf {
        data_dir.join(FILE_NAME)
    }

    /// The remembered model for a harness, if any.
    pub fn model_for(&self, harness: HarnessId) -> Option<&RememberedModel> {
        self.model_by_harness.get(&harness)
    }

    /// Remember a pick (zeron `saveDefaults({ harness, modelByHarness })`).
    pub fn remember_model(&mut self, harness: HarnessId, id: String, label: String) {
        self.harness = Some(harness);
        self.model_by_harness
            .insert(harness, RememberedModel { id, label });
    }

    /// The remembered option picks for one model, if any.
    pub fn model_options_for(&self, harness: HarnessId, model: &str) -> Option<&ModelOptions> {
        self.model_options_by_model.get(&harness)?.get(model)
    }

    /// Mutable option picks for one model, created empty on first use.
    pub fn model_options_mut(&mut self, harness: HarnessId, model: &str) -> &mut ModelOptions {
        self.model_options_by_model
            .entry(harness)
            .or_default()
            .entry(model.to_string())
            .or_default()
    }

    /// The remembered effort level for one model, if it ever got a pick.
    pub fn effort_for(&self, harness: HarnessId, model: &str) -> Option<ReasoningLevel> {
        self.effort_by_model
            .get(&effort_key(harness, model))
            .copied()
    }

    /// Remember an effort pick for ONE model (never for the whole app).
    pub fn remember_effort(&mut self, harness: HarnessId, model: &str, level: ReasoningLevel) {
        self.effort_by_model
            .insert(effort_key(harness, model), level);
    }

    /// The cached display label for a model id, if ever seen.
    pub fn label_for(&self, id: &str) -> Option<&str> {
        self.model_labels.get(id).map(String::as_str)
    }

    /// Whether a model is starred.
    pub fn is_favorite(&self, harness: HarnessId, model: &str) -> bool {
        self.favorites
            .iter()
            .any(|f| f.harness == harness && f.model == model)
    }

    /// Star/unstar a model; returns whether it is starred AFTER the toggle.
    pub fn toggle_favorite(&mut self, harness: HarnessId, model: &str) -> bool {
        if let Some(at) = self
            .favorites
            .iter()
            .position(|f| f.harness == harness && f.model == model)
        {
            self.favorites.remove(at);
            false
        } else {
            self.favorites.push(FavoriteModel {
                harness,
                model: model.to_string(),
            });
            true
        }
    }

    /// Merge a loaded catalog into the label cache. Returns whether anything
    /// changed (callers only save when it did).
    pub fn remember_labels<'a>(
        &mut self,
        models: impl Iterator<Item = (&'a str, &'a str)>,
    ) -> bool {
        let mut changed = false;
        for (id, label) in models {
            if self.model_labels.get(id).map(String::as_str) != Some(label) {
                self.model_labels.insert(id.to_string(), label.to_string());
                changed = true;
            }
        }
        changed
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let mut defaults = ComposerDefaults {
            harness: Some(HarnessId::ClaudeCode),
            reasoning: Some(ReasoningLevel::XHigh),
            ..Default::default()
        };
        defaults.remember_model(
            HarnessId::ClaudeCode,
            "claude-fable-5".into(),
            "Fable 5".into(),
        );
        defaults.remember_model(HarnessId::Codex, "gpt-5.2-codex".into(), "GPT-5.2".into());
        defaults
            .model_options_mut(HarnessId::ClaudeCode, "claude-fable-5")
            .insert("contextWindow".into(), "1m".into());
        defaults.save(dir.path()).unwrap();
        let loaded = ComposerDefaults::load(dir.path());
        assert_eq!(loaded, defaults);
        assert_eq!(
            loaded.model_for(HarnessId::ClaudeCode).map(|m| &*m.label),
            Some("Fable 5")
        );
    }

    #[test]
    fn effort_is_remembered_per_model_and_survives_a_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let mut defaults = ComposerDefaults::default();
        defaults.remember_effort(
            HarnessId::ClaudeCode,
            "claude-opus-5",
            ReasoningLevel::Max,
        );
        defaults.remember_effort(HarnessId::Codex, "gpt-5.2-codex", ReasoningLevel::Low);
        // The key is "<harness>/<model id>", so the two never collide.
        assert_eq!(
            effort_key(HarnessId::ClaudeCode, "claude-opus-5"),
            "claude-code/claude-opus-5"
        );
        assert!(defaults.effort_by_model.contains_key("codex/gpt-5.2-codex"));
        // A model nobody picked for has no entry (it falls back to its own
        // catalog default at the picker).
        assert_eq!(
            defaults.effort_for(HarnessId::ClaudeCode, "claude-haiku-4-5"),
            None
        );
        defaults.save(dir.path()).unwrap();
        let loaded = ComposerDefaults::load(dir.path());
        assert_eq!(
            loaded.effort_for(HarnessId::ClaudeCode, "claude-opus-5"),
            Some(ReasoningLevel::Max)
        );
        assert_eq!(
            loaded.effort_for(HarnessId::Codex, "gpt-5.2-codex"),
            Some(ReasoningLevel::Low)
        );
    }

    #[test]
    fn missing_and_corrupt_files_yield_defaults() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            ComposerDefaults::load(dir.path()),
            ComposerDefaults::default()
        );
        std::fs::write(ComposerDefaults::path(dir.path()), "{nope").unwrap();
        assert_eq!(
            ComposerDefaults::load(dir.path()),
            ComposerDefaults::default()
        );
    }

    #[test]
    fn concurrent_projectless_saves_leave_a_complete_preference() {
        let dir = tempfile::tempdir().unwrap();
        std::thread::scope(|scope| {
            for i in 0..8 {
                let path = dir.path();
                scope.spawn(move || {
                    let defaults = ComposerDefaults {
                        device: Some(format!("device-{i}")),
                        no_project: true,
                        ..Default::default()
                    };
                    for _ in 0..10 {
                        defaults.save(path).unwrap();
                        let saved = ComposerDefaults::load(path);
                        assert!(saved.no_project);
                        assert!(saved.project.is_none());
                        assert!(saved.device.is_some());
                    }
                });
            }
        });
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
    }

    #[test]
    fn favorites_toggle_and_persist() {
        let dir = tempfile::tempdir().unwrap();
        let mut defaults = ComposerDefaults::default();
        assert!(defaults.toggle_favorite(HarnessId::ClaudeCode, "claude-opus-5"));
        assert!(defaults.toggle_favorite(HarnessId::Codex, "gpt-5.2-codex"));
        assert!(defaults.is_favorite(HarnessId::ClaudeCode, "claude-opus-5"));
        // Same id under a different harness is a distinct star.
        assert!(!defaults.is_favorite(HarnessId::Codex, "claude-opus-5"));
        defaults.save(dir.path()).unwrap();
        assert_eq!(ComposerDefaults::load(dir.path()), defaults);
        // Untoggle removes, preserving the other's order.
        assert!(!defaults.toggle_favorite(HarnessId::ClaudeCode, "claude-opus-5"));
        assert!(!defaults.is_favorite(HarnessId::ClaudeCode, "claude-opus-5"));
        assert!(defaults.is_favorite(HarnessId::Codex, "gpt-5.2-codex"));
    }

    #[test]
    fn permission_is_remembered_per_chat_and_survives_a_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let mut defaults = PermissionDefaults::default();
        // Nothing picked yet: Ask + workspace write, for any chat.
        assert_eq!(defaults.permission_for(None), PermissionChoice::default());
        assert_eq!(
            defaults.permission_for(Some("chat-1")).mode,
            PermissionMode::Ask
        );

        let auto = PermissionChoice {
            mode: PermissionMode::Auto,
            sandbox: SandboxLevel::DangerFullAccess,
        };
        defaults.remember_permission(Some("chat-1"), auto);
        assert_eq!(defaults.permission_for(Some("chat-1")), auto);
        // …and the same pick is now the default a NEW chat starts on.
        assert_eq!(defaults.permission_for(None), auto);
        // A chat that never got its own pick follows the sticky default.
        assert_eq!(defaults.permission_for(Some("chat-2")), auto);

        let edits = PermissionChoice {
            mode: PermissionMode::AutoEdits,
            sandbox: SandboxLevel::ReadOnly,
        };
        defaults.remember_permission(Some("chat-2"), edits);
        assert_eq!(defaults.permission_for(Some("chat-1")), auto);
        assert_eq!(defaults.permission_for(Some("chat-2")), edits);

        defaults.save(dir.path()).unwrap();
        let loaded = PermissionDefaults::load(dir.path());
        assert_eq!(loaded, defaults);
        assert_eq!(loaded.permission_for(Some("chat-1")), auto);
        assert_eq!(loaded.permission_for(Some("chat-2")), edits);
    }

    #[test]
    fn a_default_permission_pick_stores_no_chat_row() {
        let mut defaults = PermissionDefaults::default();
        let auto = PermissionChoice {
            mode: PermissionMode::Auto,
            sandbox: SandboxLevel::WorkspaceWrite,
        };
        defaults.remember_permission(Some("chat-1"), auto);
        assert!(defaults.permission_by_chat.contains_key("chat-1"));
        // Back to Ask + workspace write: the row goes away rather than
        // accumulating one entry per chat ever opened.
        defaults.remember_permission(Some("chat-1"), PermissionChoice::default());
        assert!(defaults.permission_by_chat.is_empty());
        assert_eq!(defaults.permission_for(Some("chat-1")).mode, PermissionMode::Ask);
    }

    #[test]
    fn a_new_chat_adopts_the_pick_it_was_sent_with() {
        let mut defaults = PermissionDefaults::default();
        // Picked on the new-chat canvas, before any chat id exists.
        defaults.remember_permission(
            None,
            PermissionChoice {
                mode: PermissionMode::Auto,
                sandbox: SandboxLevel::WorkspaceWrite,
            },
        );
        defaults.adopt_permission("minted-chat");
        assert_eq!(
            defaults.permission_for(Some("minted-chat")).mode,
            PermissionMode::Auto
        );
        // The sticky default then changing must not rewrite that chat.
        defaults.remember_permission(None, PermissionChoice::default());
        assert_eq!(
            defaults.permission_for(Some("minted-chat")).mode,
            PermissionMode::Auto
        );
    }

    #[test]
    fn remember_model_updates_harness_and_row() {
        let mut defaults = ComposerDefaults::default();
        defaults.remember_model(HarnessId::Codex, "m1".into(), "One".into());
        defaults.remember_model(HarnessId::Codex, "m2".into(), "Two".into());
        assert_eq!(defaults.harness, Some(HarnessId::Codex));
        assert_eq!(
            defaults.model_for(HarnessId::Codex).map(|m| &*m.id),
            Some("m2")
        );
        assert!(defaults.model_for(HarnessId::ClaudeCode).is_none());
    }
}
