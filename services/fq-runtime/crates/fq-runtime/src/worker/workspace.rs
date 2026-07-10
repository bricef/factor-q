//! Per-invocation workspace provisioning (the parallel-workers plan,
//! Phase 0 — #14/#70).
//!
//! Agents reference their working directory through the [`WORKSPACE_TOKEN`]
//! (`${workspace}`) in `sandbox.exec_cwd`/`fs_read`/`fs_write` and in tool
//! parameters, instead of hardcoding an absolute path. The runtime binds the
//! token per invocation via a [`WorkspaceProvider`]:
//!
//! - [`StaticWorkspace`] — every invocation binds to one shared directory.
//!   Today's behavior, and the rollback mode.
//! - [`PerInvocationWorkspace`] — each invocation gets a fresh empty
//!   directory under a root, so concurrent invocations cannot touch each
//!   other's files.
//!
//! The runtime deliberately knows nothing about what a workspace
//! *contains*. Populating it — e.g. cloning an upstream repo, branching,
//! pushing — is the agent's job through its granted tools, which keeps
//! VCS-specific concerns (and host binaries) out of the runtime entirely.
//! An agent that clones fresh into `${workspace}` also starts from the
//! latest upstream by construction, which is #14's stale-base fix.
//!
//! Lifecycle coupling (plan §3): a **suspended** invocation keeps its
//! workspace — drain/crash must leave it on disk so resume continues from
//! it. Reclaim happens only on a *terminal* outcome. The binding is
//! persisted in the invocation's durable state
//! (`invocation_state.workspace_ref`) so recovery re-associates; orphaned
//! directories of terminal invocations are swept by
//! [`WorkspaceProvider::prune`] at startup, alongside the recovery scan.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use tracing::{info, warn};
use uuid::Uuid;

/// The token agents write in sandbox paths and tool parameters; the
/// runtime substitutes the invocation's workspace path for it.
pub const WORKSPACE_TOKEN: &str = "${workspace}";

/// Errors from workspace provisioning. All permanent: provisioning is
/// pure filesystem work, so a failure means the host is genuinely
/// misconfigured (permissions, disk, a missing persisted directory) —
/// redelivering the trigger would reproduce it, and the consumer has no
/// `max_deliver` bound yet (#49).
#[derive(Debug, thiserror::Error)]
pub enum WorkspaceError {
    /// Creating the per-invocation directory failed.
    #[error("workspace provisioning failed: {0}")]
    ProvisionFailed(String),
    /// A resumed invocation's persisted workspace is gone from disk.
    /// Surface it loudly rather than silently rebuilding an empty
    /// workspace — the suspended work in it is unrecoverable.
    #[error("persisted workspace missing: {0}")]
    Missing(String),
    /// Releasing a workspace failed. Callers log this and continue —
    /// a reclaim failure must never override an invocation's outcome;
    /// the startup prune sweeps what's left behind.
    #[error("workspace reclaim failed: {0}")]
    ReclaimFailed(String),
    /// An agent's sandbox uses `${workspace}` but the daemon has no
    /// binding for it — a definition/config mismatch.
    #[error(transparent)]
    Unbound(#[from] crate::agent::UnboundWorkspace),
}

impl WorkspaceError {
    /// Whether the trigger should be redelivered (see
    /// [`crate::worker::ExecutorError::is_transient`]). No current
    /// variant is — filesystem provisioning failures don't heal on
    /// retry. Kept as the seam for future providers that do have a
    /// transient leg.
    pub fn is_transient(&self) -> bool {
        false
    }
}

/// Binds `${workspace}` for each invocation and manages the workspace's
/// lifetime around the invocation's own.
#[async_trait::async_trait]
pub trait WorkspaceProvider: Send + Sync + std::fmt::Debug {
    /// Resolve the workspace for a **fresh** invocation, creating it if
    /// the provider provisions per-invocation.
    async fn provision(&self, invocation_id: Uuid) -> Result<PathBuf, WorkspaceError>;

    /// Re-bind a **resumed** invocation to the workspace persisted in its
    /// state row. Faithful re-association regardless of provider: the
    /// persisted path (which may hold uncommitted suspended work) wins
    /// over anything the provider would mint today.
    async fn reattach(
        &self,
        invocation_id: Uuid,
        persisted: &str,
    ) -> Result<PathBuf, WorkspaceError> {
        let path = PathBuf::from(persisted);
        if path.is_dir() {
            Ok(path)
        } else {
            Err(WorkspaceError::Missing(format!(
                "invocation {invocation_id} was bound to {persisted}, which no longer exists"
            )))
        }
    }

    /// Release the workspace after a **terminal** outcome. Never called
    /// on suspend — a suspended invocation's workspace must survive the
    /// restart for resume.
    async fn reclaim(&self, invocation_id: Uuid, path: &Path) -> Result<(), WorkspaceError>;

    /// Startup sweep: remove workspaces whose invocation id is not in
    /// `keep` (the still-in-flight set from the recovery scan). Default
    /// no-op for providers that don't own per-invocation storage.
    async fn prune(&self, keep: &HashSet<String>) -> Result<(), WorkspaceError> {
        let _ = keep;
        Ok(())
    }
}

/// One shared directory for every invocation — today's single-workspace
/// behavior behind the `${workspace}` token, and the rollback mode when
/// per-invocation provisioning is disabled.
#[derive(Debug)]
pub struct StaticWorkspace {
    path: PathBuf,
}

impl StaticWorkspace {
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }
}

#[async_trait::async_trait]
impl WorkspaceProvider for StaticWorkspace {
    async fn provision(&self, _invocation_id: Uuid) -> Result<PathBuf, WorkspaceError> {
        Ok(self.path.clone())
    }

    async fn reclaim(&self, _invocation_id: Uuid, _path: &Path) -> Result<(), WorkspaceError> {
        Ok(())
    }
}

/// A fresh empty directory per invocation under `root`, named by the
/// invocation id. Pure filesystem — the runtime never runs a host
/// binary. What goes *into* the directory (a clone of some upstream,
/// scratch files, …) is the agent's business through its granted tools.
#[derive(Debug)]
pub struct PerInvocationWorkspace {
    root: PathBuf,
}

impl PerInvocationWorkspace {
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }
}

#[async_trait::async_trait]
impl WorkspaceProvider for PerInvocationWorkspace {
    async fn provision(&self, invocation_id: Uuid) -> Result<PathBuf, WorkspaceError> {
        let path = self.root.join(invocation_id.to_string());
        if path.exists() {
            // Invocation ids are fresh UUIDv7s, so a collision means a
            // previous provisioning half-finished. Fail loud rather than
            // run in a directory of unknown provenance.
            return Err(WorkspaceError::ProvisionFailed(format!(
                "workspace path {} already exists",
                path.display()
            )));
        }
        std::fs::create_dir_all(&path).map_err(|err| {
            WorkspaceError::ProvisionFailed(format!(
                "cannot create workspace {}: {err}",
                path.display()
            ))
        })?;
        info!(
            invocation_id = %invocation_id,
            workspace = %path.display(),
            "provisioned invocation workspace"
        );
        Ok(path)
    }

    async fn reclaim(&self, invocation_id: Uuid, path: &Path) -> Result<(), WorkspaceError> {
        // Guard against reclaiming anything outside our root — the path
        // comes back from the caller and, on resume, from a persisted
        // state row.
        if !path.starts_with(&self.root) {
            return Err(WorkspaceError::ReclaimFailed(format!(
                "{} is not under the workspace root {}",
                path.display(),
                self.root.display()
            )));
        }
        std::fs::remove_dir_all(path).map_err(|err| {
            WorkspaceError::ReclaimFailed(format!("cannot remove {}: {err}", path.display()))
        })?;
        info!(
            invocation_id = %invocation_id,
            workspace = %path.display(),
            "reclaimed invocation workspace"
        );
        Ok(())
    }

    async fn prune(&self, keep: &HashSet<String>) -> Result<(), WorkspaceError> {
        let entries = match std::fs::read_dir(&self.root) {
            Ok(entries) => entries,
            // No directory yet — nothing was ever provisioned.
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(err) => {
                return Err(WorkspaceError::ReclaimFailed(format!(
                    "cannot read workspace root {}: {err}",
                    self.root.display()
                )));
            }
        };

        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            // Only sweep entries that look like invocation workspaces —
            // a shared checkout or operator file living beside them
            // must never be collateral.
            if keep.contains(&name) || Uuid::parse_str(&name).is_err() {
                continue;
            }
            let path = entry.path();
            match std::fs::remove_dir_all(&path) {
                Ok(()) => info!(workspace = %path.display(), "pruned orphaned workspace"),
                Err(err) => {
                    warn!(workspace = %path.display(), error = %err, "failed to prune workspace")
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn static_workspace_binds_and_reclaims_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let provider = StaticWorkspace::new(dir.path().to_path_buf());
        let id = Uuid::now_v7();
        let ws = provider.provision(id).await.unwrap();
        assert_eq!(ws, dir.path());
        provider.reclaim(id, &ws).await.unwrap();
        assert!(dir.path().is_dir(), "static workspace must survive reclaim");
    }

    #[tokio::test]
    async fn reattach_returns_persisted_path_when_present() {
        let dir = tempfile::tempdir().unwrap();
        let provider = StaticWorkspace::new(PathBuf::from("/elsewhere"));
        let persisted = dir.path().to_string_lossy().into_owned();
        let ws = provider.reattach(Uuid::now_v7(), &persisted).await.unwrap();
        assert_eq!(
            ws,
            dir.path(),
            "persisted binding wins over the provider's own"
        );
    }

    #[tokio::test]
    async fn reattach_missing_path_fails_loud() {
        let provider = StaticWorkspace::new(PathBuf::from("/elsewhere"));
        let err = provider
            .reattach(Uuid::now_v7(), "/nonexistent/workspace/path")
            .await
            .unwrap_err();
        assert!(matches!(err, WorkspaceError::Missing(_)));
    }

    #[tokio::test]
    async fn per_invocation_provision_reclaim_lifecycle() {
        let root_dir = tempfile::tempdir().unwrap();
        let root = root_dir.path().to_path_buf();
        let provider = PerInvocationWorkspace::new(root.clone());

        let id = Uuid::now_v7();
        let ws = provider.provision(id).await.unwrap();
        assert_eq!(ws, root.join(id.to_string()));
        assert!(ws.is_dir(), "workspace directory exists");

        // Terminal outcome with content left behind: reclaim removes it.
        std::fs::write(ws.join("scratch.txt"), "agent output\n").unwrap();
        provider.reclaim(id, &ws).await.unwrap();
        assert!(!ws.exists(), "workspace removed on terminal reclaim");
    }

    #[tokio::test]
    async fn provision_refuses_existing_path() {
        let root_dir = tempfile::tempdir().unwrap();
        let provider = PerInvocationWorkspace::new(root_dir.path().to_path_buf());
        let id = Uuid::now_v7();
        std::fs::create_dir_all(root_dir.path().join(id.to_string())).unwrap();
        let err = provider.provision(id).await.unwrap_err();
        assert!(matches!(err, WorkspaceError::ProvisionFailed(_)));
    }

    #[tokio::test]
    async fn reclaim_refuses_paths_outside_root() {
        let root_dir = tempfile::tempdir().unwrap();
        let elsewhere = tempfile::tempdir().unwrap();
        let provider = PerInvocationWorkspace::new(root_dir.path().to_path_buf());
        let err = provider
            .reclaim(Uuid::now_v7(), elsewhere.path())
            .await
            .unwrap_err();
        assert!(matches!(err, WorkspaceError::ReclaimFailed(_)));
        assert!(elsewhere.path().exists());
    }

    #[tokio::test]
    async fn prune_removes_only_orphaned_invocation_dirs() {
        let root_dir = tempfile::tempdir().unwrap();
        let root = root_dir.path().to_path_buf();
        let provider = PerInvocationWorkspace::new(root.clone());

        let kept = Uuid::now_v7();
        let orphan = Uuid::now_v7();
        let kept_ws = provider.provision(kept).await.unwrap();
        let orphan_ws = provider.provision(orphan).await.unwrap();
        // A non-uuid neighbor (e.g. a shared checkout) must be untouched.
        let bystander = root.join("shared-checkout");
        std::fs::create_dir_all(&bystander).unwrap();

        let keep: HashSet<String> = [kept.to_string()].into();
        provider.prune(&keep).await.unwrap();

        assert!(kept_ws.exists(), "in-flight workspace survives the sweep");
        assert!(!orphan_ws.exists(), "orphaned workspace is swept");
        assert!(bystander.exists(), "non-invocation dirs are never swept");
    }
}
