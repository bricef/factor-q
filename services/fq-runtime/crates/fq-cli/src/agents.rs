//! The `fq agent list` verb (plan Phase 4, verb 9): the daemon's live
//! agent registry, read over the authenticated edge.
//!
//! It used to load the caller's own agents directory from disk. That
//! answered a different question from the one an operator is asking —
//! the daemon holds its registry in memory and `fq reload` swaps it,
//! so a directory edited since the last reload, a client configured
//! against a different path, or a daemon started elsewhere all made
//! the listing disagree with what would actually run. The registry is
//! the daemon's, so the read is the daemon's.
//!
//! `fq agent validate` sits here too and deliberately stays local:
//! linting a file someone is about to add is an offline operation, and
//! it is why removing the listing's local read costs nothing.

use std::path::Path;

use fq_runtime::agent::definition::parse_agent;
use fq_runtime::agent_view::{AgentEntryView, AgentSummaryView};

use crate::cli::GlobalArgs;
use crate::edge_call::edge_invoke;
use fq_runtime::surface::AgentListFilter;

fn format_agent_row_human(agent: &AgentSummaryView) -> String {
    format!(
        "  {:<30} model={} tools={} path={}",
        agent.agent_id, agent.model, agent.tool_count, agent.path
    )
}

pub(crate) async fn list_agents(global: &GlobalArgs) -> anyhow::Result<()> {
    let output = edge_invoke(
        global,
        fq_ops::OpId::List(fq_ops::Domain::Agent),
        serde_json::to_value(AgentListFilter {})?,
    )
    .await?
    .map_err(|e| anyhow::anyhow!("{e}"))?;
    let entries: Vec<AgentEntryView> = serde_json::from_value(output)?;

    // The index carries both kinds of row; the rendering keeps them
    // apart exactly as it always did — agents, then a block naming
    // every definition that failed to load.
    let mut agents = Vec::new();
    let mut errors = Vec::new();
    for entry in entries {
        match entry {
            AgentEntryView::Agent(agent) => agents.push(agent),
            AgentEntryView::LoadError { message } => errors.push(message),
        }
    }

    if agents.is_empty() && errors.is_empty() {
        // Disk-shaped no longer: there is no directory in this answer,
        // and naming the caller's own would be the skew this flip
        // exists to remove.
        println!("No agents found in the daemon's registry.");
        return Ok(());
    }

    if !agents.is_empty() {
        println!(
            "Loaded {} agent(s) from the daemon's registry:",
            agents.len()
        );
        for agent in &agents {
            println!("{}", format_agent_row_human(agent));
        }
    }

    if !errors.is_empty() {
        println!();
        println!("Errors ({}):", errors.len());
        for err in &errors {
            println!("  {err}");
        }
    }

    Ok(())
}

/// `fq agent validate` — lint one definition file offline. Local by
/// design: the daemon's registry has nothing to say about a file
/// nobody has added yet, and the check needs neither a broker nor a
/// pairing.
pub(crate) fn validate_agent(path: &Path) -> anyhow::Result<()> {
    let content = std::fs::read_to_string(path)
        .map_err(|err| anyhow::anyhow!("failed to read {}: {err}", path.display()))?;

    match parse_agent(&content) {
        Ok(agent) => {
            println!("✓ {} is valid", path.display());
            println!("  id:      {}", agent.id().as_str());
            println!("  model:   {}", agent.model());
            println!("  tools:   {}", agent.tools().len());
            if let Some(budget) = agent.budget() {
                println!("  budget:  ${budget:.2}");
            }
            // #35: valid, but do not let "✓ is valid" imply the declared
            // network boundary holds — nothing enforces it yet.
            if let Some(declared) = agent.sandbox().unenforced_network() {
                println!();
                println!("  ⚠ sandbox.network is declared but NOT enforced (#35)");
                println!("    declared: {}", declared.join(", "));
                println!("    This agent has ambient network access — it can reach any");
                println!("    host regardless. Enforcement: #208 (proxy), #209 (ADR-0010).");
            }
            Ok(())
        }
        Err(err) => Err(anyhow::anyhow!("{} is invalid: {err}", path.display())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The row rendering is the part of verb 9's output that the flip
    /// had to preserve byte-for-byte: id padded to 30, then the three
    /// `key=value` fields in that order.
    #[test]
    fn an_agent_row_renders_id_model_tools_and_path() {
        let row = format_agent_row_human(&AgentSummaryView {
            agent_id: "researcher".to_string(),
            model: "claude-haiku-4-5".to_string(),
            budget: Some(1.0),
            trigger: None,
            tool_count: 2,
            prompt_bytes: 14,
            path: "/agents/researcher.md".to_string(),
        });
        assert_eq!(
            row,
            "  researcher                     model=claude-haiku-4-5 tools=2 \
             path=/agents/researcher.md"
        );
    }

    /// A long id pushes the columns right rather than being truncated:
    /// an agent id is identity, and a listing that clips it is worse
    /// than a listing that wraps.
    #[test]
    fn a_long_id_is_never_truncated() {
        let row = format_agent_row_human(&AgentSummaryView {
            agent_id: "an-agent-with-a-very-long-identifier-indeed".to_string(),
            model: "m".to_string(),
            budget: None,
            trigger: None,
            tool_count: 0,
            prompt_bytes: 0,
            path: "/a.md".to_string(),
        });
        assert!(
            row.contains("an-agent-with-a-very-long-identifier-indeed model=m"),
            "got: {row}"
        );
    }
}
