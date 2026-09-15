//! ApiKeys — the provider API keys zeron holds on behalf of the agent CLIs it
//! spawns (Settings → Accounts → "API keys").
//!
//! Zeron never calls a model provider itself. The agents do: OpenCode and Pi
//! read `OPENROUTER_API_KEY` / `DEEPSEEK_API_KEY` / `GROQ_API_KEY` / …, Grok
//! CLI reads `XAI_API_KEY`, Kimi reads `MOONSHOT_API_KEY`. Each of them picks
//! the key out of its own process environment, which it inherits from the
//! engine. So the store has exactly two jobs:
//!
//! 1. **Persist** the keys in `{data_dir}/api-keys.json`, written through the
//!    same private-file helper the agent-account slots use
//!    ([`crate::agent_accounts::write_file_atomic`] with `secret = true`:
//!    0600 on unix, a single-ACE user-only ACL on Windows). The file is engine
//!    data, never synced to the edge and never logged.
//! 2. **Inject** them into the engine's own process environment — at startup
//!    and again whenever a key is saved or removed — so every agent CLI
//!    spawned afterwards inherits them without the harness crate knowing this
//!    feature exists.
//!
//! Injection rule: a variable that was ALREADY set in the real environment
//! zeron was launched with belongs to the user (a shell export, a system-wide
//! setting) and is never overwritten or cleared. [`ApiKeys`] records the set
//! it found at construction ("foreign") and the set it has set itself
//! ("owned"), and only ever touches the latter.
//!
//! Secrets never leave this module in plaintext except through
//! [`ApiKeys::set`]'s argument: [`ApiKeys::list`] returns masked values only,
//! nothing here is `tracing`-logged, and `Debug` is not derived on the record
//! that holds the key.

use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, PoisonError};

use serde::{Deserialize, Serialize};

use zeron_proto::{ApiKeyProvider, ApiKeysSnapshot, StoredApiKey};

use crate::agent_accounts::write_file_atomic;
use crate::{EngineError, now_ms};

/// The environment variable each provider's CLIs read.
///
/// `OllamaCompatible` is the odd one out: the value is an OpenAI-compatible
/// base URL rather than a credential, and `OLLAMA_HOST` is the variable the
/// Ollama client libraries actually consult.
pub fn env_var(provider: ApiKeyProvider) -> &'static str {
    match provider {
        ApiKeyProvider::Anthropic => "ANTHROPIC_API_KEY",
        ApiKeyProvider::Openai => "OPENAI_API_KEY",
        ApiKeyProvider::Openrouter => "OPENROUTER_API_KEY",
        ApiKeyProvider::Deepseek => "DEEPSEEK_API_KEY",
        ApiKeyProvider::Groq => "GROQ_API_KEY",
        ApiKeyProvider::Xai => "XAI_API_KEY",
        ApiKeyProvider::Moonshot => "MOONSHOT_API_KEY",
        ApiKeyProvider::Google => "GEMINI_API_KEY",
        ApiKeyProvider::Mistral => "MISTRAL_API_KEY",
        ApiKeyProvider::Fireworks => "FIREWORKS_API_KEY",
        ApiKeyProvider::Together => "TOGETHER_API_KEY",
        ApiKeyProvider::OllamaCompatible => "OLLAMA_HOST",
    }
}

/// `sk-or-…4f2a` — enough of a key to tell two of them apart, never enough to
/// use one. Pure.
///
/// The visible head is the provider prefix: leading characters up to and
/// including the second `-`/`_` separator, capped at 8. Keys with no
/// separator show their first four characters; anything at or under 8
/// characters is hidden completely (there would be no tail left to elide).
/// The visible tail is the last four characters.
pub fn mask(secret: &str) -> String {
    let chars: Vec<char> = secret.chars().collect();
    if chars.len() <= 8 {
        return "\u{2022}".repeat(chars.len().max(3));
    }
    let mut head = 0usize;
    let mut separators = 0usize;
    for (ix, ch) in chars.iter().enumerate().take(8) {
        if *ch == '-' || *ch == '_' {
            separators += 1;
            head = ix + 1;
            if separators == 2 {
                break;
            }
        }
    }
    if head == 0 {
        head = 4;
    }
    let tail: String = chars[chars.len() - 4..].iter().collect();
    let head: String = chars[..head].iter().collect();
    format!("{head}\u{2026}{tail}")
}

/// A loose shape check: the prefix hint providers publish for their keys. The
/// result is ADVICE, not a gate — providers rotate key formats and zeron has
/// no business refusing a key the provider issued. `None` = nothing to say.
pub fn shape_hint(provider: ApiKeyProvider, key: &str) -> Option<&'static str> {
    let key = key.trim();
    if key.is_empty() {
        return None;
    }
    let (prefixes, hint): (&[&str], &'static str) = match provider {
        ApiKeyProvider::Anthropic => (&["sk-ant-"], "Anthropic keys usually start with sk-ant-"),
        ApiKeyProvider::Openai => (&["sk-"], "OpenAI keys usually start with sk-"),
        ApiKeyProvider::Openrouter => (&["sk-or-"], "OpenRouter keys usually start with sk-or-"),
        ApiKeyProvider::Deepseek => (&["sk-"], "DeepSeek keys usually start with sk-"),
        ApiKeyProvider::Groq => (&["gsk_"], "Groq keys usually start with gsk_"),
        ApiKeyProvider::Xai => (&["xai-"], "xAI keys usually start with xai-"),
        ApiKeyProvider::Moonshot => (&["sk-"], "Moonshot keys usually start with sk-"),
        ApiKeyProvider::Google => (&["AIza"], "Gemini keys usually start with AIza"),
        ApiKeyProvider::OllamaCompatible => (
            &["http://", "https://"],
            "Enter the full base URL, for example http://127.0.0.1:11434",
        ),
        // Mistral, Fireworks and Together issue opaque keys with no stable
        // prefix — there is nothing honest to check.
        ApiKeyProvider::Mistral | ApiKeyProvider::Fireworks | ApiKeyProvider::Together => {
            (&[], "")
        }
    };
    if prefixes.is_empty() || prefixes.iter().any(|p| key.starts_with(p)) {
        None
    } else {
        Some(hint)
    }
}

/// One stored key. `Debug` is deliberately NOT derived — the secret must not
/// reach a log line through a `{:?}` on some enclosing struct.
#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Entry {
    key: String,
    updated_at: i64,
}

impl std::fmt::Debug for Entry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Entry")
            .field("key", &mask(&self.key))
            .field("updated_at", &self.updated_at)
            .finish()
    }
}

#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct StoreFile {
    #[serde(default)]
    keys: BTreeMap<ApiKeyProvider, Entry>,
}

struct Inner {
    file: PathBuf,
    entries: Mutex<BTreeMap<ApiKeyProvider, Entry>>,
    /// Variables that were already set in the real environment at startup.
    /// The user's own export wins forever; zeron neither overwrites nor
    /// clears these.
    foreign: HashSet<&'static str>,
    /// Variables this process set itself — the only ones it may change.
    owned: Mutex<HashSet<&'static str>>,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

#[derive(Clone)]
pub struct ApiKeys {
    inner: Arc<Inner>,
}

static SHARED: OnceLock<ApiKeys> = OnceLock::new();

impl ApiKeys {
    /// The process-wide store for `data_dir`. Built once: a second instance
    /// would snapshot the environment AFTER the first one exported into it and
    /// would then treat zeron's own variables as the user's.
    pub fn shared(data_dir: &Path) -> ApiKeys {
        SHARED
            .get_or_init(|| ApiKeys::open(data_dir.join("api-keys.json")))
            .clone()
    }

    /// Open a store at an explicit path and apply it to the environment.
    /// Production goes through [`ApiKeys::shared`]; tests use this directly.
    pub fn open(file: PathBuf) -> ApiKeys {
        let entries = std::fs::read(&file)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<StoreFile>(&bytes).ok())
            .map(|store| store.keys)
            .unwrap_or_default();
        // Snapshot BEFORE the first apply, or our own exports would look like
        // the user's.
        let foreign: HashSet<&'static str> = ApiKeyProvider::ALL
            .into_iter()
            .map(env_var)
            .filter(|name| {
                std::env::var_os(name).is_some_and(|value| !value.is_empty())
            })
            .collect();
        let keys = ApiKeys {
            inner: Arc::new(Inner {
                file,
                entries: Mutex::new(entries),
                foreign,
                owned: Mutex::new(HashSet::new()),
            }),
        };
        keys.apply_all();
        keys
    }

    /// The masked view the settings page renders. No plaintext ever.
    pub fn list(&self) -> ApiKeysSnapshot {
        let entries = lock(&self.inner.entries);
        ApiKeysSnapshot {
            keys: ApiKeyProvider::ALL
                .into_iter()
                .filter_map(|provider| {
                    let entry = entries.get(&provider)?;
                    Some(StoredApiKey {
                        provider,
                        masked: if provider.is_secret() {
                            mask(&entry.key)
                        } else {
                            entry.key.clone()
                        },
                        env_var: env_var(provider).to_string(),
                        applied: !self.inner.foreign.contains(env_var(provider)),
                        updated_at: entry.updated_at,
                    })
                })
                .collect(),
        }
    }

    /// Save (or replace) one provider's key, then re-export.
    pub fn set(&self, provider: ApiKeyProvider, key: &str) -> Result<ApiKeysSnapshot, EngineError> {
        let key = key.trim();
        if key.is_empty() {
            return Err(EngineError::Other("The key is empty.".into()));
        }
        lock(&self.inner.entries).insert(
            provider,
            Entry {
                key: key.to_string(),
                updated_at: now_ms(),
            },
        );
        self.persist()?;
        self.apply(provider);
        Ok(self.list())
    }

    /// Forget one provider's key and unset the variable — but only if zeron
    /// was the one that set it.
    pub fn remove(&self, provider: ApiKeyProvider) -> Result<ApiKeysSnapshot, EngineError> {
        lock(&self.inner.entries).remove(&provider);
        self.persist()?;
        let name = env_var(provider);
        if lock(&self.inner.owned).remove(name) {
            // SAFETY / deliberate: see `apply`. Only a variable this process
            // set itself is cleared here; the user's own export is left alone.
            unsafe { std::env::remove_var(name) };
        }
        Ok(self.list())
    }

    fn persist(&self) -> Result<(), EngineError> {
        let store = StoreFile {
            keys: lock(&self.inner.entries).clone(),
        };
        let json = serde_json::to_vec_pretty(&store)
            .map_err(|e| EngineError::Other(format!("serialize api keys: {e}")))?;
        if let Some(dir) = self.inner.file.parent() {
            std::fs::create_dir_all(dir)?;
        }
        // `secret = true`: 0600 from birth on unix, single-ACE user-only ACL
        // on Windows.
        write_file_atomic(&self.inner.file, &json, true)
    }

    fn apply_all(&self) {
        for provider in ApiKeyProvider::ALL {
            self.apply(provider);
        }
    }

    /// Export one provider's key into THIS process's environment.
    ///
    /// Deliberate: the engine mutates its own environment so that every agent
    /// CLI it spawns later inherits the key, without the harness crate (or any
    /// per-harness launch spec) having to know that zeron stores keys at all.
    /// A variable the user already had set is never touched.
    fn apply(&self, provider: ApiKeyProvider) {
        let name = env_var(provider);
        if self.inner.foreign.contains(name) {
            return;
        }
        let Some(key) = lock(&self.inner.entries)
            .get(&provider)
            .map(|entry| entry.key.clone())
        else {
            return;
        };
        lock(&self.inner.owned).insert(name);
        // SAFETY: `set_var` is unsafe in edition 2024 because another thread
        // reading the environment concurrently is a data race. Every caller
        // here is the engine's own control path (startup assembly, or an RPC
        // handler on the engine runtime), and the only readers are the
        // `Command` spawns that snapshot the environment for a child process.
        // The store is the single writer, serialised by `entries`.
        unsafe { std::env::set_var(name, key) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_file(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("zeron-api-keys-{}-{name}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("api-keys.json")
    }

    #[test]
    fn masking_shows_the_provider_prefix_and_the_last_four() {
        assert_eq!(mask("sk-or-test-not-real-4f2a"), "sk-or-\u{2026}4f2a");
        assert_eq!(mask("sk-ant-api03-aaaabbbbcccc"), "sk-ant-\u{2026}cccc");
        assert_eq!(mask("gsk_aaaabbbbccccdddd"), "gsk_\u{2026}dddd");
        assert_eq!(mask("xai-aaaabbbbcccc"), "xai-\u{2026}cccc");
        // No separator in the first eight characters: fall back to four.
        assert_eq!(mask("AIzaSyAAAABBBBCCCC"), "AIza\u{2026}CCCC");
        // Nothing usable may leak out of a short key.
        assert_eq!(mask("sk-12345"), "\u{2022}\u{2022}\u{2022}\u{2022}\u{2022}\u{2022}\u{2022}\u{2022}");
        assert_eq!(mask(""), "\u{2022}\u{2022}\u{2022}");
        // The masked form never contains the middle of the secret.
        let secret = "sk-or-v1-SECRETMIDDLEPART-4f2a";
        assert!(!mask(secret).contains("SECRETMIDDLEPART"));
    }

    #[test]
    fn every_provider_maps_to_its_cli_environment_variable() {
        assert_eq!(env_var(ApiKeyProvider::Anthropic), "ANTHROPIC_API_KEY");
        assert_eq!(env_var(ApiKeyProvider::Openai), "OPENAI_API_KEY");
        assert_eq!(env_var(ApiKeyProvider::Openrouter), "OPENROUTER_API_KEY");
        assert_eq!(env_var(ApiKeyProvider::Deepseek), "DEEPSEEK_API_KEY");
        assert_eq!(env_var(ApiKeyProvider::Groq), "GROQ_API_KEY");
        assert_eq!(env_var(ApiKeyProvider::Xai), "XAI_API_KEY");
        assert_eq!(env_var(ApiKeyProvider::Moonshot), "MOONSHOT_API_KEY");
        assert_eq!(env_var(ApiKeyProvider::Google), "GEMINI_API_KEY");
        assert_eq!(env_var(ApiKeyProvider::Mistral), "MISTRAL_API_KEY");
        assert_eq!(env_var(ApiKeyProvider::Fireworks), "FIREWORKS_API_KEY");
        assert_eq!(env_var(ApiKeyProvider::Together), "TOGETHER_API_KEY");
        // Every variable name is distinct — one provider must never clobber
        // another's credential.
        let names: HashSet<&str> = ApiKeyProvider::ALL.into_iter().map(env_var).collect();
        assert_eq!(names.len(), ApiKeyProvider::ALL.len());
    }

    #[test]
    fn shape_hints_are_advice_not_a_gate() {
        assert!(shape_hint(ApiKeyProvider::Openrouter, "sk-or-abc").is_none());
        assert!(shape_hint(ApiKeyProvider::Openrouter, "gsk_abc").is_some());
        assert!(shape_hint(ApiKeyProvider::Groq, "gsk_abc").is_none());
        assert!(shape_hint(ApiKeyProvider::Xai, "xai-abc").is_none());
        assert!(shape_hint(ApiKeyProvider::Anthropic, "sk-ant-abc").is_none());
        // Opaque-key providers have nothing to check.
        assert!(shape_hint(ApiKeyProvider::Together, "whatever").is_none());
        // An empty field is the "nothing typed yet" state, not a wrong shape.
        assert!(shape_hint(ApiKeyProvider::Anthropic, "   ").is_none());
    }

    #[test]
    fn keys_round_trip_through_the_store_file_and_the_environment() {
        // TOGETHER_* and MISTRAL_* are used by no other test in this crate.
        let file = tmp_file("round-trip");
        let _ = std::fs::remove_file(&file);
        let keys = ApiKeys::open(file.clone());
        assert!(keys.list().keys.is_empty());

        let snapshot = keys
            .set(ApiKeyProvider::Together, "together-aaaabbbbcccc")
            .unwrap();
        assert_eq!(snapshot.keys.len(), 1);
        assert_eq!(snapshot.keys[0].provider, ApiKeyProvider::Together);
        assert_eq!(snapshot.keys[0].env_var, "TOGETHER_API_KEY");
        assert!(snapshot.keys[0].applied);
        // The snapshot carries the mask, never the secret.
        assert!(!snapshot.keys[0].masked.contains("aaaabbbb"));
        assert_eq!(
            std::env::var("TOGETHER_API_KEY").ok().as_deref(),
            Some("together-aaaabbbbcccc")
        );

        // The file on disk survives a reopen.
        let reopened = ApiKeys::open(file.clone());
        assert_eq!(reopened.list().keys.len(), 1);

        keys.remove(ApiKeyProvider::Together).unwrap();
        assert!(keys.list().keys.is_empty());
        assert!(std::env::var_os("TOGETHER_API_KEY").is_none());
        let _ = std::fs::remove_file(&file);
    }

    #[test]
    fn a_variable_the_user_already_set_is_never_overridden() {
        // MISTRAL_API_KEY is touched by no other test here.
        const NAME: &str = "MISTRAL_API_KEY";
        // SAFETY: test-local setup, same single-writer argument as `apply`.
        unsafe { std::env::set_var(NAME, "from-the-users-shell") };
        let file = tmp_file("foreign");
        let _ = std::fs::remove_file(&file);
        let keys = ApiKeys::open(file.clone());
        keys.set(ApiKeyProvider::Mistral, "zeron-stored-key").unwrap();
        assert_eq!(
            std::env::var(NAME).ok().as_deref(),
            Some("from-the-users-shell"),
            "the environment zeron was launched with wins"
        );
        // The row still shows, flagged as not applied, so the page can say why.
        let snapshot = keys.list();
        assert_eq!(snapshot.keys.len(), 1);
        assert!(!snapshot.keys[0].applied);
        // Removing our record must not clear the user's variable either.
        keys.remove(ApiKeyProvider::Mistral).unwrap();
        assert_eq!(
            std::env::var(NAME).ok().as_deref(),
            Some("from-the-users-shell")
        );
        unsafe { std::env::remove_var(NAME) };
        let _ = std::fs::remove_file(&file);
    }
}
