//! Resume-after-interruption host-notice producer (#157).

use super::*;

/// Ignore near-instant restarts: they add noise without meaningfully
/// invalidating observations made before the restart.
const RESUME_NOTICE_MIN_ELAPSED_MS: i64 = 10_000;

impl<R: Reducer + Send + Sync> ReducerRunner<R> {
    pub(super) fn queue_resume_notice(&self, invocation_id: Uuid, last_write_ms: i64) {
        // Read the clock exactly once. The fully rendered body is then
        // persisted by the notice channel and replayed byte-for-byte.
        let elapsed_ms = self
            .config
            .clock
            .unix_now_ms()
            .saturating_sub(last_write_ms);
        if elapsed_ms < RESUME_NOTICE_MIN_ELAPSED_MS {
            return;
        }
        self.queue_host_notice(invocation_id, "resume", render_resume_notice(elapsed_ms));
    }
}

fn render_resume_notice(elapsed_ms: i64) -> String {
    let elapsed = render_elapsed(elapsed_ms.max(0) as u64);
    format!(
        "{}This invocation was interrupted and resumed. Approximately {elapsed} passed while suspended. The world may have changed: re-verify volatile state (files, branches, remote status, running processes) before relying on observations from before the gap.</host-notice>",
        crate::events::HOST_NOTICE_SENTINEL,
    )
}

fn render_elapsed(elapsed_ms: u64) -> String {
    let seconds = elapsed_ms / 1_000;
    if seconds < 60 {
        return format!("{seconds}s");
    }
    let minutes = seconds / 60;
    if minutes < 60 {
        return format!("{minutes}m");
    }
    let hours = minutes / 60;
    if hours < 24 {
        let remainder = minutes % 60;
        return if remainder == 0 {
            format!("{hours}h")
        } else {
            format!("{hours}h {remainder}m")
        };
    }
    format!("{}d", hours / 24)
}

#[cfg(test)]
mod tests {
    use super::render_resume_notice;

    #[test]
    fn elapsed_time_is_coarse() {
        assert!(render_resume_notice(180_999).contains("Approximately 3m passed"));
        assert!(render_resume_notice(7_800_000).contains("Approximately 2h 10m passed"));
        assert!(render_resume_notice(259_200_000).contains("Approximately 3d passed"));
    }
}
