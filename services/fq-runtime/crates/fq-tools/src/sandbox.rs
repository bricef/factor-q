//! Runtime sandbox enforcement for tool invocations.
//!
//! `ToolSandbox` is the runtime counterpart to the declarative
//! `Sandbox` in the agent definition. When a tool is about to touch
//! the filesystem it asks the sandbox whether the target path is
//! allowed; the sandbox canonicalises both the target and the
//! configured prefixes before comparing, so path traversal (`..`),
//! `.` components, and symlinks all resolve to their real locations.
//!
//! # Security properties
//!
//! - **Path traversal** is defeated by full canonicalisation. A path
//!   containing `..` is resolved before comparison, so
//!   `/data/docs/../../../etc/passwd` evaluates to `/etc/passwd` and
//!   fails the containment check.
//! - **Symlinks** are followed during canonicalisation, so a symlink
//!   at `/data/docs/evil` that points at `/etc/passwd` resolves to
//!   `/etc/passwd` and fails.
//! - **Write targets that don't yet exist** cannot be canonicalised
//!   directly, so we canonicalise the parent directory instead and
//!   append the filename. This means the parent must already exist
//!   and must itself be within an allowed write prefix.
//!
//! # Known limitations
//!
//! - **TOCTOU**: nothing stops the filesystem from mutating between
//!   the check and the open. An attacker with concurrent write access
//!   to the allowed directories could swap in a symlink after the
//!   check. Process-level protection is inherently racy; full
//!   isolation requires OS primitives (namespaces, seccomp) or
//!   container-level sandboxing (see ADR-0010).
//! - The sandbox does not mount or chroot — tools run in the parent
//!   process and rely on their own enforcement. Treat the sandbox as
//!   a first-line defence, not a last line.

use std::path::{Path, PathBuf};

/// Runtime sandbox declaring which paths a tool may read, write, or
/// execute commands in.
#[derive(Debug, Clone, Default)]
pub struct ToolSandbox {
    fs_read: Vec<PathBuf>,
    fs_write: Vec<PathBuf>,
    exec_cwd: Vec<PathBuf>,
}

impl ToolSandbox {
    /// Create an empty sandbox. Nothing is allowed until paths are
    /// added via `allow_read` / `allow_write`.
    pub fn new() -> Self {
        Self::default()
    }

    /// Grant read access to a path prefix. The path is stored as
    /// given; canonicalisation happens at check time.
    pub fn allow_read(mut self, path: impl Into<PathBuf>) -> Self {
        self.fs_read.push(path.into());
        self
    }

    /// Grant write access to a path prefix.
    pub fn allow_write(mut self, path: impl Into<PathBuf>) -> Self {
        self.fs_write.push(path.into());
        self
    }

    /// Grant exec access — allow commands to be run with this path as
    /// their working directory. Distinct from read/write: an agent
    /// with read or write access to a directory does NOT automatically
    /// get permission to execute commands there.
    pub fn allow_exec_cwd(mut self, path: impl Into<PathBuf>) -> Self {
        self.exec_cwd.push(path.into());
        self
    }

    pub fn read_prefixes(&self) -> &[PathBuf] {
        &self.fs_read
    }

    pub fn write_prefixes(&self) -> &[PathBuf] {
        &self.fs_write
    }

    pub fn exec_cwd_prefixes(&self) -> &[PathBuf] {
        &self.exec_cwd
    }

    /// Check that a target path is allowed for reading.
    ///
    /// Returns the canonicalised target path on success. On failure,
    /// classifies the outcome as either `NotFound` (the path itself
    /// does not exist) or `PermissionDenied` (the path exists or its
    /// parent exists but resolves outside every allowed prefix).
    pub fn check_read(&self, target: &Path) -> Result<PathBuf, SandboxError> {
        if self.fs_read.is_empty() {
            return Err(SandboxError::PermissionDenied {
                target: target.to_path_buf(),
                reason: "no read prefixes configured".to_string(),
            });
        }

        let canonical = canonicalise_existing(target).map_err(|err| match err.kind() {
            std::io::ErrorKind::NotFound => SandboxError::NotFound(target.to_path_buf()),
            _ => SandboxError::Io {
                path: target.to_path_buf(),
                source: err,
            },
        })?;
        self.check_within(&canonical, &self.fs_read, target)
    }

    /// Check that a target directory is allowed as a command's working
    /// directory. The target must already exist (you can't run a
    /// process in a non-existent directory), and is canonicalised
    /// before comparison against the allowed prefixes. This is the
    /// first and only check for the shell tool — nothing else about
    /// the command's execution is validated here.
    pub fn check_exec_cwd(&self, target: &Path) -> Result<PathBuf, SandboxError> {
        if self.exec_cwd.is_empty() {
            return Err(SandboxError::PermissionDenied {
                target: target.to_path_buf(),
                reason: "no exec_cwd prefixes configured".to_string(),
            });
        }

        let canonical = canonicalise_existing(target).map_err(|err| match err.kind() {
            std::io::ErrorKind::NotFound => SandboxError::NotFound(target.to_path_buf()),
            _ => SandboxError::Io {
                path: target.to_path_buf(),
                source: err,
            },
        })?;

        if !canonical.is_dir() {
            return Err(SandboxError::InvalidPath {
                target: target.to_path_buf(),
                reason: "exec cwd must be a directory".to_string(),
            });
        }

        self.check_within(&canonical, &self.exec_cwd, target)
    }

    /// Check that a target path is allowed for writing.
    ///
    /// The target does not need to exist. If it exists, its
    /// canonical form is compared to the allowed write prefixes; if
    /// it does not, the parent directory is canonicalised and the
    /// filename appended, giving the would-be path of the new file.
    pub fn check_write(&self, target: &Path) -> Result<PathBuf, SandboxError> {
        if self.fs_write.is_empty() {
            return Err(SandboxError::PermissionDenied {
                target: target.to_path_buf(),
                reason: "no write prefixes configured".to_string(),
            });
        }

        let canonical = canonicalise_for_write(target)?;
        self.check_within(&canonical, &self.fs_write, target)
    }

    fn check_within(
        &self,
        canonical: &Path,
        prefixes: &[PathBuf],
        original: &Path,
    ) -> Result<PathBuf, SandboxError> {
        for prefix in prefixes {
            let canonical_prefix = match std::fs::canonicalize(prefix) {
                Ok(p) => p,
                Err(_) => continue, // A prefix that doesn't exist cannot contain anything.
            };
            if canonical.starts_with(&canonical_prefix) {
                return Ok(canonical.to_path_buf());
            }
        }
        Err(SandboxError::PermissionDenied {
            target: original.to_path_buf(),
            reason: format!(
                "resolved path {} is outside every allowed prefix",
                canonical.display()
            ),
        })
    }
}

/// Canonicalise a path that must already exist.
fn canonicalise_existing(target: &Path) -> std::io::Result<PathBuf> {
    std::fs::canonicalize(target)
}

/// Canonicalise a path that may not yet exist. Used for write checks.
///
/// If the target exists, canonicalise it directly. Otherwise
/// canonicalise the parent directory and join the filename. The
/// parent must already exist — we don't speculatively create
/// directories during sandbox checks.
fn canonicalise_for_write(target: &Path) -> Result<PathBuf, SandboxError> {
    if target.exists() {
        return std::fs::canonicalize(target).map_err(|err| SandboxError::Io {
            path: target.to_path_buf(),
            source: err,
        });
    }
    let parent = target.parent().ok_or_else(|| SandboxError::InvalidPath {
        target: target.to_path_buf(),
        reason: "path has no parent directory".to_string(),
    })?;
    let filename = target.file_name().ok_or_else(|| SandboxError::InvalidPath {
        target: target.to_path_buf(),
        reason: "path has no final component".to_string(),
    })?;
    // An empty parent means the target is relative with no directory
    // component, e.g. `"foo.txt"`. Treat the current directory as the
    // parent in that case.
    let parent = if parent.as_os_str().is_empty() {
        Path::new(".")
    } else {
        parent
    };
    let canonical_parent = std::fs::canonicalize(parent).map_err(|err| match err.kind() {
        std::io::ErrorKind::NotFound => SandboxError::NotFound(parent.to_path_buf()),
        _ => SandboxError::Io {
            path: parent.to_path_buf(),
            source: err,
        },
    })?;
    Ok(canonical_parent.join(filename))
}

/// Errors from sandbox checks.
#[derive(Debug, thiserror::Error)]
pub enum SandboxError {
    #[error("permission denied: {target:?}: {reason}")]
    PermissionDenied { target: PathBuf, reason: String },

    #[error("path not found: {0:?}")]
    NotFound(PathBuf),

    #[error("invalid path: {target:?}: {reason}")]
    InvalidPath { target: PathBuf, reason: String },

    #[error("io error for {path:?}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::fs::symlink;
    use tempfile::tempdir;

    fn write_file(dir: &Path, rel: &str, contents: &str) -> PathBuf {
        let path = dir.join(rel);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(&path, contents).unwrap();
        path
    }

    fn make_sandbox(allowed_read: &[&Path], allowed_write: &[&Path]) -> ToolSandbox {
        let mut sb = ToolSandbox::new();
        for p in allowed_read {
            sb = sb.allow_read(*p);
        }
        for p in allowed_write {
            sb = sb.allow_write(*p);
        }
        sb
    }

    fn make_exec_sandbox(allowed_exec: &[&Path]) -> ToolSandbox {
        let mut sb = ToolSandbox::new();
        for p in allowed_exec {
            sb = sb.allow_exec_cwd(*p);
        }
        sb
    }

    // --- read checks --------------------------------------------------

    #[test]
    fn read_within_allowed_prefix_is_ok() {
        let dir = tempdir().unwrap();
        let file = write_file(dir.path(), "docs/hello.md", "hi");
        let sb = make_sandbox(&[dir.path()], &[]);
        let canonical = sb.check_read(&file).unwrap();
        assert_eq!(canonical, fs::canonicalize(&file).unwrap());
    }

    #[test]
    fn read_outside_allowed_prefix_is_denied() {
        let allowed = tempdir().unwrap();
        let other = tempdir().unwrap();
        let file = write_file(other.path(), "secret.txt", "no");
        let sb = make_sandbox(&[allowed.path()], &[]);
        let err = sb.check_read(&file).unwrap_err();
        assert!(matches!(err, SandboxError::PermissionDenied { .. }));
    }

    #[test]
    fn read_with_parent_traversal_staying_inside_is_ok() {
        let dir = tempdir().unwrap();
        let _ = write_file(dir.path(), "a/b/target.txt", "x");
        let sb = make_sandbox(&[dir.path()], &[]);
        let traversal = dir.path().join("a/b/../b/target.txt");
        assert!(sb.check_read(&traversal).is_ok());
    }

    #[test]
    fn read_with_parent_traversal_escaping_is_denied() {
        let allowed = tempdir().unwrap();
        let other = tempdir().unwrap();
        let _ = write_file(other.path(), "secret.txt", "no");
        let sb = make_sandbox(&[allowed.path()], &[]);
        let escape = allowed.path().join("../").join(
            other
                .path()
                .file_name()
                .unwrap()
                .to_string_lossy()
                .to_string(),
        );
        let escape = escape.join("secret.txt");
        let err = sb.check_read(&escape).unwrap_err();
        assert!(matches!(err, SandboxError::PermissionDenied { .. }));
    }

    #[test]
    fn read_through_symlink_pointing_inside_is_ok() {
        let dir = tempdir().unwrap();
        let real = write_file(dir.path(), "real.txt", "hi");
        let link = dir.path().join("link.txt");
        symlink(&real, &link).unwrap();
        let sb = make_sandbox(&[dir.path()], &[]);
        assert!(sb.check_read(&link).is_ok());
    }

    #[test]
    fn read_through_symlink_pointing_outside_is_denied() {
        let allowed = tempdir().unwrap();
        let other = tempdir().unwrap();
        let outside = write_file(other.path(), "outside.txt", "no");
        let link = allowed.path().join("escape");
        symlink(&outside, &link).unwrap();
        let sb = make_sandbox(&[allowed.path()], &[]);
        let err = sb.check_read(&link).unwrap_err();
        assert!(matches!(err, SandboxError::PermissionDenied { .. }));
    }

    #[test]
    fn read_nonexistent_file_reports_not_found_not_denied() {
        let dir = tempdir().unwrap();
        let missing = dir.path().join("ghost.txt");
        let sb = make_sandbox(&[dir.path()], &[]);
        let err = sb.check_read(&missing).unwrap_err();
        assert!(matches!(err, SandboxError::NotFound(_)));
    }

    #[test]
    fn empty_read_prefix_list_denies_everything() {
        let dir = tempdir().unwrap();
        let file = write_file(dir.path(), "hi.txt", "hi");
        let sb = ToolSandbox::new();
        let err = sb.check_read(&file).unwrap_err();
        assert!(matches!(err, SandboxError::PermissionDenied { .. }));
    }

    #[test]
    fn multiple_read_prefixes_any_may_match() {
        let a = tempdir().unwrap();
        let b = tempdir().unwrap();
        let file_a = write_file(a.path(), "a.txt", "a");
        let file_b = write_file(b.path(), "b.txt", "b");
        let sb = make_sandbox(&[a.path(), b.path()], &[]);
        assert!(sb.check_read(&file_a).is_ok());
        assert!(sb.check_read(&file_b).is_ok());
    }

    // --- write checks -------------------------------------------------

    #[test]
    fn write_new_file_inside_is_ok() {
        let dir = tempdir().unwrap();
        let sb = make_sandbox(&[], &[dir.path()]);
        let target = dir.path().join("new.txt");
        let resolved = sb.check_write(&target).unwrap();
        assert_eq!(resolved, fs::canonicalize(dir.path()).unwrap().join("new.txt"));
    }

    #[test]
    fn write_new_file_outside_is_denied() {
        let allowed = tempdir().unwrap();
        let other = tempdir().unwrap();
        let sb = make_sandbox(&[], &[allowed.path()]);
        let target = other.path().join("new.txt");
        let err = sb.check_write(&target).unwrap_err();
        assert!(matches!(err, SandboxError::PermissionDenied { .. }));
    }

    #[test]
    fn write_existing_file_inside_is_ok() {
        let dir = tempdir().unwrap();
        let file = write_file(dir.path(), "existing.txt", "old");
        let sb = make_sandbox(&[], &[dir.path()]);
        assert!(sb.check_write(&file).is_ok());
    }

    #[test]
    fn write_existing_file_outside_is_denied() {
        let allowed = tempdir().unwrap();
        let other = tempdir().unwrap();
        let file = write_file(other.path(), "existing.txt", "no");
        let sb = make_sandbox(&[], &[allowed.path()]);
        let err = sb.check_write(&file).unwrap_err();
        assert!(matches!(err, SandboxError::PermissionDenied { .. }));
    }

    #[test]
    fn write_to_nonexistent_parent_is_not_found() {
        let dir = tempdir().unwrap();
        let sb = make_sandbox(&[], &[dir.path()]);
        let target = dir.path().join("nonexistent/deeper/new.txt");
        let err = sb.check_write(&target).unwrap_err();
        assert!(matches!(err, SandboxError::NotFound(_)));
    }

    #[test]
    fn write_through_escaping_traversal_is_denied() {
        let allowed = tempdir().unwrap();
        let other = tempdir().unwrap();
        let sb = make_sandbox(&[], &[allowed.path()]);
        let escape = allowed
            .path()
            .join("../")
            .join(other.path().file_name().unwrap())
            .join("new.txt");
        let err = sb.check_write(&escape).unwrap_err();
        assert!(matches!(err, SandboxError::PermissionDenied { .. }));
    }

    #[test]
    fn write_existing_symlink_pointing_outside_is_denied() {
        let allowed = tempdir().unwrap();
        let other = tempdir().unwrap();
        let outside = write_file(other.path(), "outside.txt", "no");
        let link = allowed.path().join("escape");
        symlink(&outside, &link).unwrap();
        let sb = make_sandbox(&[], &[allowed.path()]);
        let err = sb.check_write(&link).unwrap_err();
        assert!(matches!(err, SandboxError::PermissionDenied { .. }));
    }

    #[test]
    fn empty_write_prefix_list_denies_everything() {
        let dir = tempdir().unwrap();
        let target = dir.path().join("new.txt");
        let sb = ToolSandbox::new();
        let err = sb.check_write(&target).unwrap_err();
        assert!(matches!(err, SandboxError::PermissionDenied { .. }));
    }

    // --- cross-contamination ----------------------------------------

    #[test]
    fn read_access_does_not_imply_write() {
        let dir = tempdir().unwrap();
        let _ = write_file(dir.path(), "hi.txt", "hi");
        let sb = make_sandbox(&[dir.path()], &[]);
        let target = dir.path().join("new.txt");
        let err = sb.check_write(&target).unwrap_err();
        assert!(matches!(err, SandboxError::PermissionDenied { .. }));
    }

    #[test]
    fn write_access_does_not_imply_read() {
        let dir = tempdir().unwrap();
        let file = write_file(dir.path(), "hi.txt", "hi");
        let sb = make_sandbox(&[], &[dir.path()]);
        let err = sb.check_read(&file).unwrap_err();
        assert!(matches!(err, SandboxError::PermissionDenied { .. }));
    }

    // --- exec cwd checks --------------------------------------------

    #[test]
    fn exec_cwd_within_allowed_prefix_is_ok() {
        let dir = tempdir().unwrap();
        let sub = dir.path().join("work");
        fs::create_dir_all(&sub).unwrap();
        let sb = make_exec_sandbox(&[dir.path()]);
        let canonical = sb.check_exec_cwd(&sub).unwrap();
        assert_eq!(canonical, fs::canonicalize(&sub).unwrap());
    }

    #[test]
    fn exec_cwd_outside_allowed_prefix_is_denied() {
        let allowed = tempdir().unwrap();
        let other = tempdir().unwrap();
        let sb = make_exec_sandbox(&[allowed.path()]);
        let err = sb.check_exec_cwd(other.path()).unwrap_err();
        assert!(matches!(err, SandboxError::PermissionDenied { .. }));
    }

    #[test]
    fn exec_cwd_with_parent_traversal_escape_is_denied() {
        let allowed = tempdir().unwrap();
        let other = tempdir().unwrap();
        let sb = make_exec_sandbox(&[allowed.path()]);
        let escape = allowed
            .path()
            .join("../")
            .join(other.path().file_name().unwrap());
        let err = sb.check_exec_cwd(&escape).unwrap_err();
        assert!(matches!(err, SandboxError::PermissionDenied { .. }));
    }

    #[test]
    fn exec_cwd_non_directory_is_rejected() {
        let dir = tempdir().unwrap();
        let file = write_file(dir.path(), "not-a-dir.txt", "hi");
        let sb = make_exec_sandbox(&[dir.path()]);
        let err = sb.check_exec_cwd(&file).unwrap_err();
        assert!(matches!(err, SandboxError::InvalidPath { .. }));
    }

    #[test]
    fn exec_cwd_missing_directory_is_not_found() {
        let dir = tempdir().unwrap();
        let missing = dir.path().join("ghost");
        let sb = make_exec_sandbox(&[dir.path()]);
        let err = sb.check_exec_cwd(&missing).unwrap_err();
        assert!(matches!(err, SandboxError::NotFound(_)));
    }

    #[test]
    fn exec_cwd_through_symlink_pointing_outside_is_denied() {
        let allowed = tempdir().unwrap();
        let other = tempdir().unwrap();
        let link = allowed.path().join("escape");
        symlink(other.path(), &link).unwrap();
        let sb = make_exec_sandbox(&[allowed.path()]);
        let err = sb.check_exec_cwd(&link).unwrap_err();
        assert!(matches!(err, SandboxError::PermissionDenied { .. }));
    }

    #[test]
    fn empty_exec_prefix_list_denies_everything() {
        let dir = tempdir().unwrap();
        let sb = ToolSandbox::new();
        let err = sb.check_exec_cwd(dir.path()).unwrap_err();
        assert!(matches!(err, SandboxError::PermissionDenied { .. }));
    }

    #[test]
    fn read_access_does_not_imply_exec() {
        let dir = tempdir().unwrap();
        let sb = make_sandbox(&[dir.path()], &[]);
        let err = sb.check_exec_cwd(dir.path()).unwrap_err();
        assert!(matches!(err, SandboxError::PermissionDenied { .. }));
    }

    #[test]
    fn write_access_does_not_imply_exec() {
        let dir = tempdir().unwrap();
        let sb = make_sandbox(&[], &[dir.path()]);
        let err = sb.check_exec_cwd(dir.path()).unwrap_err();
        assert!(matches!(err, SandboxError::PermissionDenied { .. }));
    }
}
