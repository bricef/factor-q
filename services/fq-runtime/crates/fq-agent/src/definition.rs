//! Parser for Markdown agent definition files with YAML frontmatter.
//!
//! See ADR-0005 for the format specification. The parser produces a
//! validated [`Agent`] via the fluent builder — the intermediate
//! deserialisation types are private to this module.

use std::collections::HashMap;

use serde::Deserialize;

use fq_ops::events::Effort;

use crate::{
    Agent, BuildError, CapabilityValidation, ElicitationGrant, McpServerDeclaration, RootsGrant,
    SamplingGrant, Sandbox, StaticResourcePin,
};

/// Parse an agent definition from the raw Markdown content.
///
/// The content must begin with a YAML frontmatter block delimited by `---`
/// lines, followed by the system prompt in Markdown.
pub fn parse_agent(content: &str) -> Result<Agent, ParseError> {
    parse_agent_with_default(content, None)
}

/// Parse an agent definition, falling back to `default_model` when the
/// frontmatter omits `model:` (the worker default, ADR-0003). An agent
/// with neither an explicit model nor a default fails to build
/// (`BuildError::MissingField("model")`).
pub fn parse_agent_with_default(
    content: &str,
    default_model: Option<&str>,
) -> Result<Agent, ParseError> {
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
        .map(|m| {
            // A server is reached over a stdio command or a Streamable
            // HTTP url — exactly one.
            if m.command.is_some() == m.url.is_some() {
                return Err(ParseError::InvalidMcp(format!(
                    "mcp server '{}' must set exactly one of `command` or `url`",
                    m.server
                )));
            }
            Ok(McpServerDeclaration {
                server: m.server,
                command: m.command,
                args: m.args,
                env: m.env.into_iter().collect(),
                url: m.url,
            })
        })
        .collect::<Result<Vec<_>, ParseError>>()?;

    let static_resources = frontmatter
        .static_resources
        .iter()
        .map(|s| StaticResourcePin::parse(s))
        .collect::<Result<Vec<_>, _>>()?;

    let mut builder = Agent::builder()
        .id(frontmatter.name)
        .system_prompt(body)
        .tools(frontmatter.tools)
        .sandbox(sandbox)
        .mcp_servers(mcp_servers)
        .static_resources(static_resources);

    // Explicit `model:` wins; otherwise fall back to the worker default.
    // If neither is present, `build()` fails with a missing-model error.
    if let Some(model) = frontmatter
        .model
        .or_else(|| default_model.map(str::to_string))
    {
        builder = builder.model(model);
    }

    if let Some(budget) = frontmatter.budget {
        builder = builder.budget(budget);
    }
    if let Some(max_iterations) = frontmatter.max_iterations {
        builder = builder.max_iterations(max_iterations);
    }
    if let Some(effort) = frontmatter.effort {
        builder = builder.effort(effort);
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
///
/// `deny_unknown_fields` because the alternative is silence, and silence
/// here is expensive. Without it serde drops any key it does not
/// recognise, so `budgett:` for `budget:` left the agent with no budget
/// cap at all — and `fq agent validate` reported `✓ valid`, because the
/// only trace was a *missing* line in its output. ADR-0004 is "cost
/// controls from day one"; one transposed character defeated it, past a
/// tool that said the definition was fine (#514).
///
/// `sandboxx:` is the same shape with a security edge: the agent runs
/// with no grants rather than the ones its author wrote.
///
/// Legal here because this struct flattens nothing — the same reasoning,
/// and the same fix, as `ProviderConfig` in `fq-runtime`'s config, where
/// `api` was silently accepted for `api_shape`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Frontmatter {
    name: String,
    /// Optional: falls back to `agents.default_model` when omitted. A
    /// definition with neither fails to load.
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    tools: Vec<String>,
    #[serde(default)]
    sandbox: SandboxFrontmatter,
    budget: Option<f64>,
    /// Optional per-agent override for the per-invocation LLM-turn cap.
    /// Absent = fall back to the daemon config default.
    max_iterations: Option<u32>,
    effort: Option<Effort>,
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

/// A misspelled key here drops a server's transport, command or grants —
/// the declaration still loads, and the agent gets an MCP server that is
/// not the one its author described.
///
/// `deny_unknown_fields` for the same reason `Frontmatter` has it (#514):
/// silence is the expensive failure. Legal here because this struct
/// flattens nothing. #515 closed the top level only, so `mcp: [{ commandd:
/// … }]` still loaded — a server with no command at all — until #520.
///
/// One level deeper is still silent: a typo *inside* a `sampling` or
/// `elicitation` grant is swallowed by the untagged `Repr` that
/// `CapabilityGrant`'s `Deserialize` delegates to, which buffers content
/// the way `flatten` does. The attribute alone does not fix it — added to
/// `CapabilityValidation` it rejects the typo but reports "data did not
/// match any variant of untagged enum Repr", naming an internal type
/// instead of the bad key. `Repr` has to dispatch on the value explicitly
/// so the inner error survives (#526).
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct McpFrontmatter {
    server: String,
    #[serde(default)]
    command: Option<String>,
    #[serde(default)]
    url: Option<String>,
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

/// Every field here is `#[serde(default)]` over a `Vec<String>`, so a
/// misspelled key does not fail — it yields an *empty grant list*. The
/// author wrote a grant, the tool confirmed the definition, and the agent
/// silently cannot do the thing, surfacing later as a tool failure with no
/// connection to the typo that caused it.
///
/// `deny_unknown_fields` for the same reason `Frontmatter` has it (#514):
/// silence is the expensive failure. Legal here because this struct
/// flattens nothing. #515 closed the top level and left this one open —
/// `sandbox: { fs_writ: … }` was the more likely typo of the two, because
/// the nested keys are the ones an author actually edits (#520).
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
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
    #[error("invalid mcp server: {0}")]
    InvalidMcp(String),
}

#[cfg(test)]
mod tests {

    /// The defect #514 was filed for: a misspelled key was dropped in
    /// silence, so `budgett:` produced an agent with no budget cap and
    /// `fq agent validate` still said it was fine. The error must name
    /// the offending field — an operator who mistyped one character
    /// needs to be told which one.
    #[test]
    fn a_misspelled_key_is_rejected_and_named() {
        let err = parse_agent("---\nname: typo\nmodel: m\nbudgett: 0.05\n---\nBody.\n")
            .expect_err("an unknown key must not be silently dropped");
        let msg = err.to_string();
        assert!(
            msg.contains("budgett"),
            "the error must name the offending key, got: {msg}"
        );
        assert!(
            msg.contains("budget"),
            "the error should list the field that was meant, got: {msg}"
        );
    }

    /// The same trap with a security edge rather than a cost one: a
    /// dropped `sandbox` left the agent with no grants at all, not the
    /// ones its author wrote.
    #[test]
    fn a_misspelled_sandbox_key_is_rejected() {
        let err = parse_agent(
            "---\nname: typo\nmodel: m\nsandboxx:\n  fs_read: [\"/tmp\"]\n---\nBody.\n",
        )
        .expect_err("a misspelled sandbox key must not be silently dropped");
        assert!(err.to_string().contains("sandboxx"));
    }

    /// #515 closed the top level, which left the more likely typo open:
    /// the nested keys are the ones an author actually edits, and every
    /// one of them defaults to an empty `Vec`, so `fs_writ:` granted no
    /// write access at all while reporting a valid definition. The failure
    /// surfaced later as a permission denial with nothing to connect it
    /// back to the missing `e` (#520).
    #[test]
    fn a_misspelled_key_inside_sandbox_is_rejected_and_named() {
        let err =
            parse_agent("---\nname: typo\nmodel: m\nsandbox:\n  fs_writ: [\"/tmp\"]\n---\nBody.\n")
                .expect_err("a typo inside sandbox must not be silently dropped");
        let msg = err.to_string();
        assert!(
            msg.contains("fs_writ"),
            "the error must name the offending key, got: {msg}"
        );
        assert!(
            msg.contains("fs_write"),
            "the error should list the field that was meant, got: {msg}"
        );
    }

    /// The same one level down in `mcp`, where a dropped key means the
    /// server runs with a transport its author did not write — here, no
    /// command at all (#520).
    #[test]
    fn a_misspelled_key_inside_an_mcp_entry_is_rejected_and_named() {
        let err = parse_agent(
            "---\nname: typo\nmodel: m\nmcp:\n  - server: notes\n    commandd: notes-mcp\n---\nBody.\n",
        )
        .expect_err("a typo inside an mcp entry must not be silently dropped");
        let msg = err.to_string();
        assert!(
            msg.contains("commandd"),
            "the error must name the offending key, got: {msg}"
        );
        assert!(
            msg.contains("mcp"),
            "the error should locate which block it came from, got: {msg}"
        );
    }

    /// The other half of strictness: correctly-spelled nested blocks must
    /// still load. A `deny_unknown_fields` that also rejected valid input
    /// would be caught by the example definitions, but not before it had
    /// stopped a daemon from starting.
    #[test]
    fn correctly_spelled_nested_blocks_still_load() {
        let agent = parse_agent(
            "---\nname: nested\nmodel: m\nsandbox:\n  fs_read: [\"/tmp\"]\n  fs_write: [\"/out\"]\n  network: [\"api.example.com\"]\nmcp:\n  - server: notes\n    command: notes-mcp\n    args: [\"--fast\"]\n    roots: true\n---\nBody.\n",
        )
        .expect("valid nested blocks must still parse");
        assert_eq!(agent.sandbox().fs_read_paths(), ["/tmp"]);
        assert_eq!(agent.sandbox().fs_write_paths(), ["/out"]);
        assert_eq!(agent.sandbox().network_patterns(), ["api.example.com"]);
        assert_eq!(agent.mcp_servers().len(), 1);
    }

    /// Strictness must not cost us the fields that are genuinely
    /// optional — every one of them omitted is still a valid definition.
    #[test]
    fn every_optional_field_may_still_be_omitted() {
        let agent = parse_agent("---\nname: bare\nmodel: m\n---\nBody.\n")
            .expect("name and model alone are a valid definition");
        assert!(agent.budget().is_none());
        assert!(agent.max_iterations().is_none());
        assert!(agent.effort().is_none());
        assert!(agent.trigger().is_none());
        assert!(agent.mcp_servers().is_empty());
    }
    use super::*;

    #[test]
    fn omitted_model_falls_back_to_worker_default() {
        let content = "---\nname: triage\n---\n\nYou are an agent.\n";
        let agent = parse_agent_with_default(content, Some("claude-haiku-4-5"))
            .expect("should parse with the default applied");
        assert_eq!(agent.model(), "claude-haiku-4-5");
    }

    #[test]
    fn explicit_model_beats_the_worker_default() {
        let content = "---\nname: fixer\nmodel: claude-opus-4-8\n---\n\nYou are an agent.\n";
        let agent =
            parse_agent_with_default(content, Some("claude-haiku-4-5")).expect("should parse");
        assert_eq!(agent.model(), "claude-opus-4-8");
    }

    #[test]
    fn omitted_model_and_no_default_is_an_error() {
        let content = "---\nname: triage\n---\n\nYou are an agent.\n";
        assert!(
            parse_agent(content).is_err(),
            "a definition with no model and no default must fail to build"
        );
    }

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
    fn parses_reasoning_effort_from_frontmatter() {
        let agent =
            parse_agent("---\nname: test\nmodel: test-model\neffort: xhigh\n---\nprompt").unwrap();
        assert_eq!(agent.effort(), Some(Effort::XHigh));
    }

    #[test]
    fn absent_reasoning_effort_uses_provider_default() {
        let agent = parse_agent("---\nname: test\nmodel: test-model\n---\nprompt").unwrap();
        assert_eq!(agent.effort(), None);
    }

    #[test]
    fn parses_max_iterations_override_from_frontmatter() {
        let content = r#"---
name: bounded
model: claude-haiku
max_iterations: 250
---

Prompt body.
"#;
        let agent = parse_agent(content).unwrap();
        assert_eq!(agent.max_iterations(), Some(250));
    }

    #[test]
    fn max_iterations_absent_falls_back_to_none() {
        let content = r#"---
name: unbounded
model: claude-haiku
---

Prompt body.
"#;
        let agent = parse_agent(content).unwrap();
        assert!(agent.max_iterations().is_none());
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
name: exec-agent
model: claude-haiku-4-5
tools:
  - exec
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
name: exec-agent
model: claude-haiku-4-5
tools:
  - exec
sandbox:
  exec_cwd:
    - /tmp/fq-workspace
---

Prompt.
"#;
        let agent = parse_agent(content).unwrap();
        let tool_sandbox = agent.sandbox().to_tool_sandbox(None).unwrap();
        let prefixes = tool_sandbox.exec_cwd_prefixes();
        assert_eq!(prefixes.len(), 1);
        assert_eq!(prefixes[0], std::path::PathBuf::from("/tmp/fq-workspace"));
    }

    #[test]
    fn workspace_token_binds_in_sandbox_paths() {
        let content = r#"---
name: bound
model: claude-haiku-4-5
tools:
  - exec
sandbox:
  fs_read:
    - ${workspace}
  fs_write:
    - ${workspace}/out
  exec_cwd:
    - ${workspace}
---

Prompt.
"#;
        let agent = parse_agent(content).unwrap();
        let ws = std::path::Path::new("/wt/0198");
        let ts = agent.sandbox().to_tool_sandbox(Some(ws)).unwrap();
        assert_eq!(ts.read_prefixes(), &[std::path::PathBuf::from("/wt/0198")]);
        assert_eq!(
            ts.write_prefixes(),
            &[std::path::PathBuf::from("/wt/0198/out")]
        );
        assert_eq!(
            ts.exec_cwd_prefixes(),
            &[std::path::PathBuf::from("/wt/0198")]
        );
    }

    #[test]
    fn sandbox_env_names_flow_through_to_tool_sandbox() {
        // Issue #34: env-var grants must reach ToolSandbox so the exec
        // tool can pass them through to a child (they are names, not
        // paths — no workspace binding).
        let content = r#"---
name: env-agent
model: claude-haiku-4-5
tools:
  - exec
sandbox:
  exec_cwd:
    - /work
  env:
    - HOME
    - GH_TOKEN
---

Prompt.
"#;
        let agent = parse_agent(content).unwrap();
        let ts = agent.sandbox().to_tool_sandbox(None).unwrap();
        assert_eq!(
            ts.env_allowlist(),
            &["HOME".to_string(), "GH_TOKEN".to_string()],
            "sandbox.env names must be carried onto ToolSandbox"
        );
    }

    #[test]
    fn network_declaration_loads_and_is_reported_unenforced() {
        // Issue #35: `sandbox.network` is parsed but nothing enforces it.
        // Both halves of that decision are asserted here:
        //   1. declaring it is NOT a load error — definitions record
        //      intent ahead of enforcement, and rejecting them would
        //      break every fleet agent (they all declare hosts);
        //   2. it is reported as the no-op it currently is, so the load
        //      path can warn instead of silently honouring nothing.
        let content = r#"---
name: network-agent
model: claude-haiku-4-5
tools:
  - exec
sandbox:
  exec_cwd:
    - /work
  network:
    - github.com
    - api.github.com
---

Prompt.
"#;
        let agent = parse_agent(content)
            .expect("a definition declaring sandbox.network must still load (#35)");
        assert_eq!(
            agent.sandbox().unenforced_network(),
            Some(&["github.com".to_string(), "api.github.com".to_string()][..]),
            "a non-empty sandbox.network must be reported as declared-but-unenforced"
        );
    }

    #[test]
    fn absent_network_declaration_reports_nothing() {
        // Nothing declared, nothing to warn about — the warning must not
        // fire for the agents that never asked for network in the first
        // place (#35).
        let content = r#"---
name: quiet-agent
model: claude-haiku-4-5
tools:
  - exec
sandbox:
  exec_cwd:
    - /work
---

Prompt.
"#;
        let agent = parse_agent(content).unwrap();
        assert!(
            agent.sandbox().unenforced_network().is_none(),
            "an absent sandbox.network must not be reported as unenforced"
        );
    }

    #[test]
    fn workspace_token_without_binding_fails_loud() {
        let content = r#"---
name: unbound
model: claude-haiku-4-5
tools:
  - exec
sandbox:
  exec_cwd:
    - ${workspace}
---

Prompt.
"#;
        let agent = parse_agent(content).unwrap();
        let err = agent.sandbox().to_tool_sandbox(None).unwrap_err();
        assert!(err.to_string().contains("${workspace}"));
    }

    #[test]
    fn parses_full_sandbox_with_all_dimensions() {
        let content = r#"---
name: inspector
model: claude-haiku-4-5
tools:
  - file_read
  - file_write
  - exec
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
        let ts = sb.to_tool_sandbox(None).unwrap();
        assert_eq!(ts.read_prefixes().len(), 1);
        assert_eq!(ts.write_prefixes().len(), 1);
        assert_eq!(ts.exec_cwd_prefixes().len(), 1);
    }

    #[test]
    fn config_snapshot_includes_exec_cwd() {
        let content = r#"---
name: exec-agent
model: claude-haiku-4-5
tools:
  - exec
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
        assert_eq!(first.command.as_deref(), Some("npx"));
        assert_eq!(first.args, vec!["@modelcontextprotocol/server-everything"]);
        assert!(first.env.is_empty());

        let second = &agent.mcp_servers()[1];
        assert_eq!(second.server, "custom");
        assert_eq!(second.command.as_deref(), Some("my-server"));
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

    #[test]
    fn parses_a_streamable_http_mcp_server() {
        let content = r#"---
name: http-agent
model: claude-haiku
mcp:
  - server: remote
    url: http://127.0.0.1:8000/mcp
---

You are a test agent.
"#;
        let agent = parse_agent(content).unwrap();
        let server = &agent.mcp_servers()[0];
        assert_eq!(server.url.as_deref(), Some("http://127.0.0.1:8000/mcp"));
        assert!(server.command.is_none());
    }

    #[test]
    fn rejects_mcp_server_with_neither_command_nor_url() {
        let content = r#"---
name: bad-agent
model: claude-haiku
mcp:
  - server: nope
---

You are a test agent.
"#;
        assert!(matches!(
            parse_agent(content),
            Err(ParseError::InvalidMcp(_))
        ));
    }

    #[test]
    fn rejects_mcp_server_with_both_command_and_url() {
        let content = r#"---
name: bad-agent
model: claude-haiku
mcp:
  - server: nope
    command: npx
    url: http://127.0.0.1:8000/mcp
---

You are a test agent.
"#;
        assert!(matches!(
            parse_agent(content),
            Err(ParseError::InvalidMcp(_))
        ));
    }
}
