use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    pub providers: ProvidersConfig,
    pub agent: AgentConfig,
    #[serde(default)]
    pub tools: ToolsConfig,
    #[serde(default)]
    pub memory: MemoryConfig,
    #[serde(default)]
    pub stt: SttConfig,
    #[serde(default)]
    pub tts: TtsConfig,
    #[serde(default)]
    pub mnemonics: MnemonicsConfig,
    #[serde(default)]
    pub mcp: McpConfig,
    #[serde(default)]
    pub channels: ChannelsConfig,
    #[serde(default)]
    pub http: HttpConfig,
}

/// Optional local HTTP API. Off by default. When enabled, binds an
/// axum server to `bind` (default 127.0.0.1:1789) and authenticates
/// every request with `Authorization: Bearer <token>`. Designed for
/// phone / second-laptop / cron access to the same agent that the
/// desktop window is driving.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HttpConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "default_http_bind")]
    pub bind: String,
    /// Shared secret. Empty string disables auth (refuses to start).
    #[serde(default)]
    pub token: String,
}

impl Default for HttpConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            bind: default_http_bind(),
            token: String::new(),
        }
    }
}

fn default_http_bind() -> String {
    "127.0.0.1:1789".to_string()
}

/// Outbound notification channels (Telegram first; more later). Each
/// channel is opt-in and fire-and-forget — a delivery failure prints a
/// stderr line and is otherwise invisible to the agent loop.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ChannelsConfig {
    #[serde(default)]
    pub telegram: TelegramConfig,
    #[serde(default)]
    pub webhook: WebhookConfig,
    #[serde(default)]
    pub whatsapp: WhatsappChannelConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct WhatsappChannelConfig {
    /// When true, Goblin auto-replies to incoming WA messages as Atakan.
    #[serde(default)]
    pub auto_reply: bool,
}

/// Generic JSON webhook sink. Every published event is POSTed as
/// `{"kind": "...", "text": "...", "ts": ...}` to `url`. Use this to
/// forward observations into Slack incoming-webhooks, Discord, a
/// claude-mem worker, or any in-house collector.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct WebhookConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub url: String,
    /// Optional `Authorization: Bearer <token>` header. Empty string
    /// means no Authorization header is sent at all.
    #[serde(default)]
    pub bearer_token: String,
    #[serde(default = "default_webhook_events")]
    pub events: Vec<String>,
}

fn default_webhook_events() -> Vec<String> {
    vec!["decision".to_string(), "tool".to_string(), "error".to_string()]
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct TelegramConfig {
    #[serde(default)]
    pub enabled: bool,
    /// Bot token from @BotFather. Required when `enabled = true`.
    #[serde(default)]
    pub bot_token: String,
    /// Destination chat id (your personal id, a group id, or a channel
    /// id). Negative ids are groups/channels.
    #[serde(default)]
    pub chat_id: String,
    /// Which event kinds to push. Default is just "decision" — i.e. the
    /// summary that round produces, not every tool call. Add "tool" or
    /// "error" if you want a louder feed.
    #[serde(default = "default_telegram_events")]
    pub events: Vec<String>,
}

fn default_telegram_events() -> Vec<String> {
    vec!["decision".to_string(), "error".to_string()]
}

/// Generic MCP server registration. Same shape as Claude Code's MCP
/// config so existing entries (`mcp_servers.<name>`) can be copy-pasted
/// into `[mcp.servers.<name>]`.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct McpConfig {
    #[serde(default)]
    pub servers: std::collections::HashMap<String, McpServerConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpServerConfig {
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: std::collections::HashMap<String, String>,
    #[serde(default = "default_true")]
    pub enabled: bool,
}

/// External `mnemonics` binary (cross-project semantic memory). When the
/// binary is reachable this gives the agent two extra tools
/// (`mnemonics_retrieve`, `mnemonics_ingest`) that complement Goblin's own
/// project-scoped memory at .goblin/memory.db.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MnemonicsConfig {
    #[serde(default = "default_mnemonics_enabled")]
    pub enabled: bool,
    #[serde(default = "default_mnemonics_binary")]
    pub binary: String,
    /// Namespace to use for ingests originating from this Goblin install.
    /// Reads are unfiltered by default so the agent can recall anything.
    #[serde(default = "default_mnemonics_ns")]
    pub default_ns: String,
}

impl Default for MnemonicsConfig {
    fn default() -> Self {
        Self {
            enabled: default_mnemonics_enabled(),
            binary: default_mnemonics_binary(),
            default_ns: default_mnemonics_ns(),
        }
    }
}

fn default_mnemonics_enabled() -> bool { true }
fn default_mnemonics_binary() -> String { "mnemonics".to_string() }
fn default_mnemonics_ns() -> String { "proj:goblin".to_string() }

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProvidersConfig {
    pub openai: Option<OpenAIConfig>,
    pub anthropic: Option<AnthropicConfig>,
    pub nvidia: Option<NvidiaConfig>,
    pub gemini: Option<GeminiConfig>,
    pub glm: Option<GlmConfig>,
    #[serde(default)]
    pub generic: Vec<GenericConfig>,
    #[serde(default)]
    pub auto_route: AutoRouteConfig,
    #[serde(default)]
    pub multi_agent: MultiAgentConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OpenAIConfig {
    pub api_key: String,
    #[serde(default)]
    pub key_pool: Vec<String>,
    #[serde(default = "default_openai_base")]
    pub base_url: String,
    pub models: Vec<String>,
}

fn default_openai_base() -> String {
    "https://api.deepseek.com/v1".to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AnthropicConfig {
    pub api_key: String,
    #[serde(default)]
    pub key_pool: Vec<String>,
    #[serde(default = "default_anthropic_base")]
    pub base_url: String,
}

fn default_anthropic_base() -> String {
    "https://api.anthropic.com/v1".to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NvidiaConfig {
    pub api_key: String,
    #[serde(default)]
    pub key_pool: Vec<String>,
    #[serde(default = "default_nvidia_base")]
    pub base_url: String,
}

fn default_nvidia_base() -> String {
    "https://integrate.api.nvidia.com/v1".to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GeminiConfig {
    pub api_key: String,
    #[serde(default)]
    pub key_pool: Vec<String>,
    #[serde(default = "default_gemini_base")]
    pub base_url: String,
    pub models: Vec<String>,
}

fn default_gemini_base() -> String {
    "https://generativelanguage.googleapis.com/v1beta".to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GlmConfig {
    pub api_key: String,
    #[serde(default)]
    pub key_pool: Vec<String>,
    #[serde(default = "default_glm_base")]
    pub base_url: String,
    pub models: Vec<String>,
}

fn default_glm_base() -> String {
    "https://open.bigmodel.cn/api/paas/v4".to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GenericConfig {
    pub name: String,
    pub api_key: String,
    #[serde(default)]
    pub key_pool: Vec<String>,
    pub base_url: String,
    pub models: Vec<String>,
    #[serde(default = "default_provider_type")]
    pub provider_type: String,
}

fn default_provider_type() -> String {
    "openai".to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AutoRouteConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "default_fast_model")]
    pub fast_model: String,
    #[serde(default = "default_strong_model")]
    pub strong_model: String,
    #[serde(default)]
    pub vision_model: Option<String>,
}

impl Default for AutoRouteConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            fast_model: default_fast_model(),
            strong_model: default_strong_model(),
            vision_model: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MultiAgentConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub agents: Vec<AgentProfile>,
    #[serde(default = "default_max_depth")]
    pub max_depth: u32,
    #[serde(default = "default_max_children")]
    pub max_children: u32,
}

impl Default for MultiAgentConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            agents: vec![],
            max_depth: default_max_depth(),
            max_children: default_max_children(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentProfile {
    pub name: String,
    pub description: String,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub provider: Option<String>,
    #[serde(default)]
    pub system_prompt: Option<String>,
    #[serde(default)]
    pub allowed_tools: Vec<String>,
    #[serde(default)]
    pub blocked_tools: Vec<String>,
    #[serde(default)]
    pub triggers: Vec<String>,
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default)]
    pub sandbox: bool,
}

fn default_max_depth() -> u32 { 3 }
fn default_max_children() -> u32 { 5 }
fn default_true() -> bool { true }

// Trigger-based multi-agent routing config — wired in config.toml but
// not yet consumed by the agent loop. Kept here so the routing path can
// be turned on by the loop without re-introducing the surface.
#[allow(dead_code)]
impl Config {
    pub fn route_to_agent(&self, user_message: &str) -> Option<&AgentProfile> {
        if !self.providers.multi_agent.enabled || self.providers.multi_agent.agents.is_empty() {
            return None;
        }

        let msg_lower = user_message.to_lowercase();

        // Find first matching agent by trigger keywords
        for agent in &self.providers.multi_agent.agents {
            if !agent.enabled {
                continue;
            }
            for trigger in &agent.triggers {
                if msg_lower.contains(&trigger.to_lowercase()) {
                    return Some(agent);
                }
            }
        }

        None
    }

    pub fn get_agent_profile(&self, name: &str) -> Option<&AgentProfile> {
        self.providers.multi_agent.agents.iter().find(|a| a.name == name)
    }
}

fn default_fast_model() -> String {
    "deepseek-v4-flash".to_string()
}

fn default_strong_model() -> String {
    "deepseek-v4-pro".to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentConfig {
    #[serde(default = "default_model")]
    pub default_model: String,
    #[serde(default)]
    pub max_turns: u32,
    #[serde(default = "default_max_tokens")]
    pub max_tokens: u32,
    #[serde(default)]
    pub temperature: f32,
    /// Messages to protect from context compression (default: 20)
    #[serde(default = "default_context_protect")]
    pub context_protect_last_n: usize,
    /// Hard message count limit before aggressive compression (default: 400)
    #[serde(default = "default_context_hard_limit")]
    pub context_hard_limit: usize,
    /// Target compression ratio [0-1] (default: 0.8)
    #[serde(default = "default_context_target_ratio")]
    pub context_target_ratio: f64,
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            default_model: default_model(),
            max_turns: 30,
            max_tokens: default_max_tokens(),
            temperature: 0.0,
            context_protect_last_n: default_context_protect(),
            context_hard_limit: default_context_hard_limit(),
            context_target_ratio: default_context_target_ratio(),
        }
    }
}

fn default_context_protect() -> usize { 20 }
fn default_context_hard_limit() -> usize { 400 }
fn default_context_target_ratio() -> f64 { 0.8 }

fn default_model() -> String {
    "deepseek-v4-pro".to_string()
}

fn default_max_tokens() -> u32 {
    // V4 reasoning models burn 100s of tokens on reasoning_content before
    // emitting visible content. 8192 truncates real answers with
    // finish_reason="length". 32768 leaves headroom for chain-of-thought.
    32768
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ToolsConfig {
    #[serde(default)]
    pub shell_enabled: bool,
    #[serde(default)]
    pub browser_enabled: bool,
    #[serde(default)]
    pub workdir: Option<String>,
    /// Optional regex allowlist for `bash` / `bash_background` commands.
    /// When non-empty, a command must match at least one entry or be
    /// rejected. Patterns are checked against the full command string
    /// (the same string that's passed to `bash -c`). Empty list means
    /// "no allowlist" (the current default — every command runs).
    #[serde(default)]
    pub shell_allowlist: Vec<String>,
    /// Regex blocklist. Always checked, even when the allowlist is
    /// empty. A match here is a hard reject. Use this to ban things
    /// like `rm -rf /`, `sudo`, or known-bad CI tokens regardless of
    /// what the allowlist says.
    #[serde(default)]
    pub shell_blocklist: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct MemoryConfig {
    #[serde(default)]
    pub max_observations: u32,
    #[serde(default)]
    pub auto_compact_days: u32,
    #[serde(default)]
    pub embedding: EmbeddingConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EmbeddingConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "default_embedding_provider")]
    pub provider: String,
    #[serde(default)]
    pub api_key: Option<String>,
    #[serde(default = "default_embedding_base")]
    pub base_url: String,
    #[serde(default = "default_embedding_model")]
    pub model: String,
}

impl Default for EmbeddingConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            provider: default_embedding_provider(),
            api_key: None,
            base_url: default_embedding_base(),
            model: default_embedding_model(),
        }
    }
}

fn default_embedding_provider() -> String { "openai".to_string() }
fn default_embedding_base() -> String { "https://api.openai.com/v1".to_string() }
fn default_embedding_model() -> String { "text-embedding-3-small".to_string() }

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SttConfig {
    #[serde(default = "default_stt_provider")]
    pub provider: String,
    #[serde(default)]
    pub api_key: Option<String>,
    #[serde(default = "default_stt_base_url")]
    pub base_url: String,
    #[serde(default = "default_stt_model")]
    pub model: String,
}

impl Default for SttConfig {
    fn default() -> Self {
        Self {
            provider: default_stt_provider(),
            api_key: None,
            base_url: default_stt_base_url(),
            model: default_stt_model(),
        }
    }
}

fn default_stt_provider() -> String {
    "none".to_string()
}

fn default_stt_base_url() -> String {
    "https://api.openai.com/v1".to_string()
}

fn default_stt_model() -> String {
    "whisper-1".to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TtsConfig {
    /// TTS provider: "macos" (default), "openai", "edge"
    #[serde(default = "default_tts_provider")]
    pub provider: String,
    #[serde(default)]
    pub api_key: Option<String>,
    #[serde(default = "default_tts_base_url")]
    pub base_url: String,
    /// Model for provider (openai: tts-1 or tts-1-hd, edge: en-US-AriaNeural)
    #[serde(default = "default_tts_model")]
    pub model: String,
    /// Voice name (openai: alloy, echo, fable, nova, onyx, shimmer)
    #[serde(default = "default_tts_voice")]
    pub voice: String,
}

impl Default for TtsConfig {
    fn default() -> Self {
        Self {
            provider: default_tts_provider(),
            api_key: None,
            base_url: default_tts_base_url(),
            model: default_tts_model(),
            voice: default_tts_voice(),
        }
    }
}

fn default_tts_provider() -> String { "macos".to_string() }
fn default_tts_base_url() -> String { "https://api.openai.com/v1".to_string() }
fn default_tts_model() -> String { "tts-1".to_string() }
fn default_tts_voice() -> String { "alloy".to_string() }

impl Config {
    pub fn load() -> Result<Self, String> {
        let mut paths: Vec<PathBuf> = vec![
            PathBuf::from("goblin.toml"),
            PathBuf::from("config.toml"),
        ];
        if let Some(dir) = dirs_next() {
            paths.insert(0, dir.join("config.toml"));
        }

        for path in &paths {
            if path.exists() {
                let content = fs::read_to_string(path)
                    .map_err(|e| format!("Failed to read config {:?}: {}", path, e))?;
                return toml::from_str(&content)
                    .map_err(|e| format!("Failed to parse config: {}", e));
            }
        }

        Ok(Config {
            providers: ProvidersConfig {
                openai: None,
                anthropic: None,
                nvidia: None,
                gemini: None,
                glm: None,
                generic: vec![],
                auto_route: AutoRouteConfig::default(),
                multi_agent: MultiAgentConfig::default(),
            },
            agent: AgentConfig::default(),
            tools: ToolsConfig::default(),
            memory: MemoryConfig::default(),
            stt: SttConfig::default(),
            tts: TtsConfig::default(),
            mnemonics: MnemonicsConfig::default(),
            mcp: McpConfig::default(),
            channels: ChannelsConfig::default(),
            http: HttpConfig::default(),
        })
    }

    pub fn provider_name(&self) -> &str {
        if self.providers.openai.is_some() { "openai" }
        else if self.providers.anthropic.is_some() { "anthropic" }
        else if self.providers.nvidia.is_some() { "nvidia" }
        else if self.providers.gemini.is_some() { "gemini" }
        else if self.providers.glm.is_some() { "glm" }
        else if let Some(g) = self.providers.generic.first() { &g.name }
        else { "none" }
    }

    #[allow(dead_code)]
    pub fn has_any_provider(&self) -> bool {
        self.providers.openai.is_some()
            || self.providers.anthropic.is_some()
            || self.providers.nvidia.is_some()
            || self.providers.gemini.is_some()
            || self.providers.glm.is_some()
            || !self.providers.generic.is_empty()
    }

    pub fn default_model(&self) -> &str {
        &self.agent.default_model
    }

    pub fn auto_route_model(&self, user_message: &str, has_image: bool) -> &str {
        if !self.providers.auto_route.enabled {
            return &self.agent.default_model;
        }

        if has_image {
            if let Some(ref vm) = self.providers.auto_route.vision_model {
                return vm;
            }
            // Fall back to strong model if no vision model configured
            return &self.providers.auto_route.strong_model;
        }

        let msg_lower = user_message.to_lowercase();
        let strong_keywords = [
            "analyze", "explain", "debug", "refactor", "architecture",
            "design", "review", "optimize", "implement", "migration",
            "complex", "security", "vulnerability", "race condition",
            "deadlock", "algorithm", "premortem", "eisenhower",
            "coordinate", "delegate", "multi_edit", "commit",
        ];

        let is_long = user_message.len() > 200;
        let has_strong_signal = strong_keywords.iter().any(|kw| msg_lower.contains(kw));

        if is_long || has_strong_signal {
            &self.providers.auto_route.strong_model
        } else {
            &self.providers.auto_route.fast_model
        }
    }

    #[allow(dead_code)]
    pub fn get_key_for_provider(&self, provider_name: &str, index: usize) -> Option<&str> {
        let pool: &[String] = match provider_name {
            "openai" => self.providers.openai.as_ref().map_or(&[], |c| &c.key_pool),
            "anthropic" => self.providers.anthropic.as_ref().map_or(&[], |c| &c.key_pool),
            "nvidia" => self.providers.nvidia.as_ref().map_or(&[], |c| &c.key_pool),
            "gemini" => self.providers.gemini.as_ref().map_or(&[], |c| &c.key_pool),
            "glm" => self.providers.glm.as_ref().map_or(&[], |c| &c.key_pool),
            _ => {
                for g in &self.providers.generic {
                    if g.name == provider_name {
                        return g.key_pool.get(index).map(|s| s.as_str());
                    }
                }
                &[]
            }
        };
        pool.get(index).map(|s| s.as_str())
    }

    #[allow(dead_code)]
    pub fn key_pool_size(&self, provider_name: &str) -> usize {
        match provider_name {
            "openai" => self.providers.openai.as_ref().map_or(0, |c| c.key_pool.len()),
            "anthropic" => self.providers.anthropic.as_ref().map_or(0, |c| c.key_pool.len()),
            "nvidia" => self.providers.nvidia.as_ref().map_or(0, |c| c.key_pool.len()),
            "gemini" => self.providers.gemini.as_ref().map_or(0, |c| c.key_pool.len()),
            "glm" => self.providers.glm.as_ref().map_or(0, |c| c.key_pool.len()),
            _ => {
                for g in &self.providers.generic {
                    if g.name == provider_name {
                        return g.key_pool.len();
                    }
                }
                0
            }
        }
    }

    pub fn config_path() -> PathBuf {
        if let Some(home) = std::env::var_os("HOME") {
            let mut p = PathBuf::from(home);
            p.push(".goblin");
            p.push("config.toml");
            return p;
        }
        PathBuf::from("config.toml")
    }
}

fn dirs_next() -> Option<PathBuf> {
    if let Some(home) = std::env::var_os("HOME") {
        let mut p = PathBuf::from(home);
        p.push(".goblin");
        return Some(p);
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_config() -> Config {
        Config {
            providers: ProvidersConfig {
                openai: Some(OpenAIConfig {
                    api_key: "sk-test".into(),
                    key_pool: vec!["key1".into(), "key2".into()],
                    base_url: "https://api.deepseek.com/v1".into(),
                    models: vec!["deepseek-v4-pro".into()],
                }),
                anthropic: None,
                nvidia: None,
                gemini: None,
                glm: None,
                generic: vec![],
                auto_route: AutoRouteConfig {
                    enabled: true,
                    fast_model: "deepseek-v4-flash".into(),
                    strong_model: "deepseek-v4-pro".into(),
                    vision_model: Some("llama-vision".into()),
                },
                multi_agent: MultiAgentConfig::default(),
            },
            agent: AgentConfig {
                default_model: "deepseek-v4-pro".into(),
                max_turns: 10,
                max_tokens: 8192,
                temperature: 0.0,
                context_protect_last_n: 20,
                context_hard_limit: 400,
                context_target_ratio: 0.8,
            },
            tools: ToolsConfig::default(),
            memory: MemoryConfig::default(),
            stt: SttConfig::default(),
            tts: TtsConfig::default(),
            mnemonics: MnemonicsConfig::default(),
            mcp: McpConfig::default(),
            channels: ChannelsConfig::default(),
            http: HttpConfig::default(),
        }
    }

    fn empty_config() -> Config {
        Config {
            providers: ProvidersConfig {
                openai: None, anthropic: None, nvidia: None, gemini: None, glm: None,
                generic: vec![],
                auto_route: AutoRouteConfig::default(),
                multi_agent: MultiAgentConfig::default(),
            },
            agent: AgentConfig::default(),
            tools: ToolsConfig::default(),
            memory: MemoryConfig::default(),
            stt: SttConfig::default(),
            tts: TtsConfig::default(),
            mnemonics: MnemonicsConfig::default(),
            mcp: McpConfig::default(),
            channels: ChannelsConfig::default(),
            http: HttpConfig::default(),
        }
    }

    #[test]
    fn has_any_provider_with_openai() {
        let cfg = test_config();
        assert!(cfg.has_any_provider());
    }

    #[test]
    fn has_any_provider_empty() {
        let cfg = empty_config();
        assert!(!cfg.has_any_provider());
    }

    #[test]
    fn has_any_provider_with_generic() {
        let mut cfg = empty_config();
        cfg.providers.generic = vec![GenericConfig {
            name: "ollama".into(),
            api_key: "k".into(),
            key_pool: vec![],
            base_url: "http://localhost:11434".into(),
            models: vec!["llama3".into()],
            provider_type: "openai".into(),
        }];
        assert!(cfg.has_any_provider());
    }

    #[test]
    fn provider_name_openai() {
        let cfg = test_config();
        assert_eq!(cfg.provider_name(), "openai");
    }

    #[test]
    fn provider_name_empty() {
        let cfg = empty_config();
        assert_eq!(cfg.provider_name(), "none");
    }

    #[test]
    fn auto_route_disabled_falls_back_to_default() {
        let mut cfg = test_config();
        cfg.providers.auto_route.enabled = false;
        let model = cfg.auto_route_model("analyze complex code refactor", false);
        assert_eq!(model, "deepseek-v4-pro"); // default model, not strong
    }

    #[test]
    fn auto_route_strong_keyword() {
        let cfg = test_config();
        let model = cfg.auto_route_model("please analyze this code", false);
        assert_eq!(model, "deepseek-v4-pro");
    }

    #[test]
    fn auto_route_long_message() {
        let cfg = test_config();
        let long = "x".repeat(201);
        let model = cfg.auto_route_model(&long, false);
        assert_eq!(model, "deepseek-v4-pro");
    }

    #[test]
    fn auto_route_short_simple() {
        let cfg = test_config();
        let model = cfg.auto_route_model("hello how are you", false);
        assert_eq!(model, "deepseek-v4-flash");
    }

    #[test]
    fn auto_route_vision_model() {
        let cfg = test_config();
        let model = cfg.auto_route_model("what is this image", true);
        assert_eq!(model, "llama-vision");
    }

    #[test]
    fn auto_route_vision_no_model_falls_to_strong() {
        let mut cfg = test_config();
        cfg.providers.auto_route.vision_model = None;
        let model = cfg.auto_route_model("describe image", true);
        assert_eq!(model, "deepseek-v4-pro");
    }

    #[test]
    fn get_key_for_provider_known() {
        let cfg = test_config();
        assert_eq!(cfg.get_key_for_provider("openai", 0), Some("key1"));
        assert_eq!(cfg.get_key_for_provider("openai", 1), Some("key2"));
        assert_eq!(cfg.get_key_for_provider("openai", 2), None);
    }

    #[test]
    fn get_key_for_provider_unknown() {
        let cfg = test_config();
        assert_eq!(cfg.get_key_for_provider("nonexistent", 0), None);
    }

    #[test]
    fn key_pool_size() {
        let cfg = test_config();
        assert_eq!(cfg.key_pool_size("openai"), 2);
        assert_eq!(cfg.key_pool_size("unknown"), 0);
    }

    #[test]
    fn default_model_returns_agent_default() {
        let cfg = test_config();
        assert_eq!(cfg.default_model(), "deepseek-v4-pro");
    }

    #[test]
    fn auto_route_premortem_triggers_strong() {
        let cfg = test_config();
        let model = cfg.auto_route_model("bu plan icin premortem yap", false);
        assert_eq!(model, "deepseek-v4-pro");
    }

    #[test]
    fn auto_route_eisenhower_triggers_strong() {
        let cfg = test_config();
        let model = cfg.auto_route_model("eisenhower matrisi cikar", false);
        assert_eq!(model, "deepseek-v4-pro");
    }

    #[test]
    fn auto_route_delegate_triggers_strong() {
        let cfg = test_config();
        let model = cfg.auto_route_model("delegate this task", false);
        assert_eq!(model, "deepseek-v4-pro");
    }

    #[test]
    fn auto_route_multi_edit_triggers_strong() {
        let cfg = test_config();
        let model = cfg.auto_route_model("multi_edit the files", false);
        assert_eq!(model, "deepseek-v4-pro");
    }

    #[test]
    fn auto_route_security_triggers_strong() {
        let cfg = test_config();
        let model = cfg.auto_route_model("check for security vulnerability", false);
        assert_eq!(model, "deepseek-v4-pro");
    }

    #[test]
    fn auto_route_case_insensitive() {
        let cfg = test_config();
        let model = cfg.auto_route_model("ANALYZE THIS CODE", false);
        assert_eq!(model, "deepseek-v4-pro");
    }
}
