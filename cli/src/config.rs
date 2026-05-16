use serde::Deserialize;
use std::collections::HashMap;

#[derive(Debug, Clone, Deserialize, Default)]
pub struct Config {
    #[serde(default)]
    pub providers: ProvidersConfig,
    #[serde(default)]
    pub agent: AgentConfig,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct AgentConfig {
    #[serde(default = "default_model")]
    pub default_model: String,
    #[serde(default = "default_max_rounds")]
    pub max_tool_rounds: u32,
    #[serde(default)]
    pub soul_file: Option<String>,
}

fn default_model() -> String { "deepseek-v4-flash".to_string() }
fn default_max_rounds() -> u32 { 15 }

#[derive(Debug, Clone, Deserialize, Default)]
pub struct ProvidersConfig {
    pub openai: Option<OpenAIConfig>,
    pub anthropic: Option<AnthropicConfig>,
    pub nvidia: Option<NvidiaConfig>,
    #[serde(default)]
    pub generic: Vec<GenericConfig>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct OpenAIConfig {
    pub api_key: String,
    #[serde(default = "default_openai_base")]
    pub base_url: String,
    pub models: Vec<String>,
}

fn default_openai_base() -> String { "https://api.openai.com/v1".to_string() }

#[derive(Debug, Clone, Deserialize)]
pub struct AnthropicConfig {
    pub api_key: String,
    pub models: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct NvidiaConfig {
    pub api_key: String,
    pub models: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct GenericConfig {
    pub name: String,
    pub api_key: String,
    pub base_url: String,
    pub models: Vec<String>,
    #[serde(default)]
    pub headers: HashMap<String, String>,
}

pub struct ProviderConfig {
    pub base_url: String,
    pub api_key: String,
    pub model: String,
    pub is_anthropic: bool,
}

impl Config {
    pub fn load() -> Self {
        let path = dirs_config_path();
        let Ok(text) = std::fs::read_to_string(&path) else {
            return Self::default();
        };
        toml::from_str(&text).unwrap_or_default()
    }

    pub fn resolve_provider(&self, model_hint: Option<&str>) -> Option<ProviderConfig> {
        let model = model_hint.unwrap_or(&self.agent.default_model).to_string();

        // Check generic providers first (priority for custom setups like DeepSeek via openai compat)
        for g in &self.providers.generic {
            if g.models.iter().any(|m| m == &model) {
                return Some(ProviderConfig {
                    base_url: g.base_url.clone(),
                    api_key: g.api_key.clone(),
                    model,
                    is_anthropic: false,
                });
            }
        }

        // NVIDIA NIM
        if let Some(nv) = &self.providers.nvidia {
            if nv.models.iter().any(|m| m == &model) {
                return Some(ProviderConfig {
                    base_url: "https://integrate.api.nvidia.com/v1".to_string(),
                    api_key: nv.api_key.clone(),
                    model,
                    is_anthropic: false,
                });
            }
        }

        // Anthropic
        if let Some(ant) = &self.providers.anthropic {
            if ant.models.iter().any(|m| m == &model) {
                return Some(ProviderConfig {
                    base_url: "https://api.anthropic.com".to_string(),
                    api_key: ant.api_key.clone(),
                    model,
                    is_anthropic: true,
                });
            }
        }

        // OpenAI / OpenAI-compat (DeepSeek etc)
        if let Some(oa) = &self.providers.openai {
            if oa.models.iter().any(|m| m == &model) || model_hint.is_none() {
                return Some(ProviderConfig {
                    base_url: oa.base_url.clone(),
                    api_key: oa.api_key.clone(),
                    model: if oa.models.is_empty() { model } else {
                        model_hint.map(|s| s.to_string()).unwrap_or_else(|| oa.models[0].clone())
                    },
                    is_anthropic: false,
                });
            }
        }

        None
    }
}

fn dirs_config_path() -> std::path::PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
    std::path::PathBuf::from(home).join(".goblin").join("config.toml")
}
