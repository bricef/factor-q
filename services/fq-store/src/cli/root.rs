//! Opening the store's databases under the root directory, and what a
//! command does when that directory is not there.
//!
//! The library has no notion of a root: [`SqliteNameIndex::open`] and
//! [`SqliteGrantLog::open`] take a database file, and SQLite creates the
//! file but never the directory it goes in. This module is where `--root`
//! becomes `root/index.db` and `root/grants.db`, and it is the only way the
//! CLI opens either, so no command opens a database without having said
//! what it does about a missing root. `tests/cli_open_gate.rs` fails on any
//! other open in the CLI's sources.
//!
//! The rule is the one the content-addressed commands already follow:
//! writing creates, reading does not. `object put` and `grant add`, the two
//! database commands that can be the first thing done to a new store,
//! create the root, parents included, as `put` does. Every other database
//! command exits naming the directory and creates nothing; the commands
//! that open no database (`put`, `get`, `has`, `size`, `metrics`, `serve`
//! and the key and offline token commands) do not come here. A missing root is
//! almost always a mistyped `--root` or the default root from the wrong
//! working directory, and creating it would make `object ls` print
//! nothing, `grant check` answer `denied`, `gc` report that every invariant
//! holds and `token mint` mint a token with no grants, each exiting as it
//! would for a real, empty store.

use std::io::ErrorKind;
use std::path::Path;

use clap::parser::ValueSource;

use super::{CliResult, GrantCommand, ObjectCommand, parse_scope, parse_verbs, principal};
use crate::fs::FilesystemStore;
use crate::{Repository, SqliteGrantLog, SqliteNameIndex};

/// What a command that opens a database does when the root does not exist.
pub(super) enum MissingRoot {
    /// Create it, parents included.
    Create,
    /// Exit naming it, having created nothing.
    Refuse,
}

impl ObjectCommand {
    /// `object put` creates a missing root, once its input is known to be
    /// there, so a mistyped input path leaves no store behind. The other
    /// object commands refuse one.
    pub(super) async fn missing_root(&self) -> CliResult<MissingRoot> {
        match self {
            ObjectCommand::Put { path, .. } => {
                if path != "-" {
                    tokio::fs::metadata(path).await?;
                }
                Ok(MissingRoot::Create)
            }
            ObjectCommand::Get { .. }
            | ObjectCommand::Ls { .. }
            | ObjectCommand::Rm { .. }
            | ObjectCommand::Resolve { .. }
            | ObjectCommand::History { .. }
            | ObjectCommand::Bind { .. } => Ok(MissingRoot::Refuse),
        }
    }
}

impl GrantCommand {
    /// `grant add` creates a missing root, once its arguments parse, so a
    /// typo leaves no store behind. The other grant commands refuse one.
    pub(super) fn missing_root(&self) -> CliResult<MissingRoot> {
        match self {
            GrantCommand::Add {
                agent,
                verbs,
                scope,
            } => {
                principal(agent)?;
                parse_verbs(verbs)?;
                parse_scope(scope)?;
                Ok(MissingRoot::Create)
            }
            GrantCommand::Ls { .. } | GrantCommand::Check { .. } | GrantCommand::Rm { .. } => {
                Ok(MissingRoot::Refuse)
            }
        }
    }
}

/// The store root, and where it came from: `--root`, `FQ_CAS_ROOT`, or
/// the default.
pub(super) struct StoreRoot<'a> {
    path: &'a Path,
    source: Option<ValueSource>,
}

impl<'a> StoreRoot<'a> {
    pub(super) fn new(path: &'a Path, source: Option<ValueSource>) -> Self {
        Self { path, source }
    }

    /// The named-object repository: the content store and `index.db`.
    pub(super) async fn repository(
        &self,
        missing: MissingRoot,
    ) -> CliResult<Repository<FilesystemStore, SqliteNameIndex>> {
        self.prepare(missing).await?;
        Ok(Repository::new(
            FilesystemStore::new(self.path),
            SqliteNameIndex::open(self.path.join("index.db")).await?,
        ))
    }

    /// The grant log, `grants.db`.
    pub(super) async fn grant_log(&self, missing: MissingRoot) -> CliResult<SqliteGrantLog> {
        self.prepare(missing).await?;
        Ok(SqliteGrantLog::open(self.path.join("grants.db")).await?)
    }

    /// Leave the root a directory a database can be opened in, or say why
    /// it is not one.
    async fn prepare(&self, missing: MissingRoot) -> CliResult<()> {
        let why = match tokio::fs::metadata(self.path).await {
            Ok(meta) if meta.is_dir() => return Ok(()),
            Ok(_) => format!("the store root {} is not a directory", self.describe()),
            // Not found may be a link to nothing, which no command can create
            // through, so it gets no advice to start a store.
            Err(e) if e.kind() == ErrorKind::NotFound => {
                match (tokio::fs::read_link(self.path).await, missing) {
                    (Ok(target), _) => format!(
                        "the store root {} is a symbolic link to {}, which does not exist",
                        self.describe(),
                        target.display()
                    ),
                    (Err(_), MissingRoot::Create) => return self.ensure_root_exists().await,
                    (Err(_), MissingRoot::Refuse) => format!(
                        "the store root {} does not exist. Point --root or FQ_CAS_ROOT at an \
                         existing store, or start a new one with `fq-cas object put` or \
                         `fq-cas grant add`.",
                        self.describe()
                    ),
                }
            }
            Err(e) => format!("cannot use the store root {}: {e}", self.describe()),
        };
        Err(why.into())
    }

    /// Create the store root, parents included, as `put` does when it first
    /// writes.
    async fn ensure_root_exists(&self) -> CliResult<()> {
        tokio::fs::create_dir_all(self.path)
            .await
            .map_err(|e| format!("cannot create the store root {}: {e}", self.describe()).into())
    }

    /// The root as an absolute path, so that the default names the working
    /// directory, and where it came from.
    fn describe(&self) -> String {
        let path = std::path::absolute(self.path).unwrap_or_else(|_| self.path.to_path_buf());
        let source = match self.source {
            Some(ValueSource::CommandLine) => "from --root",
            Some(ValueSource::EnvVariable) => "from FQ_CAS_ROOT",
            _ => "the default, in the working directory",
        };
        format!("{} ({source})", path.display())
    }
}
