use serde::Deserialize;

/// Parsed agent definition from a Markdown file with YAML frontmatter.
#[derive(Debug)]
pub struct AgentDefinition {
    pub frontmatter: AgentFrontmatter,
    pub system_prompt: String,
}

/// YAML frontmatter from an agent definition file.
#[derive(Debug, Deserialize)]
pub struct AgentFrontmatter {
    pub name: String,
    pub model: String,
    #[serde(default)]
    pub tools: Vec<String>,
    #[serde(default)]
    pub sandbox: SandboxConfig,
    pub budget: Option<f64>,
    pub trigger: Option<String>,
}

/// Sandbox configuration for an agent.
#[derive(Debug, Default, Deserialize)]
pub struct SandboxConfig {
    #[serde(default)]
    pub fs_read: Vec<String>,
    #[serde(default)]
    pub fs_write: Vec<String>,
    #[serde(default)]
    pub network: Vec<String>,
    #[serde(default)]
    pub env: Vec<String>,
}

impl AgentDefinition {
    /// Parse an agent definition from Markdown content with YAML frontmatter.
    pub fn parse(content: &str) -> Result<Self, AgentParseError> {
        let (frontmatter_str, body) = split_frontmatter(content)?;
        let frontmatter: AgentFrontmatter =
            serde_yaml::from_str(frontmatter_str).map_err(AgentParseError::InvalidYaml)?;
        Ok(Self {
            frontmatter,
            system_prompt: body.to_string(),
        })
    }
}

fn split_frontmatter(content: &str) -> Result<(&str, &str), AgentParseError> {
    let content = content.trim_start();
    if !content.starts_with("---") {
        return Err(AgentParseError::MissingFrontmatter);
    }
    let after_opening = &content[3..];
    let closing = after_opening
        .find("\n---")
        .ok_or(AgentParseError::MissingFrontmatter)?;
    let frontmatter = &after_opening[..closing];
    let body = &after_opening[closing + 4..];
    Ok((frontmatter.trim(), body.trim()))
}

#[derive(Debug, thiserror::Error)]
pub enum AgentParseError {
    #[error("missing or malformed YAML frontmatter")]
    MissingFrontmatter,
    #[error("invalid YAML: {0}")]
    InvalidYaml(#[from] serde_yaml::Error),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_valid_definition() {
        let content = r#"---
name: test-agent
model: claude-haiku
tools:
  - read
  - shell
sandbox:
  fs_read:
    - /tmp/test
budget: 0.50
trigger: tasks.test.*
---

You are a test agent. Do test things.

## Guidelines

- Be thorough
"#;
        let def = AgentDefinition::parse(content).unwrap();
        assert_eq!(def.frontmatter.name, "test-agent");
        assert_eq!(def.frontmatter.model, "claude-haiku");
        assert_eq!(def.frontmatter.tools, vec!["read", "shell"]);
        assert_eq!(def.frontmatter.sandbox.fs_read, vec!["/tmp/test"]);
        assert_eq!(def.frontmatter.budget, Some(0.50));
        assert!(def.system_prompt.contains("You are a test agent"));
    }

    #[test]
    fn parse_missing_frontmatter() {
        let content = "Just some markdown without frontmatter";
        assert!(AgentDefinition::parse(content).is_err());
    }
}
