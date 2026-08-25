//! The Agent view: the daemon's live registry, projected for the
//! operator surface.
//!
//! Every reader answers from the same registry handle, through the
//! edge's `agent.list`/`agent.get`. The projection lives here, once,
//! so no two readers can drift: an operator at the CLI and a colleague
//! on the dashboard are looking at the same computation over the same
//! `SharedRegistry`, not two renderings of the same idea.
//!
//! Its own module rather than a section of the runtime's `views.rs`
//! because it folds nothing: every other view is a fold of atoms read
//! out of a store, whereas an agent definition is configuration the
//! daemon holds in memory and `fq reload` swaps wholesale. Same shape
//! on the wire, different provenance — worth a file boundary that says
//! so.
//!
//! It sits in this crate rather than the runtime because a projection
//! has to live with the type it reads: `LoadedAgent` is this crate's
//! and the view shapes are the contract crate's, so anywhere else the
//! `From` impls below are two foreign types and the orphan rule refuses
//! them. That is the coherence rule agreeing with the domain — reading
//! a registry is the registry's business, and it needs nothing the
//! runtime has. `fq-runtime` re-exports this as `agent_view`.

// The shapes are `fq_ops::agent_view` and are re-exported here, so a
// caller reaches them by the same path as before. What stays is the
// projecting: it needs the registry, which is this crate's.
pub use fq_ops::agent_view::*;

use crate::{AgentRegistry, LoadedAgent};

impl From<&LoadedAgent> for AgentSummaryView {
    /// Project one loaded definition into its index row.
    fn from(loaded: &LoadedAgent) -> Self {
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

impl From<&AgentRegistry> for AgentsView {
    /// Project a registry snapshot: every loaded definition sorted by
    /// id, plus the per-file load errors as the registry phrased them.
    ///
    /// The sort is part of the contract, not a convenience — the
    /// registry is a `HashMap`, so an unsorted listing would reorder
    /// itself between two reads of an unchanged registry.
    fn from(registry: &AgentRegistry) -> Self {
        let mut agents: Vec<AgentSummaryView> =
            registry.iter().map(AgentSummaryView::from).collect();
        agents.sort_by(|a, b| a.agent_id.cmp(&b.agent_id));
        let errors = registry.errors().iter().map(|e| e.to_string()).collect();
        AgentsView { agents, errors }
    }
}

impl From<&LoadedAgent> for AgentDetailView {
    /// Project one loaded definition in full.
    fn from(loaded: &LoadedAgent) -> Self {
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
                    fq_ops::events::Effort::Minimal => "minimal",
                    fq_ops::events::Effort::Low => "low",
                    fq_ops::events::Effort::Medium => "medium",
                    fq_ops::events::Effort::High => "high",
                    fq_ops::events::Effort::XHigh => "xhigh",
                }
                .to_string()
            }),
            trigger: agent.trigger().map(String::from),
            path: loaded.path.display().to_string(),
        }
    }
}

/// The Agent view's index: loaded definitions first, in id order, then
/// the files the registry rejected. Agents before errors because that
/// is the order a listing reads in — what is running, then what failed
/// to.
///
/// A function rather than a `From`: the conversion lands on
/// `Vec<AgentEntryView>`, and an impl written that way reads as a fact
/// about `Vec` rather than about the roster.
pub fn agent_index(registry: &AgentRegistry) -> Vec<AgentEntryView> {
    let snapshot = AgentsView::from(registry);
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

/// Census a registry snapshot into the shape `control.status` declares.
///
/// A `From` impl rather than a constructor on the type: the shape lives
/// in the wire crate, which cannot see an `AgentRegistry`, and an
/// inherent method has to sit with its type. The reading stays here,
/// beside the registry it reads.
impl From<&AgentRegistry> for fq_ops::surface::StatusRegistry {
    fn from(registry: &AgentRegistry) -> Self {
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
        let view = AgentsView::from(&registry);
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
        let snapshot = AgentsView::from(&registry);
        let index = agent_index(&registry);
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
        let index = agent_index(&registry);
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
            .get_loaded(&crate::AgentId::new("probe").unwrap())
            .expect("probe is loaded");
        let detail = AgentDetailView::from(loaded);
        assert_eq!(detail.agent_id, "probe");
        assert!(detail.system_prompt.contains("You are a probe."));
        assert_eq!(detail.tools, vec!["exec".to_string()]);
        assert_eq!(detail.budget, Some(0.10));
        assert!(detail.path.ends_with("probe.md"), "got: {}", detail.path);
    }
}
