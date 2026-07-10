//! Per-invocation workspace provisioning (the parallel-workers plan,
//! Phase 0 — #14/#70).
//!
//! Agents reference their working directory through the [`WORKSPACE_TOKEN`]
//! (`${workspace}`) in `sandbox.exec_cwd`/`fs_read`/`fs_write` and in tool
//! parameters, instead of hardcoding an absolute path. The runtime binds the
//! token per invocation via a [`WorkspaceProvider`]:
//!
//! - [`StaticWorkspace`] — every invocation binds to one shared checkout.
//!   Today's behavior, and the `worktrees = false` rollback mode.
//! - [`GitWorktreeProvider`] — each invocation gets a fresh, detached git
//!   worktree off `base_ref` (fetched first, so a stale local ref can't
//!   quietly reintroduce #14's stale-PR bug).
//!
//! Lifecycle coupling (plan §3): a **suspended** invocation keeps its
//! workspace — drain/crash must leave the worktree on disk so resume
//! continues from it. Reclaim happens only on a *terminal* outcome. The
//! binding is persisted in the invocation's durable state
//! (`invocation_state.workspace_ref`) so recovery re-associates; orphaned
//! worktrees of terminal invocations are swept by [`WorkspaceProvider::prune`]
//! at startup, alongside the recovery scan.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use tokio::process::Command;
use tracing::{debug, info, warn};
use uuid::Uuid;

/// The token agents write in sandbox paths and tool parameters; the
/// runtime substitutes the invocation's workspace path for it.
pub const WORKSPACE_TOKEN: &str = "${workspace}";

/// Errors from workspace provisioning. The transient/permanent split
/// matters at the trigger boundary: a pre-WAL transient failure NAKs the
/// trigger for redelivery, a permanent one ACK-and-consumes it. Until the
/// consumer gets a `max_deliver` bound (#49), misclassifying a permanent
/// condition as transient redelivers forever — so only the fetch (network)
/// leg is transient.
#[derive(Debug, thiserror::Error)]
pub enum WorkspaceError {
    /// `git fetch` failed — usually the network, retryable.
    #[error("workspace fetch failed: {0}")]
    FetchFailed(String),
    /// Provisioning failed (bad ref, disk, path collision) — permanent.
    #[error("workspace provisioning failed: {0}")]
    ProvisionFailed(String),
    /// A resumed invocation's persisted workspace is gone from disk.
    /// Permanent: the uncommitted work is unrecoverable; surface it
    /// loudly rather than silently rebuilding an empty workspace.
    #[error("persisted workspace missing: {0}")]
    Missing(String),
    /// Releasing a workspace failed. Callers log this and continue —
    /// a reclaim failure must never override an invocation's outcome;
    /// the startup prune sweeps what's left behind.
    #[error("workspace reclaim failed: {0}")]
    ReclaimFailed(String),
    /// An agent's sandbox uses `${workspace}` but the daemon has no
    /// binding for it — a definition/config mismatch. Permanent.
    #[error(transparent)]
    Unbound(#[from] crate::agent::UnboundWorkspace),
}

impl WorkspaceError {
    pub fn is_transient(&self) -> bool {
        matches!(self, WorkspaceError::FetchFailed(_))
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

/// One shared checkout for every invocation — today's single-workspace
/// behavior behind the `${workspace}` token, and the rollback mode when
/// worktrees are disabled.
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

/// A fresh, detached git worktree per invocation, provisioned off
/// `base_ref` after a fetch. Worktrees live under `worktrees_dir`, one
/// directory per invocation id, sharing `repo`'s object store.
#[derive(Debug)]
pub struct GitWorktreeProvider {
    repo: PathBuf,
    worktrees_dir: PathBuf,
    base_ref: String,
}

impl GitWorktreeProvider {
    pub fn new(repo: PathBuf, worktrees_dir: PathBuf, base_ref: String) -> Self {
        Self {
            repo,
            worktrees_dir,
            base_ref,
        }
    }

    async fn git(&self, args: &[&str]) -> Result<(), String> {
        let output = Command::new("git")
            .arg("-C")
            .arg(&self.repo)
            .args(args)
            .output()
            .await
            .map_err(|err| format!("failed to spawn git: {err}"))?;
        if output.status.success() {
            Ok(())
        } else {
            Err(format!(
                "git {} exited {}: {}",
                args.join(" "),
                output.status,
                String::from_utf8_lossy(&output.stderr).trim()
            ))
        }
    }

    /// `origin/main`-style refs are remote-tracking: fetch the remote's
    /// branch first so provisioning starts from the *latest* base, not
    /// whatever the local tracking ref last saw (the quiet way #14's
    /// stale-base bug would come back). Local refs (`main`, `HEAD`,
    /// `refs/…`) skip the fetch.
    fn remote_branch(&self) -> Option<(&str, &str)> {
        if self.base_ref.starts_with("refs/") {
            return None;
        }
        self.base_ref.split_once('/')
    }
}

#[async_trait::async_trait]
impl WorkspaceProvider for GitWorktreeProvider {
    async fn provision(&self, invocation_id: Uuid) -> Result<PathBuf, WorkspaceError> {
        if let Some((remote, branch)) = self.remote_branch() {
            self.git(&["fetch", "--quiet", remote, branch])
                .await
                .map_err(WorkspaceError::FetchFailed)?;
        }

        std::fs::create_dir_all(&self.worktrees_dir).map_err(|err| {
            WorkspaceError::ProvisionFailed(format!(
                "cannot create worktrees dir {}: {err}",
                self.worktrees_dir.display()
            ))
        })?;

        let path = self.worktrees_dir.join(invocation_id.to_string());
        if path.exists() {
            // Invocation ids are fresh UUIDv7s, so a collision means a
            // previous provisioning half-finished. Fail loud rather than
            // run in a directory of unknown provenance.
            return Err(WorkspaceError::ProvisionFailed(format!(
                "worktree path {} already exists",
                path.display()
            )));
        }

        let path_str = path.to_string_lossy().into_owned();
        self.git(&["worktree", "add", "--detach", &path_str, &self.base_ref])
            .await
            .map_err(WorkspaceError::ProvisionFailed)?;

        info!(
            invocation_id = %invocation_id,
            worktree = %path.display(),
            base_ref = %self.base_ref,
            "provisioned invocation worktree"
        );
        Ok(path)
    }

    async fn reclaim(&self, invocation_id: Uuid, path: &Path) -> Result<(), WorkspaceError> {
        // --force: a terminal invocation legitimately leaves a dirty
        // tree behind (its durable output is the pushed branch/PR, not
        // the worktree); an un-forced remove would refuse and leak.
        let path_str = path.to_string_lossy().into_owned();
        self.git(&["worktree", "remove", "--force", &path_str])
            .await
            .map_err(WorkspaceError::ReclaimFailed)?;
        debug!(
            invocation_id = %invocation_id,
            worktree = %path.display(),
            "reclaimed invocation worktree"
        );
        Ok(())
    }

    async fn prune(&self, keep: &HashSet<String>) -> Result<(), WorkspaceError> {
        let entries = match std::fs::read_dir(&self.worktrees_dir) {
            Ok(entries) => entries,
            // No directory yet — nothing was ever provisioned.
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(err) => {
                return Err(WorkspaceError::ReclaimFailed(format!(
                    "cannot read worktrees dir {}: {err}",
                    self.worktrees_dir.display()
                )));
            }
        };

        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if keep.contains(&name) {
                continue;
            }
            let path = entry.path();
            let path_str = path.to_string_lossy().into_owned();
            match self
                .git(&["worktree", "remove", "--force", &path_str])
                .await
            {
                Ok(()) => info!(worktree = %path.display(), "pruned orphaned worktree"),
                Err(err) => {
                    warn!(worktree = %path.display(), error = %err, "failed to prune worktree")
                }
            }
        }

        // Clear any registrations whose directories are already gone
        // (e.g. an operator rm -rf'd a worktree by hand).
        if let Err(err) = self.git(&["worktree", "prune"]).await {
            warn!(error = %err, "git worktree prune failed");
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn sh(dir: &Path, cmd: &str, args: &[&str]) {
        let status = Command::new(cmd)
            .current_dir(dir)
            .args(args)
            .output()
            .await
            .unwrap();
        assert!(
            status.status.success(),
            "{cmd} {args:?} failed: {}",
            String::from_utf8_lossy(&status.stderr)
        );
    }

    /// A tempdir git repo with one commit, no remotes — provisioning off
    /// `HEAD` exercises the no-fetch path hermetically.
    async fn test_repo() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("repo");
        std::fs::create_dir(&repo).unwrap();
        sh(&repo, "git", &["init", "--quiet"]).await;
        std::fs::write(repo.join("README.md"), "hello\n").unwrap();
        sh(&repo, "git", &["add", "."]).await;
        sh(
            &repo,
            "git",
            &[
                "-c",
                "user.email=test@example.invalid",
                "-c",
                "user.name=test",
                "commit",
                "--quiet",
                "-m",
                "init",
            ],
        )
        .await;
        (dir, repo)
    }

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
            .reattach(Uuid::now_v7(), "/nonexistent/worktree/path")
            .await
            .unwrap_err();
        assert!(matches!(err, WorkspaceError::Missing(_)));
    }

    #[tokio::test]
    async fn worktree_provision_reclaim_lifecycle() {
        let (_guard, repo) = test_repo().await;
        let wt_dir = repo.parent().unwrap().join("wt");
        let provider = GitWorktreeProvider::new(repo.clone(), wt_dir.clone(), "HEAD".to_string());

        let id = Uuid::now_v7();
        let ws = provider.provision(id).await.unwrap();
        assert_eq!(ws, wt_dir.join(id.to_string()));
        assert!(ws.join("README.md").is_file(), "worktree has the checkout");
        assert!(ws.join(".git").exists(), "worktree is git-linked");

        // Terminal outcome with a dirty tree: reclaim must still succeed.
        std::fs::write(ws.join("scratch.txt"), "uncommitted\n").unwrap();
        provider.reclaim(id, &ws).await.unwrap();
        assert!(!ws.exists(), "worktree removed on terminal reclaim");
    }

    #[tokio::test]
    async fn provision_refuses_existing_path() {
        let (_guard, repo) = test_repo().await;
        let wt_dir = repo.parent().unwrap().join("wt");
        let provider = GitWorktreeProvider::new(repo.clone(), wt_dir.clone(), "HEAD".to_string());
        let id = Uuid::now_v7();
        std::fs::create_dir_all(wt_dir.join(id.to_string())).unwrap();
        let err = provider.provision(id).await.unwrap_err();
        assert!(matches!(err, WorkspaceError::ProvisionFailed(_)));
    }

    #[tokio::test]
    async fn prune_removes_only_non_kept_worktrees() {
        let (_guard, repo) = test_repo().await;
        let wt_dir = repo.parent().unwrap().join("wt");
        let provider = GitWorktreeProvider::new(repo.clone(), wt_dir.clone(), "HEAD".to_string());

        let kept = Uuid::now_v7();
        let orphan = Uuid::now_v7();
        let kept_ws = provider.provision(kept).await.unwrap();
        let orphan_ws = provider.provision(orphan).await.unwrap();

        let keep: HashSet<String> = [kept.to_string()].into();
        provider.prune(&keep).await.unwrap();

        assert!(kept_ws.exists(), "in-flight worktree survives the sweep");
        assert!(!orphan_ws.exists(), "orphaned worktree is swept");
    }
}
