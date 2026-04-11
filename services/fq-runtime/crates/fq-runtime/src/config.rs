use serde::Deserialize;

#[derive(Debug, Deserialize)]
pub struct Config {
    pub nats: NatsConfig,
    pub agents: AgentsConfig,
    pub providers: ProvidersConfig,
}

#[derive(Debug, Deserialize)]
pub struct NatsConfig {
    pub url: String,
}

#[derive(Debug, Deserialize)]
pub struct AgentsConfig {
    pub directory: String,
}

#[derive(Debug, Deserialize)]
pub struct ProvidersConfig {
    pub anthropic: Option<AnthropicConfig>,
}

#[derive(Debug, Deserialize)]
pub struct AnthropicConfig {
    pub api_key_env: String,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            nats: NatsConfig {
                url: "nats://localhost:4222".to_string(),
            },
            agents: AgentsConfig {
                directory: "agents".to_string(),
            },
            providers: ProvidersConfig {
                anthropic: Some(AnthropicConfig {
                    api_key_env: "ANTHROPIC_API_KEY".to_string(),
                }),
            },
        }
    }
}
