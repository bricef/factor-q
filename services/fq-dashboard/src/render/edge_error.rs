//! Rendering for pages whose edge request received an error answer.

use super::esc;

/// What a dashboard page shows when the runtime answered with an error.
///
/// This is deliberately distinct from the unreachable banner: the daemon
/// replied, so diagnosing a connectivity fault would send the operator in
/// the wrong direction. Both the page identity and the edge's own words are
/// retained and escaped.
pub fn edge_error(title: &str, error: &str) -> String {
    format!(
        concat!(
            r#"<p class="bad">the runtime answered, and could not serve this {}.</p>"#,
            r#"<pre class="turn err">{}</pre>"#,
            r#"<p class="muted">the daemon is reachable — this is not a connectivity fault.</p>"#,
        ),
        esc(title),
        esc(error),
    )
}
