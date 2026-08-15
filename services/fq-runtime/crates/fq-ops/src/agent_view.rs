//! The agent roster, as the operator surface declares it.
//!
//! Shapes only. Projecting a loaded definition or a registry snapshot
//! into these needs the registry itself, which is `fq-runtime`'s, so
//! those conversions stay there.

use serde::{Deserialize, Serialize};

/// One agent definition in the daemon's live registry — the summary
/// row behind the dashboard's agents list and the Agent view's loaded
/// index row.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, schemars::JsonSchema)]
pub struct AgentSummaryView {
    pub agent_id: String,
    pub model: String,
    pub budget: Option<f64>,
    /// The NATS trigger suffix the agent listens on, if any.
    pub trigger: Option<String>,
    pub tool_count: i64,
    /// Size of the system prompt, so the list hints at definition
    /// weight without shipping every prompt on every refresh.
    pub prompt_bytes: i64,
    /// The definition file this agent was loaded from. On the summary
    /// as well as the detail because "which file is this?" is the
    /// question a listing gets asked next, and answering it from the
    /// index costs one string per row instead of one Get per agent.
    pub path: String,
}

/// One row of the Agent view's index (`agent.list`): a definition file
/// the daemon read, and what became of it.
///
/// A registry snapshot is not only its agents. A file that failed to
/// parse is usually the most operationally interesting thing in the
/// directory — the agent someone expects to be running and is not —
/// and it has no agent id to be listed under, so it rides the index as
/// its own kind of row. Dropping it would make the listing lie by
/// omission, and there is nowhere else on today's surface to put it.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, schemars::JsonSchema)]
#[serde(tag = "entry", rename_all = "snake_case")]
pub enum AgentEntryView {
    /// A definition the registry loaded.
    Agent(AgentSummaryView),
    /// A definition the registry rejected, rendered exactly as the
    /// daemon reports it — the message names the file.
    LoadError { message: String },
}

/// The registry listing plus its per-file load errors — a broken
/// definition should be visible on the operator surface, not only in
/// the daemon log.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Default)]
pub struct AgentsView {
    /// Sorted by agent id.
    pub agents: Vec<AgentSummaryView>,
    pub errors: Vec<String>,
}

/// One agent definition in full — the Agent view's state (`agent.get`)
/// and the dashboard's agent detail page. Sourced from the daemon's
/// registry handle, so `fq reload` is reflected without a restart.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, schemars::JsonSchema)]
pub struct AgentDetailView {
    pub agent_id: String,
    pub model: String,
    pub system_prompt: String,
    pub tools: Vec<String>,
    /// Declared MCP server names.
    pub mcp_servers: Vec<String>,
    pub budget: Option<f64>,
    pub max_iterations: Option<u32>,
    pub effort: Option<String>,
    pub trigger: Option<String>,
    /// The definition file the agent was loaded from.
    pub path: String,
}
