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
}

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
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OpenAIConfig {
    pub api_key: String,
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
    #[serde(default = "default_anthropic_base")]
    pub base_url: String,
}

fn default_anthropic_base() -> String {
    "https://api.anthropic.com/v1".to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NvidiaConfig {
    pub api_key: String,
    #[serde(default = "default_nvidia_base")]
    pub base_url: String,
}

fn default_nvidia_base() -> String {
    "https://integrate.api.nvidia.com/v1".to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GeminiConfig {
    pub api_key: String,
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
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            default_model: default_model(),
            max_turns: 30,
            max_tokens: default_max_tokens(),
            temperature: 0.0,
        }
    }
}

fn default_model() -> String {
    "deepseek-v4-flash".to_string()
}

fn default_max_tokens() -> u32 {
    8192
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ToolsConfig {
    #[serde(default)]
    pub shell_enabled: bool,
    #[serde(default)]
    pub browser_enabled: bool,
    #[serde(default)]
    pub workdir: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct MemoryConfig {
    #[serde(default)]
    pub max_observations: u32,
    #[serde(default)]
    pub auto_compact_days: u32,
}

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
            },
            agent: AgentConfig::default(),
            tools: ToolsConfig::default(),
            memory: MemoryConfig::default(),
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
}

fn dirs_next() -> Option<PathBuf> {
    if let Some(home) = std::env::var_os("HOME") {
        let mut p = PathBuf::from(home);
        p.push(".goblin");
        return Some(p);
    }
    None
}
