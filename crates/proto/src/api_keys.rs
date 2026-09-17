//! Provider API keys — the wire shape of Settings → Accounts → "API keys".
//!
//! Zeron itself never calls these providers. The keys exist so that the agent
//! CLIs zeron spawns (OpenCode, Pi, Grok, Kimi, Claude Code, Codex …) find the
//! credential they expect in their environment. The engine owns the store and
//! the process-environment injection; this module only carries the shapes.
//!
//! Nothing here ever carries a secret: [`StoredApiKey`] is the MASKED view the
//! settings page renders. The plaintext key travels exactly once, from the UI
//! to the engine, inside a `SetApiKey` request.

use serde::{Deserialize, Serialize};

/// A provider zeron can hold a key for. The set is fixed: every entry maps to
/// an environment variable an agent CLI is known to read.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ApiKeyProvider {
    Anthropic,
    Openai,
    Openrouter,
    Deepseek,
    Groq,
    Xai,
    /// Moonshot AI's direct API — the pay-as-you-go sibling of the Kimi Code
    /// CLI's OAuth login (which is an account, not a key).
    Moonshot,
    Google,
    Mistral,
    Fireworks,
    Together,
    Cline,
    Cloudflare,
    /// An OpenAI-compatible server reached over a custom base URL (Ollama,
    /// LM Studio, vLLM …). The stored value is that URL, not a secret.
    OllamaCompatible,
}

impl ApiKeyProvider {
    /// Display order of the "Add API key" list and of the stored-key rows.
    pub const ALL: [ApiKeyProvider; 14] = [
        ApiKeyProvider::Anthropic,
        ApiKeyProvider::Openai,
        ApiKeyProvider::Openrouter,
        ApiKeyProvider::Deepseek,
        ApiKeyProvider::Groq,
        ApiKeyProvider::Xai,
        ApiKeyProvider::Moonshot,
        ApiKeyProvider::Google,
        ApiKeyProvider::Mistral,
        ApiKeyProvider::Fireworks,
        ApiKeyProvider::Together,
        ApiKeyProvider::Cline,
        ApiKeyProvider::Cloudflare,
        ApiKeyProvider::OllamaCompatible,
    ];

    /// Human label (settings rows, dropdown).
    pub fn label(self) -> &'static str {
        match self {
            ApiKeyProvider::Anthropic => "Anthropic",
            ApiKeyProvider::Openai => "OpenAI",
            ApiKeyProvider::Openrouter => "OpenRouter",
            ApiKeyProvider::Deepseek => "DeepSeek",
            ApiKeyProvider::Groq => "Groq",
            ApiKeyProvider::Xai => "xAI",
            ApiKeyProvider::Moonshot => "Moonshot (Kimi API)",
            ApiKeyProvider::Google => "Google Gemini",
            ApiKeyProvider::Mistral => "Mistral",
            ApiKeyProvider::Fireworks => "Fireworks",
            ApiKeyProvider::Together => "Together",
            ApiKeyProvider::Cline => "Cline",
            ApiKeyProvider::Cloudflare => "Cloudflare AI Gateway",
            ApiKeyProvider::OllamaCompatible => "Ollama-compatible base URL",
        }
    }

    /// The value is a secret to be masked everywhere. False only for the
    /// OpenAI-compatible base URL, which is an address, not a credential.
    pub fn is_secret(self) -> bool {
        self != ApiKeyProvider::OllamaCompatible
    }
}

/// One stored key as the settings page sees it — masked, never plaintext.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StoredApiKey {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cloudflare: Option<CloudflareGateway>,
    pub provider: ApiKeyProvider,
    /// `sk-or-…4f2a`: enough to recognise which key this is, not enough to use it.
    pub masked: String,
    /// The environment variable the engine exports for this provider.
    pub env_var: String,
    /// False when the same variable was already set in the real environment
    /// zeron was launched with — that one wins and zeron does not override it.
    #[serde(default)]
    pub applied: bool,
    /// Epoch millis of the last save.
    pub updated_at: i64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CloudflareGateway {
    pub account_id: String,
    pub gateway_id: String,
}

/// `ListApiKeys` / `SetApiKey` / `RemoveApiKey` reply.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ApiKeysSnapshot {
    pub keys: Vec<StoredApiKey>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_ids_are_kebab_case_on_the_wire() {
        assert_eq!(
            serde_json::to_value(ApiKeyProvider::Openrouter).unwrap(),
            serde_json::json!("openrouter")
        );
        assert_eq!(
            serde_json::to_value(ApiKeyProvider::OllamaCompatible).unwrap(),
            serde_json::json!("ollama-compatible")
        );
        assert_eq!(
            serde_json::from_value::<ApiKeyProvider>(serde_json::json!("xai")).unwrap(),
            ApiKeyProvider::Xai
        );
    }

    #[test]
    fn stored_key_contract_is_camel_case() {
        let snapshot = ApiKeysSnapshot {
            keys: vec![StoredApiKey {
                provider: ApiKeyProvider::Openrouter,
                cloudflare: None,
                masked: "sk-or-\u{2026}4f2a".into(),
                env_var: "OPENROUTER_API_KEY".into(),
                applied: true,
                updated_at: 42,
            }],
        };
        let value = serde_json::to_value(&snapshot).unwrap();
        assert_eq!(value["keys"][0]["envVar"], "OPENROUTER_API_KEY");
        assert_eq!(value["keys"][0]["updatedAt"], 42);
        assert_eq!(
            serde_json::from_value::<ApiKeysSnapshot>(value).unwrap(),
            snapshot
        );
    }

    #[test]
    fn only_the_custom_base_url_is_not_a_secret() {
        for provider in ApiKeyProvider::ALL {
            assert_eq!(
                provider.is_secret(),
                provider != ApiKeyProvider::OllamaCompatible,
                "{provider:?}"
            );
        }
    }
}
