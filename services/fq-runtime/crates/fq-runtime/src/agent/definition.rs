//! Parser for Markdown agent definition files with YAML frontmatter.
//!
//! See ADR-0005 for the format specification. The parser produces a
//! validated [`Agent`] via the fluent builder — the intermediate
//! deserialisation types are private to this module.

use std::collections::HashMap;

use serde::Deserialize;

use super::{
    Agent, BuildError, CapabilityValidation, ElicitationGrant, McpServerDeclaration, RootsGrant,
    SamplingGrant, Sandbox, StaticResourcePin,
};

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

    // Aggregate the per-server capability flags into agent-level
    // grants (the sub-budget is aggregate, declared at the top level).
    // Computed before `mcp` is consumed into declarations below.
    let servers_granting = |pick: fn(&McpFrontmatter) -> bool| -> Vec<String> {
        frontmatter
            .mcp
            .iter()
            .filter(|m| pick(m))
            .map(|m| m.server.clone())
            .collect()
    };
    let sampling_servers = servers_granting(|m| m.sampling.is_granted());
    let elicitation_servers = servers_granting(|m| m.elicitation.is_granted());
    let roots_servers = servers_granting(|m| m.roots);

    // Aggregate each capability's per-server validation policy (a server
    // may declare `sampling:` / `elicitation:` as a table). v1 unions
    // across granting servers; per-server policy is a follow-up with the
    // multi-server work.
    let mut sampling_validation = CapabilityValidation::default();
    let mut elicitation_validation = CapabilityValidation::default();
    for m in frontmatter.mcp.iter() {
        if let Some(cv) = m.sampling.validation() {
            merge_validation(&mut sampling_validation, cv);
        }
        if let Some(cv) = m.elicitation.validation() {
            merge_validation(&mut elicitation_validation, cv);
        }
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

    let static_resources = frontmatter
        .static_resources
        .iter()
        .map(|s| StaticResourcePin::parse(s))
        .collect::<Result<Vec<_>, _>>()?;

    let mut builder = Agent::builder()
        .id(frontmatter.name)
        .model(frontmatter.model)
        .system_prompt(body)
        .tools(frontmatter.tools)
        .sandbox(sandbox)
        .mcp_servers(mcp_servers)
        .static_resources(static_resources);

    if let Some(budget) = frontmatter.budget {
        builder = builder.budget(budget);
    }
    if let Some(trigger) = frontmatter.trigger {
        builder = builder.trigger(trigger);
    }
    if !sampling_servers.is_empty() {
        builder = builder.sampling_grant(SamplingGrant {
            servers: sampling_servers,
            max_cost: frontmatter.sampling_budget,
        });
    }
    if !elicitation_servers.is_empty() {
        builder = builder.elicitation_grant(ElicitationGrant {
            servers: elicitation_servers,
            max_cost: frontmatter.elicitation_budget,
        });
    }
    if !roots_servers.is_empty() {
        builder = builder.roots_grant(RootsGrant {
            servers: roots_servers,
        });
    }
    if !sampling_validation.is_empty() {
        builder = builder.sampling_validation(sampling_validation);
    }
    if !elicitation_validation.is_empty() {
        builder = builder.elicitation_validation(elicitation_validation);
    }

    Ok(builder.build()?)
}

/// Merge `from` into `into` (union): any redaction flag set wins, and
/// evaluator lists concatenate in declaration order.
fn merge_validation(into: &mut CapabilityValidation, from: &CapabilityValidation) {
    into.redact_secrets |= from.redact_secrets;
    into.reject_sensitive_fields |= from.reject_sensitive_fields;
    into.input_validation
        .extend(from.input_validation.iter().cloned());
    into.output_validation
        .extend(from.output_validation.iter().cloned());
}

/// A per-server capability flag in frontmatter: either a bare bool
/// (`sampling: true`) or a validation table
/// (`sampling: { redact_secrets: true, output_validation: [...] }`). A
/// table — or `true` — grants the capability; absent or `false` does
/// not. (Roots take only a bool — no validation policy.)
#[derive(Debug, Clone, Default)]
enum CapabilityGrant {
    /// Not granted (absent or `false`).
    #[default]
    Off,
    /// Granted with the default allow-everything validation seam (`true`).
    On,
    /// Granted with an explicit validation policy (a table).
    Configured(CapabilityValidation),
}

impl CapabilityGrant {
    fn is_granted(&self) -> bool {
        !matches!(self, CapabilityGrant::Off)
    }

    fn validation(&self) -> Option<&CapabilityValidation> {
        match self {
            CapabilityGrant::Configured(cv) => Some(cv),
            _ => None,
        }
    }
}

impl<'de> Deserialize<'de> for CapabilityGrant {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        // A bare bool, or a validation table.
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Repr {
            Flag(bool),
            Config(CapabilityValidation),
        }
        Ok(match Repr::deserialize(deserializer)? {
            Repr::Flag(false) => CapabilityGrant::Off,
            Repr::Flag(true) => CapabilityGrant::On,
            Repr::Config(cv) => CapabilityGrant::Configured(cv),
        })
    }
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
    #[serde(default)]
    static_resources: Vec<String>,
    /// Aggregate sampling sub-budget (USD) across all servers granted
    /// `sampling`, enforced per invocation. `None` = bounded only by
    /// the invocation `budget` (ADR-0017 / ADR-0018).
    sampling_budget: Option<f64>,
    /// Aggregate elicitation sub-budget (USD), same semantics.
    elicitation_budget: Option<f64>,
}

#[derive(Debug, Deserialize)]
struct McpFrontmatter {
    server: String,
    command: String,
    #[serde(default)]
    args: Vec<String>,
    #[serde(default)]
    env: HashMap<String, String>,
    /// Per-server capability grants (ADR-0017, nothing by default):
    /// may this server request sampling / elicitation, and are the
    /// agent's workspace roots advertised to it?
    #[serde(default)]
    sampling: CapabilityGrant,
    #[serde(default)]
    elicitation: CapabilityGrant,
    #[serde(default)]
    roots: bool,
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
        assert!(matches!(
            err,
            ParseError::InvalidAgent(BuildError::InvalidId(_))
        ));
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
            &[
                "/tmp/fq-workspace".to_string(),
                "/var/lib/factor-q".to_string()
            ]
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
        assert_eq!(prefixes[0], std::path::PathBuf::from("/tmp/fq-workspace"));
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
        assert_eq!(
            second.env,
            vec![("API_KEY".to_string(), "secret".to_string())]
        );
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
    fn parses_static_resources() {
        let content = r#"---
name: pinned
model: claude-haiku
mcp:
  - server: everything
    command: npx
    args:
      - "@modelcontextprotocol/server-everything"
static_resources:
  - "mcp://everything/test://static/resource/1"
---

Prompt.
"#;
        let agent = parse_agent(content).unwrap();
        assert_eq!(agent.static_resources().len(), 1);
        let pin = &agent.static_resources()[0];
        assert_eq!(pin.server, "everything");
        assert_eq!(pin.uri, "test://static/resource/1");
    }

    #[test]
    fn rejects_malformed_static_resource() {
        let content = r#"---
name: broken
model: claude-haiku
static_resources:
  - "not-a-pin"
---

Prompt.
"#;
        let err = parse_agent(content).unwrap_err();
        assert!(matches!(
            err,
            ParseError::InvalidAgent(BuildError::InvalidStaticResource(_))
        ));
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

    #[test]
    fn parses_capability_grants_from_frontmatter() {
        let content = r#"---
name: granting-agent
model: claude-haiku
sampling_budget: 0.50
elicitation_budget: 0.25
mcp:
  - server: everything
    command: npx
    sampling: true
    elicitation: true
    roots: true
  - server: tools-only
    command: other
---

You are a test agent.
"#;
        let agent = parse_agent(content).unwrap();

        let sampling = agent.sampling_grant().expect("sampling granted");
        assert_eq!(sampling.servers, vec!["everything".to_string()]);
        assert_eq!(sampling.max_cost, Some(0.50));

        let elicitation = agent.elicitation_grant().expect("elicitation granted");
        assert_eq!(elicitation.servers, vec!["everything".to_string()]);
        assert_eq!(elicitation.max_cost, Some(0.25));

        let roots = agent.roots_grant().expect("roots granted");
        assert_eq!(roots.servers, vec!["everything".to_string()]);

        // The tools-only server is in none of the grants.
        assert!(!sampling.permits("tools-only"));
        assert!(!roots.permits("tools-only"));
    }

    #[test]
    fn parses_capability_validation_table() {
        let content = r#"---
name: validated-agent
model: claude-haiku
mcp:
  - server: everything
    command: npx
    sampling:
      redact_secrets: true
      output_validation: [approve_all, { llm: claude-haiku-4-5 }]
    elicitation:
      reject_sensitive_fields: true
      input_validation: [deny_all]
---

You are a validated agent.
"#;
        let agent = parse_agent(content).unwrap();

        // A validation table grants the capability, same as `true`.
        assert!(agent.sampling_grant().is_some());
        assert!(agent.elicitation_grant().is_some());

        let sv = agent.sampling_validation();
        assert!(sv.redact_secrets);
        assert_eq!(sv.output_validation.len(), 2);

        let ev = agent.elicitation_validation();
        assert!(ev.reject_sensitive_fields);
        assert_eq!(ev.input_validation.len(), 1);
    }

    #[test]
    fn no_capability_grants_by_default() {
        let content = r#"---
name: plain-agent
model: claude-haiku
mcp:
  - server: everything
    command: npx
---

You are a test agent.
"#;
        let agent = parse_agent(content).unwrap();
        assert!(agent.sampling_grant().is_none());
        assert!(agent.elicitation_grant().is_none());
        assert!(agent.roots_grant().is_none());
    }

    #[test]
    fn grants_round_trip_through_config_snapshot() {
        let content = r#"---
name: granting-agent
model: claude-haiku
sampling_budget: 0.50
mcp:
  - server: everything
    command: npx
    sampling: true
    roots: true
---

You are a test agent.
"#;
        let agent = parse_agent(content).unwrap();
        let snapshot = agent.to_snapshot();

        let sampling = snapshot.sampling.expect("snapshot captures sampling grant");
        assert_eq!(sampling.servers, vec!["everything".to_string()]);
        assert_eq!(sampling.max_cost, Some(0.50));
        assert_eq!(
            snapshot
                .roots
                .expect("snapshot captures roots grant")
                .servers,
            vec!["everything".to_string()]
        );
        assert!(
            snapshot.elicitation.is_none(),
            "no elicitation grant was declared"
        );
    }
}
