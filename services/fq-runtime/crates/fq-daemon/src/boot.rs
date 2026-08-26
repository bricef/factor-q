//! What the daemon works out before it runs anything: where its stores
//! are, whether they need migrating, and the host label it registers
//! itself under.
//!
//! Split out of `daemon.rs` — each of these is a function of the config
//! alone, so they are testable without a broker, a store or a runtime.

/// Migrate a leftover v1 single-file `events.db` into the split
/// layout, then hand back the per-store paths. The daemon calls this
/// before it opens anything.
///
/// It used to be "every command that opens a store for *writing*",
/// with read commands surfacing a migration hint instead of running
/// the split themselves. No client verb opens a store at all now
/// (plan Phase 4), so the daemon is the only caller and the hint has
/// no one left to give — `fq status` is the last client-side reader
/// and it reports a pending split as a line in its own output.
pub(crate) async fn ensure_split_dbs(
    config: &fq_runtime::Config,
) -> anyhow::Result<fq_runtime::RuntimeDbPaths> {
    match fq_runtime::split_legacy_events_db(&config.cache.directory).await? {
        fq_runtime::SplitOutcome::Completed(stats) => {
            println!(
                "migrated legacy events.db into worker.db + control-plane.db + projection.db \
                 ({stats}); events.db.pre-split kept as rollback"
            );
        }
        fq_runtime::SplitOutcome::NotNeeded => {}
    }
    Ok(runtime_db_paths(config))
}

/// Build the `${workspace}` provider from `[workspace]` (parallel-workers
/// Phase 0): with `per_invocation = true` each invocation gets a fresh
/// empty directory under `path`; otherwise every invocation binds to
/// `path` itself. No `path` configured → no binding, and agents that use
/// the token fail loudly at invocation start. Pure filesystem either way
/// — what goes into a workspace is the agent's business.
pub(crate) fn workspace_provider(
    config: &fq_runtime::Config,
) -> Option<std::sync::Arc<dyn fq_runtime::worker::workspace::WorkspaceProvider>> {
    use fq_runtime::worker::workspace::{PerInvocationWorkspace, StaticWorkspace};
    let ws = &config.workspace;
    let path = ws.path.clone()?;
    if ws.per_invocation {
        Some(std::sync::Arc::new(PerInvocationWorkspace::new(path)))
    } else {
        Some(std::sync::Arc::new(StaticWorkspace::new(path)))
    }
}

/// Per-store SQLite database paths under the configured cache
/// directory (the #262 split layout: `worker.db`, `control-plane.db`,
/// `projection.db`). Stored next to the pricing JSON rather than in
/// their own subdirectory.
pub(crate) fn runtime_db_paths(config: &fq_runtime::Config) -> fq_runtime::RuntimeDbPaths {
    fq_runtime::RuntimeDbPaths::under(&config.cache.directory)
}

/// Best-effort host label for the worker registration row.
/// Operator-informational only — the value isn't load-bearing
/// in v1 and a placeholder is fine when no hostname is
/// available. v2 will likely prefer a syscall-backed lookup.
pub(crate) fn local_host_label() -> String {
    std::env::var("HOSTNAME")
        .or_else(|_| std::env::var("COMPUTERNAME"))
        .unwrap_or_else(|_| "local".to_string())
}
