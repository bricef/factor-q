//! The Agent view: the daemon's live registry, projected for the
//! operator surface.
//!
//! Two surfaces answer from the same registry handle — the edge's
//! `agent.list`/`agent.get` (plan Phase 4, verb 9) and the read
//! service's `agents`/`agent` RPCs, which the dashboard still calls
//! until cohort 4.4 re-points it. The projection lives here, once, so
//! the two cannot drift while they coexist: an operator reading the
//! CLI and a colleague reading the dashboard are looking at the same
//! computation over the same `SharedRegistry`, not two renderings of
//! the same idea.
//!
//! Its own module rather than a section of `views.rs` because it folds
//! nothing: every other view is a fold of atoms read out of a store,
//! whereas an agent definition is configuration the daemon holds in
//! memory and `fq reload` swaps wholesale. Same shape on the wire,
//! different provenance — worth a file boundary that says so.

use serde::{Deserialize, Serialize};

use crate::agent::{AgentRegistry, LoadedAgent};

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

impl AgentSummaryView {
    /// Project one loaded definition into its index row.
    pub fn from_loaded(loaded: &LoadedAgent) -> Self {
        AgentSummaryView {
            agent_id: loaded.agent.id().as_str().to_string(),
            model: loaded.agent.model().to_string(),
            budget: loaded.agent.budget(),
            trigger: loaded.agent.trigger().map(String::from),
            tool_count: loaded.agent.tools().len() as i64,
            prompt_bytes: loaded.agent.system_prompt().len() as i64,
            path: loaded.path.display().to_string(),
        }
    }
}

impl AgentsView {
    /// Project a registry snapshot: every loaded definition sorted by
    /// id, plus the per-file load errors as the registry phrased them.
    ///
    /// The sort is part of the contract, not a convenience — the
    /// registry is a `HashMap`, so an unsorted listing would reorder
    /// itself between two reads of an unchanged registry.
    pub fn from_registry(registry: &AgentRegistry) -> Self {
        let mut agents: Vec<AgentSummaryView> =
            registry.iter().map(AgentSummaryView::from_loaded).collect();
        agents.sort_by(|a, b| a.agent_id.cmp(&b.agent_id));
        let errors = registry.errors().iter().map(|e| e.to_string()).collect();
        AgentsView { agents, errors }
    }
}

impl AgentDetailView {
    /// Project one loaded definition in full.
    pub fn from_loaded(loaded: &LoadedAgent) -> Self {
        let agent = &loaded.agent;
        AgentDetailView {
            agent_id: agent.id().as_str().to_string(),
            model: agent.model().to_string(),
            system_prompt: agent.system_prompt().to_string(),
            tools: agent.tools().to_vec(),
            mcp_servers: agent
                .mcp_servers()
                .iter()
                .map(|s| s.server.clone())
                .collect(),
            budget: agent.budget(),
            max_iterations: agent.max_iterations(),
            // The definition frontmatter's own lowercase spelling.
            effort: agent.effort().map(|e| {
                match e {
                    crate::events::Effort::Minimal => "minimal",
                    crate::events::Effort::Low => "low",
                    crate::events::Effort::Medium => "medium",
                    crate::events::Effort::High => "high",
                    crate::events::Effort::XHigh => "xhigh",
                }
                .to_string()
            }),
            trigger: agent.trigger().map(String::from),
            path: loaded.path.display().to_string(),
        }
    }
}

impl AgentEntryView {
    /// The Agent view's index: loaded definitions first, in id order,
    /// then the files the registry rejected. Agents before errors
    /// because that is the order a listing reads in — what is running,
    /// then what failed to.
    pub fn index(registry: &AgentRegistry) -> Vec<Self> {
        let snapshot = AgentsView::from_registry(registry);
        snapshot
            .agents
            .into_iter()
            .map(AgentEntryView::Agent)
            .chain(
                snapshot
                    .errors
                    .into_iter()
                    .map(|message| AgentEntryView::LoadError { message }),
            )
            .collect()
    }
}

/// Census a registry snapshot into the shape `control.status` declares.
///
/// A `From` impl rather than a constructor on the type: the shape lives
/// in the wire crate, which cannot see an `AgentRegistry`, and an
/// inherent method has to sit with its type. The reading stays here,
/// beside the registry it reads.
impl From<&crate::AgentRegistry> for fq_ops::surface::StatusRegistry {
    fn from(registry: &crate::AgentRegistry) -> Self {
        fq_ops::surface::StatusRegistry {
            agents: registry.len() as i64,
            load_errors: registry.errors().iter().map(|e| e.to_string()).collect(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A registry with one good definition and one unparseable file:
    /// both surfaces' inputs, projected once.
    fn seeded_registry() -> (tempfile::TempDir, AgentRegistry) {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("probe.md"),
            "---\nname: probe\nmodel: claude-haiku-4-5\ntools:\n  - exec\nbudget: 0.10\n---\n\n\
             You are a probe.\n",
        )
        .unwrap();
        std::fs::write(dir.path().join("notes.md"), "# not a definition\n").unwrap();
        let registry = AgentRegistry::load_from_directory(dir.path(), None).unwrap();
        (dir, registry)
    }

    #[test]
    fn the_snapshot_carries_rows_and_errors() {
        let (_dir, registry) = seeded_registry();
        let view = AgentsView::from_registry(&registry);
        assert_eq!(view.agents.len(), 1);
        assert_eq!(view.agents[0].agent_id, "probe");
        assert_eq!(view.agents[0].tool_count, 1);
        assert!(view.agents[0].path.ends_with("probe.md"));
        assert_eq!(view.errors.len(), 1, "{:?}", view.errors);
        assert!(view.errors[0].contains("notes.md"), "{:?}", view.errors);
    }

    /// The index is the snapshot flattened: the same rows and the same
    /// errors, in that order — the property the CLI's rendering and
    /// the dashboard's page both depend on.
    #[test]
    fn the_index_is_the_snapshot_flattened() {
        let (_dir, registry) = seeded_registry();
        let snapshot = AgentsView::from_registry(&registry);
        let index = AgentEntryView::index(&registry);
        assert_eq!(index.len(), snapshot.agents.len() + snapshot.errors.len());
        assert!(matches!(
            &index[0],
            AgentEntryView::Agent(row) if row.agent_id == "probe"
        ));
        assert!(matches!(
            &index[1],
            AgentEntryView::LoadError { message } if message == &snapshot.errors[0]
        ));
    }

    /// The tag is part of the wire contract: a reader distinguishes
    /// the two kinds of row by `entry`, and a loaded row's fields sit
    /// beside it rather than nested.
    #[test]
    fn index_rows_are_tagged_on_the_wire() {
        let (_dir, registry) = seeded_registry();
        let index = AgentEntryView::index(&registry);
        let json = serde_json::to_value(&index).unwrap();
        assert_eq!(json[0]["entry"], "agent");
        assert_eq!(json[0]["agent_id"], "probe");
        assert_eq!(json[1]["entry"], "load_error");
        assert!(
            json[1]["message"]
                .as_str()
                .unwrap()
                .contains("missing or malformed YAML frontmatter")
        );
        // And it round-trips, which the edge's typed List requires.
        let back: Vec<AgentEntryView> = serde_json::from_value(json).unwrap();
        assert_eq!(back, index);
    }

    #[test]
    fn the_detail_carries_the_prompt_and_its_file() {
        let (_dir, registry) = seeded_registry();
        let loaded = registry
            .get_loaded(&crate::agent::AgentId::new("probe").unwrap())
            .expect("probe is loaded");
        let detail = AgentDetailView::from_loaded(loaded);
        assert_eq!(detail.agent_id, "probe");
        assert!(detail.system_prompt.contains("You are a probe."));
        assert_eq!(detail.tools, vec!["exec".to_string()]);
        assert_eq!(detail.budget, Some(0.10));
        assert!(detail.path.ends_with("probe.md"), "got: {}", detail.path);
    }
}
