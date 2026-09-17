//! Per-model effort ladders for models routed through pi.
//!
//! The `pi-acp` adapter advertises ONE flat `thought_level` ladder
//! (`off, minimal, low, medium, high, xhigh`) for every model, so the picker
//! used to offer X-High on models that cannot think at all. pi itself knows
//! better: its bundled `pi-ai` catalog carries a `reasoning` flag and an
//! optional `thinkingLevelMap` per model, and `getSupportedThinkingLevels`
//! (pi-ai `models.js`) derives the real ladder from them:
//!
//! - `reasoning: false` → only `off`, i.e. no effort control at all;
//! - a level whose map entry is explicitly `null` is not offered;
//! - `xhigh` and `max` are offered ONLY when the map names them (they are
//!   provider extras, not part of the common ladder);
//! - everything else (`minimal`…`high`) is offered.
//!
//! This module reads those catalog files straight from the installed pi
//! package (found next to the `pi` shim on PATH: npm installs the bundled
//! `pi-ai` under `node_modules/@earendil-works/pi-coding-agent/node_modules/`
//! or hoisted one level up), keys them the way `pi-acp` composes model ids
//! (`{provider}/{id}`), and overlays the derived ladders on the discovered
//! rows. The wire's own ladder still bounds the result: `pi-acp` rejects
//! `max`, so a model whose map names it never gets a rung the adapter would
//! refuse. A model the catalog does not know keeps the wire ladder.
//!
//! Custom models from `~/.pi/agent/models.json` are read too (same fields
//! per model); pi treats a missing `reasoning` there as `false`.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

use serde::Deserialize;
use zeron_proto::{Model, ReasoningLevel};

/// The common ladder, in pi's order. `Off` has no zeron rung.
const LADDER: [(&str, ReasoningLevel); 6] = [
    ("minimal", ReasoningLevel::Minimal),
    ("low", ReasoningLevel::Low),
    ("medium", ReasoningLevel::Medium),
    ("high", ReasoningLevel::High),
    ("xhigh", ReasoningLevel::XHigh),
    ("max", ReasoningLevel::Max),
];

/// What pi records about one model's thinking support.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct PiThinking {
    pub reasoning: bool,
    /// `None` when the catalog row has no map at all (common ladder minus the
    /// provider extras). `Some(None)` for a level means "explicitly off".
    pub level_map: Option<HashMap<String, Option<String>>>,
}

impl PiThinking {
    /// pi's `getSupportedThinkingLevels`, restricted to the rungs `wire`
    /// (the adapter's advertised ladder) will accept. Empty means the model
    /// has no effort control.
    pub fn supported_levels(&self, wire: &[ReasoningLevel]) -> Vec<ReasoningLevel> {
        if !self.reasoning {
            return Vec::new();
        }
        LADDER
            .iter()
            .filter(|(name, level)| {
                let extra = matches!(*name, "xhigh" | "max");
                let offered = match &self.level_map {
                    Some(map) => match map.get(*name) {
                        Some(None) => false,
                        Some(Some(_)) => true,
                        None => !extra,
                    },
                    None => !extra,
                };
                offered && wire.contains(level)
            })
            .map(|(_, level)| *level)
            .collect()
    }
}

/// pi's catalog keyed by lowercase `{provider}/{id}`.
#[derive(Debug, Default, Clone)]
pub struct PiCatalog {
    by_id: HashMap<String, PiThinking>,
}

impl PiCatalog {
    pub fn is_empty(&self) -> bool {
        self.by_id.is_empty()
    }

    pub fn len(&self) -> usize {
        self.by_id.len()
    }

    pub fn get(&self, id: &str) -> Option<&PiThinking> {
        self.by_id.get(&id.trim().to_ascii_lowercase())
    }

    fn insert(&mut self, provider: &str, id: &str, thinking: PiThinking) {
        let (provider, id) = (provider.trim(), id.trim());
        if provider.is_empty() || id.is_empty() {
            return;
        }
        self.by_id
            .insert(format!("{provider}/{id}").to_ascii_lowercase(), thinking);
    }

    /// Parse one bundled `providers/data/<provider>.json`: `{ "<api>": {
    /// "<model id>": { provider, reasoning, thinkingLevelMap?, … } } }`.
    /// A malformed file adds nothing and breaks nothing.
    pub fn add_bundled(&mut self, body: &str) {
        let Ok(apis) = serde_json::from_str::<HashMap<String, HashMap<String, BundledRow>>>(body)
        else {
            return;
        };
        for models in apis.into_values() {
            for (id, row) in models {
                let Some(provider) = row.provider else {
                    continue;
                };
                self.insert(
                    &provider,
                    &id,
                    PiThinking {
                        reasoning: row.reasoning.unwrap_or(false),
                        level_map: row.thinking_level_map,
                    },
                );
            }
        }
    }

    /// Parse the user's `~/.pi/agent/models.json`: `{ "providers": {
    /// "<name>": { "models": [ { id, reasoning?, thinkingLevelMap?, … } ] } } }`.
    pub fn add_custom(&mut self, body: &str) {
        let Ok(file) = serde_json::from_str::<CustomFile>(body) else {
            return;
        };
        for (provider, entry) in file.providers {
            for row in entry.models {
                self.insert(
                    &provider,
                    &row.id,
                    PiThinking {
                        reasoning: row.reasoning.unwrap_or(false),
                        level_map: row.thinking_level_map,
                    },
                );
            }
        }
    }

    /// Read every bundled data file under `dir` (pi-ai's
    /// `dist/providers/data`).
    pub fn add_bundled_dir(&mut self, dir: &Path) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().is_some_and(|e| e == "json")
                && let Ok(body) = std::fs::read_to_string(&path)
            {
                self.add_bundled(&body);
            }
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct BundledRow {
    provider: Option<String>,
    reasoning: Option<bool>,
    thinking_level_map: Option<HashMap<String, Option<String>>>,
}

#[derive(Debug, Deserialize)]
struct CustomFile {
    #[serde(default)]
    providers: HashMap<String, CustomProvider>,
}

#[derive(Debug, Deserialize)]
struct CustomProvider {
    #[serde(default)]
    models: Vec<CustomRow>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CustomRow {
    id: String,
    reasoning: Option<bool>,
    thinking_level_map: Option<HashMap<String, Option<String>>>,
}

/// Overlay pi's per-model ladders on discovered rows. `wire` is the ladder
/// the adapter advertised (the rungs it will accept); rows the catalog does
/// not know are left alone.
pub fn apply(models: &mut [Model], catalog: &PiCatalog, wire: &[ReasoningLevel]) {
    if catalog.is_empty() {
        return;
    }
    for model in models {
        if let Some(thinking) = catalog.get(&model.id) {
            model.reasoning_levels = thinking.supported_levels(wire);
        }
    }
}

/// Where the installed pi keeps its bundled catalog, if pi is installed:
/// the `pi` shim's directory is npm's global bin dir, the package sits in
/// its `node_modules`, and `pi-ai` is either nested under it or hoisted.
pub fn bundled_data_dir(pi_shim: &Path) -> Option<PathBuf> {
    let bin_dir = pi_shim.parent()?;
    let scope = bin_dir.join("node_modules").join("@earendil-works");
    let data = |root: PathBuf| root.join("dist").join("providers").join("data");
    [
        data(scope
            .join("pi-coding-agent")
            .join("node_modules")
            .join("@earendil-works")
            .join("pi-ai")),
        data(scope.join("pi-ai")),
    ]
    .into_iter()
    .find(|dir| dir.is_dir())
}

/// The user's custom model file, `~/.pi/agent/models.json`.
fn custom_models_path() -> Option<PathBuf> {
    crate::home_dir().map(|home| home.join(".pi").join("agent").join("models.json"))
}

/// The installed pi's catalog, read once per process (pi upgrades take
/// effect on the next engine start, like the adapter's own model cache).
pub fn catalog() -> Arc<PiCatalog> {
    static CATALOG: OnceLock<Arc<PiCatalog>> = OnceLock::new();
    CATALOG
        .get_or_init(|| {
            let mut catalog = PiCatalog::default();
            if let Some(dir) = crate::find_executable("pi", crate::acp::npm_global_bins())
                .and_then(|shim| bundled_data_dir(&shim))
            {
                catalog.add_bundled_dir(&dir);
                tracing::debug!(dir = %dir.display(), models = catalog.len(), "pi catalog read");
            } else {
                tracing::debug!("pi catalog not found; models keep the adapter's ladder");
            }
            if let Some(body) = custom_models_path().and_then(|p| std::fs::read_to_string(p).ok())
            {
                catalog.add_custom(&body);
            }
            Arc::new(catalog)
        })
        .clone()
}

#[cfg(test)]
mod tests {
    use super::*;

    pub(super) const WIRE: [ReasoningLevel; 5] = [
        ReasoningLevel::Minimal,
        ReasoningLevel::Low,
        ReasoningLevel::Medium,
        ReasoningLevel::High,
        ReasoningLevel::XHigh,
    ];

    fn map(pairs: &[(&str, Option<&str>)]) -> Option<HashMap<String, Option<String>>> {
        Some(
            pairs
                .iter()
                .map(|(k, v)| ((*k).to_owned(), v.map(str::to_owned)))
                .collect(),
        )
    }

    #[test]
    fn a_non_reasoning_model_has_no_ladder() {
        let t = PiThinking {
            reasoning: false,
            level_map: None,
        };
        assert!(t.supported_levels(&WIRE).is_empty());
    }

    #[test]
    fn no_map_means_the_common_ladder_without_extras() {
        let t = PiThinking {
            reasoning: true,
            level_map: None,
        };
        assert_eq!(
            t.supported_levels(&WIRE),
            vec![
                ReasoningLevel::Minimal,
                ReasoningLevel::Low,
                ReasoningLevel::Medium,
                ReasoningLevel::High
            ]
        );
    }

    #[test]
    fn explicit_nulls_drop_rungs_and_named_extras_add_them() {
        // openrouter/deepseek/deepseek-v4-pro as pi ships it.
        let t = PiThinking {
            reasoning: true,
            level_map: map(&[
                ("off", Some("none")),
                ("minimal", None),
                ("low", None),
                ("medium", None),
                ("high", Some("high")),
                ("xhigh", Some("xhigh")),
                ("max", None),
            ]),
        };
        assert_eq!(
            t.supported_levels(&WIRE),
            vec![ReasoningLevel::High, ReasoningLevel::XHigh]
        );
    }

    #[test]
    fn the_wire_ladder_bounds_the_result() {
        // Claude via the anthropic provider names max; pi-acp refuses it.
        let t = PiThinking {
            reasoning: true,
            level_map: map(&[("off", None), ("xhigh", Some("xhigh")), ("max", Some("max"))]),
        };
        assert_eq!(
            t.supported_levels(&WIRE),
            vec![
                ReasoningLevel::Minimal,
                ReasoningLevel::Low,
                ReasoningLevel::Medium,
                ReasoningLevel::High,
                ReasoningLevel::XHigh
            ]
        );
        assert!(t.supported_levels(&[ReasoningLevel::High]) == vec![ReasoningLevel::High]);
    }

    #[test]
    fn bundled_files_key_by_provider_and_id() {
        let mut catalog = PiCatalog::default();
        catalog.add_bundled(
            r#"{"openai-completions":{
                "deepseek/deepseek-chat":{"provider":"openrouter","reasoning":false},
                "deepseek/deepseek-v4-pro":{"provider":"openrouter","reasoning":true,
                    "thinkingLevelMap":{"off":"none","high":"high","xhigh":"xhigh","max":null}}
            },"anthropic-messages":{
                "anthropic/claude-3-haiku":{"provider":"openrouter","reasoning":false}
            }}"#,
        );
        assert_eq!(catalog.len(), 3);
        assert!(!catalog.get("OpenRouter/deepseek/deepseek-chat").unwrap().reasoning);
        assert!(catalog.get("openrouter/deepseek/deepseek-v4-pro").unwrap().reasoning);
        assert!(catalog.get("openrouter/nope").is_none());
        // Garbage adds nothing.
        catalog.add_bundled("not json");
        assert_eq!(catalog.len(), 3);
    }

    #[test]
    fn custom_models_default_to_no_reasoning() {
        let mut catalog = PiCatalog::default();
        catalog.add_custom(
            r#"{"providers":{"local":{"baseUrl":"http://x","models":[
                {"id":"plain"},
                {"id":"thinker","reasoning":true}
            ]}}}"#,
        );
        assert!(!catalog.get("local/plain").unwrap().reasoning);
        assert!(catalog.get("local/thinker").unwrap().reasoning);
    }

    #[test]
    fn apply_overlays_known_rows_only() {
        let mut catalog = PiCatalog::default();
        catalog.add_bundled(
            r#"{"x":{"a":{"provider":"p","reasoning":false},
                     "b":{"provider":"p","reasoning":true}}}"#,
        );
        let row = |id: &str| Model {
            id: id.into(),
            label: id.into(),
            description: None,
            reasoning_levels: WIRE.to_vec(),
            options: Vec::new(),
            pricing: None,
        };
        let mut models = vec![row("p/a"), row("p/b"), row("p/unknown")];
        apply(&mut models, &catalog, &WIRE);
        assert!(models[0].reasoning_levels.is_empty());
        assert_eq!(models[1].reasoning_levels, WIRE[..4].to_vec());
        assert_eq!(models[2].reasoning_levels, WIRE.to_vec());
    }

    #[test]
    fn bundled_dir_is_found_next_to_the_shim() {
        let root = std::env::temp_dir().join(format!("zeron-pi-{}", std::process::id()));
        let data = root
            .join("node_modules/@earendil-works/pi-coding-agent/node_modules/@earendil-works/pi-ai/dist/providers/data");
        std::fs::create_dir_all(&data).unwrap();
        assert_eq!(bundled_data_dir(&root.join("pi.cmd")), Some(data));
        assert_eq!(bundled_data_dir(&std::env::temp_dir().join("nowhere/pi")), None);
        let _ = std::fs::remove_dir_all(&root);
    }
}

/// Live probe against the pi installed on this machine (`cargo test -p
/// zeron-harness --lib pi_thinking::live -- --ignored --nocapture`).
#[cfg(test)]
mod live {
    #[test]
    #[ignore = "needs a pi install on PATH"]
    fn installed_catalog_loads() {
        let catalog = super::catalog();
        println!("pi catalog rows: {}", catalog.len());
        for id in [
            "openrouter/anthropic/claude-3-haiku",
            "openrouter/anthropic/claude-fable-5.1",
            "openrouter/deepseek/deepseek-v4-pro",
            "deepseek/deepseek-v4-pro",
        ] {
            println!("{id}: {:?}", catalog.get(id).map(|t| t.supported_levels(&super::tests::WIRE)));
        }
        assert!(!catalog.is_empty());
    }
}
