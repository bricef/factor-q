//! Tool-set-changed host-notice producer (#158).

use std::collections::BTreeSet;

use super::*;

impl<R: Reducer + Send + Sync> ReducerRunner<R> {
    pub(super) async fn queue_tools_changed_notice(
        &self,
        invocation_id: Uuid,
        fresh: &[ToolSchema],
    ) -> Result<(), ExecutorError> {
        let rows = self
            .config
            .store
            .list_llm_dispatches_for_invocation(&invocation_id.to_string())
            .await
            .map_err(map_store_err)?;
        let Some(previous) = rows
            .iter()
            .rev()
            .find(|row| row.status == DispatchStatus::Completed)
        else {
            return Ok(());
        };
        let previous: ModelRequest = match serde_json::from_str(&previous.request_payload) {
            Ok(request) => request,
            Err(error) => {
                warn!(%invocation_id, %error, "could not parse prior LLM request for tool-set diff");
                return Ok(());
            }
        };

        let previous: BTreeSet<_> = previous.tools.into_iter().map(|tool| tool.name).collect();
        let fresh: BTreeSet<_> = fresh.iter().map(|tool| tool.name.clone()).collect();
        let added: Vec<_> = fresh.difference(&previous).cloned().collect();
        let removed: Vec<_> = previous.difference(&fresh).cloned().collect();
        if added.is_empty() && removed.is_empty() {
            return Ok(());
        }

        self.queue_host_notice(
            invocation_id,
            "tools_changed",
            render_tools_changed_notice(&added, &removed),
        );
        Ok(())
    }
}

fn render_tools_changed_notice(added: &[String], removed: &[String]) -> String {
    let mut changes = Vec::new();
    if !added.is_empty() {
        changes.push(format!("Added: {}.", added.join(", ")));
    }
    if !removed.is_empty() {
        changes.push(format!("Removed: {}.", removed.join(", ")));
    }
    let instruction = match (added.is_empty(), removed.is_empty()) {
        (false, false) => {
            "Do not call removed tools; check the added ones before choosing an approach."
        }
        (false, true) => "Check the added tools before choosing an approach.",
        (true, false) => "Do not call removed tools.",
        (true, true) => unreachable!("the caller skips empty diffs"),
    };
    format!(
        "{}The tools available to you changed while this invocation was suspended. {} {instruction}</host-notice>",
        crate::events::HOST_NOTICE_SENTINEL,
        changes.join(" "),
    )
}

#[cfg(test)]
mod tests {
    use super::render_tools_changed_notice;

    #[test]
    fn renderer_lists_sorted_changes_and_wraps_the_notice() {
        let body = render_tools_changed_notice(
            &["added_a".into(), "added_b".into()],
            &["removed_c".into()],
        );
        assert_eq!(
            body,
            "<host-notice>The tools available to you changed while this invocation was suspended. Added: added_a, added_b. Removed: removed_c. Do not call removed tools; check the added ones before choosing an approach.</host-notice>"
        );
    }
}
