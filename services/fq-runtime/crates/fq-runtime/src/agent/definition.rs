//! Parser for Markdown agent definition files with YAML frontmatter.
//!
//! See ADR-0005 for the format specification. The parser produces a
//! validated [`Agent`] via the fluent builder — the intermediate
//! deserialisation types are private to this module.

use std::collections::HashMap;

use serde::Deserialize;

use super::{Agent, BuildError, McpServerDeclaration, Sandbox};

/// Parse an agent definition from the raw Markdown content.
///
/// The content must begin with a YAML frontmatter block delimited by `---`
/// lines, followed by the system prompt in Markdown.
pub fn parse_agent(content: &str) -> Result<Agent, ParseError> {
    let (frontmatter_str, body) = split_frontmatter(content)?;
    let frontmatter: Frontmatter = serde_yaml::from_str(frontmatter_str)?;

    let mut sandbox = Sandbox::new();
    for path in frontmatter.sandbox.fs_read {
        sandbox = sandbox.fs_read(path);
    }
    for path in frontmatter.sandbox.fs_write {
        sandbox = sandbox.fs_write(path);
    }
    for pattern in frontmatter.sandbox.network {
        sandbox = sandbox.network(pattern);
    }
    for var in frontmatter.sandbox.env {
        sandbox = sandbox.env(var);
    }
    for path in frontmatter.sandbox.exec_cwd {
        sandbox = sandbox.exec_cwd(path);
    }

    let mcp_servers: Vec<McpServerDeclaration> = frontmatter
        .mcp
        .into_iter()
        .map(|m| McpServerDeclaration {
            server: m.server,
            command: m.command,
            args: m.args,
            env: m.env.into_iter().collect(),
        })
        .collect();

    let mut builder = Agent::builder()
        .id(frontmatter.name)
        .model(frontmatter.model)
        .system_prompt(body)
        .tools(frontmatter.tools)
        .sandbox(sandbox)
        .mcp_servers(mcp_servers);

    if let Some(budget) = frontmatter.budget {
        builder = builder.budget(budget);
    }
    if let Some(trigger) = frontmatter.trigger {
        builder = builder.trigger(trigger);
    }

    Ok(builder.build()?)
}

/// YAML frontmatter structure. Private to this module — callers work with
/// [`Agent`] directly.
#[derive(Debug, Deserialize)]
struct Frontmatter {
    name: String,
    model: String,
    #[serde(default)]
    tools: Vec<String>,
    #[serde(default)]
    sandbox: SandboxFrontmatter,
    budget: Option<f64>,
    trigger: Option<String>,
    #[serde(default)]
    mcp: Vec<McpFrontmatter>,
}

#[derive(Debug, Deserialize)]
struct McpFrontmatter {
    server: String,
    command: String,
    #[serde(default)]
    args: Vec<String>,
    #[serde(default)]
    env: HashMap<String, String>,
}

#[derive(Debug, Default, Deserialize)]
struct SandboxFrontmatter {
    #[serde(default)]
    fs_read: Vec<String>,
    #[serde(default)]
    fs_write: Vec<String>,
    #[serde(default)]
    network: Vec<String>,
    #[serde(default)]
    env: Vec<String>,
    #[serde(default)]
    exec_cwd: Vec<String>,
}

fn split_frontmatter(content: &str) -> Result<(&str, &str), ParseError> {
    let content = content.trim_start();
    if !content.starts_with("---") {
        return Err(ParseError::MissingFrontmatter);
    }
    let after_opening = &content[3..];
    let closing = after_opening
        .find("\n---")
        .ok_or(ParseError::MissingFrontmatter)?;
    let frontmatter = &after_opening[..closing];
    let body = &after_opening[closing + 4..];
    Ok((frontmatter.trim(), body.trim()))
}

#[derive(Debug, thiserror::Error)]
pub enum ParseError {
    #[error("missing or malformed YAML frontmatter")]
    MissingFrontmatter,
    #[error("invalid YAML: {0}")]
    InvalidYaml(#[from] serde_yaml::Error),
    #[error("invalid agent: {0}")]
    InvalidAgent(#[from] BuildError),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_full_definition() {
        let content = r#"---
name: researcher
model: claude-haiku
tools:
  - read
  - web_search
sandbox:
  fs_read:
    - /project/docs
  network:
    - "*.api.internal"
budget: 0.50
trigger: tasks.research.*
---

You are a research agent.

## Guidelines

- Cite your sources.
"#;
        let agent = parse_agent(content).unwrap();
        assert_eq!(agent.id().as_str(), "researcher");
        assert_eq!(agent.model(), "claude-haiku");
        assert_eq!(agent.tools(), &["read", "web_search"]);
        assert_eq!(agent.budget(), Some(0.50));
        assert_eq!(agent.trigger(), Some("tasks.research.*"));
        assert_eq!(
            agent.sandbox().fs_read_paths(),
            &["/project/docs".to_string()]
        );
        assert!(agent.system_prompt().contains("You are a research agent"));
    }

    #[test]
    fn parses_minimal_definition() {
        let content = r#"---
name: minimal
model: claude-haiku
---

Prompt body.
"#;
        let agent = parse_agent(content).unwrap();
        assert_eq!(agent.id().as_str(), "minimal");
        assert!(agent.tools().is_empty());
        assert!(agent.budget().is_none());
    }

    #[test]
    fn rejects_missing_frontmatter() {
        let content = "Just markdown without frontmatter.";
        assert!(matches!(
            parse_agent(content).unwrap_err(),
            ParseError::MissingFrontmatter
        ));
    }

    #[test]
    fn rejects_invalid_agent_id_from_frontmatter() {
        let content = r#"---
name: invalid.name
model: claude-haiku
---

Prompt.
"#;
        let err = parse_agent(content).unwrap_err();
        assert!(matches!(err, ParseError::InvalidAgent(BuildError::InvalidId(_))));
    }

    #[test]
    fn parses_exec_cwd_from_frontmatter() {
        let content = r#"---
name: shell-agent
model: claude-haiku-4-5
tools:
  - shell
sandbox:
  exec_cwd:
    - /tmp/fq-workspace
    - /var/lib/factor-q
---

Prompt.
"#;
        let agent = parse_agent(content).unwrap();
        assert_eq!(
            agent.sandbox().exec_cwd_paths(),
            &["/tmp/fq-workspace".to_string(), "/var/lib/factor-q".to_string()]
        );
    }

    #[test]
    fn round_trips_exec_cwd_into_tool_sandbox() {
        let content = r#"---
name: shell-agent
model: claude-haiku-4-5
tools:
  - shell
sandbox:
  exec_cwd:
    - /tmp/fq-workspace
---

Prompt.
"#;
        let agent = parse_agent(content).unwrap();
        let tool_sandbox = agent.sandbox().to_tool_sandbox();
        let prefixes = tool_sandbox.exec_cwd_prefixes();
        assert_eq!(prefixes.len(), 1);
        assert_eq!(
            prefixes[0],
            std::path::PathBuf::from("/tmp/fq-workspace")
        );
    }

    #[test]
    fn parses_full_sandbox_with_all_dimensions() {
        let content = r#"---
name: inspector
model: claude-haiku-4-5
tools:
  - file_read
  - file_write
  - shell
sandbox:
  fs_read:
    - /tmp/readable
  fs_write:
    - /tmp/writable
  network:
    - "*.example.com"
  env:
    - HOME
    - PATH
  exec_cwd:
    - /tmp/workspace
---

Prompt.
"#;
        let agent = parse_agent(content).unwrap();
        let sb = agent.sandbox();
        assert_eq!(sb.fs_read_paths(), &["/tmp/readable".to_string()]);
        assert_eq!(sb.fs_write_paths(), &["/tmp/writable".to_string()]);
        assert_eq!(sb.network_patterns(), &["*.example.com".to_string()]);
        assert_eq!(sb.env_vars(), &["HOME".to_string(), "PATH".to_string()]);
        assert_eq!(sb.exec_cwd_paths(), &["/tmp/workspace".to_string()]);

        // And the round-trip to ToolSandbox preserves each
        // dimension separately.
        let ts = sb.to_tool_sandbox();
        assert_eq!(ts.read_prefixes().len(), 1);
        assert_eq!(ts.write_prefixes().len(), 1);
        assert_eq!(ts.exec_cwd_prefixes().len(), 1);
    }

    #[test]
    fn config_snapshot_includes_exec_cwd() {
        let content = r#"---
name: shell-agent
model: claude-haiku-4-5
tools:
  - shell
sandbox:
  exec_cwd:
    - /tmp/work
---

Prompt.
"#;
        let agent = parse_agent(content).unwrap();
        let snapshot = agent.to_snapshot();
        assert_eq!(snapshot.sandbox.exec_cwd, vec!["/tmp/work".to_string()]);
    }

    #[test]
    fn parses_mcp_from_frontmatter() {
        let content = r#"---
name: mcp-agent
model: claude-haiku
tools:
  - echo
mcp:
  - server: everything
    command: npx
    args:
      - "@modelcontextprotocol/server-everything"
  - server: custom
    command: my-server
    env:
      API_KEY: secret
---

You are a test agent.
"#;
        let agent = parse_agent(content).unwrap();
        assert_eq!(agent.mcp_servers().len(), 2);

        let first = &agent.mcp_servers()[0];
        assert_eq!(first.server, "everything");
        assert_eq!(first.command, "npx");
        assert_eq!(first.args, vec!["@modelcontextprotocol/server-everything"]);
        assert!(first.env.is_empty());

        let second = &agent.mcp_servers()[1];
        assert_eq!(second.server, "custom");
        assert_eq!(second.command, "my-server");
        assert!(second.args.is_empty());
        assert_eq!(second.env, vec![("API_KEY".to_string(), "secret".to_string())]);
    }

    #[test]
    fn agent_without_mcp_has_empty_servers() {
        let content = r#"---
name: basic
model: claude-haiku
---

Prompt.
"#;
        let agent = parse_agent(content).unwrap();
        assert!(agent.mcp_servers().is_empty());
    }

    #[test]
    fn rejects_negative_budget() {
        let content = r#"---
name: broken
model: claude-haiku
budget: -1.0
---

Prompt.
"#;
        let err = parse_agent(content).unwrap_err();
        assert!(matches!(
            err,
            ParseError::InvalidAgent(BuildError::InvalidBudget(_))
        ));
    }
}
