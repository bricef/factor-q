//! Shared scaffolding for the crate's unit tests (compiled only under `test`).

use crate::fs::{ChunkParams, FilesystemStore};
use crate::{Repository, SqliteNameIndex};

/// A fresh repository over a temp directory, with [`ChunkParams::small`] so tests
/// exercise the multi-block and shared-block paths. Keep the returned `TempDir`
/// alive for the repository's lifetime.
pub(crate) async fn repo() -> (
    tempfile::TempDir,
    Repository<FilesystemStore, SqliteNameIndex>,
) {
    let dir = tempfile::tempdir().unwrap();
    let cas = dir.path().join("cas");
    std::fs::create_dir_all(&cas).unwrap();
    let store = FilesystemStore::with_params(cas, ChunkParams::small());
    let index = SqliteNameIndex::open(dir.path().join("index.db"))
        .await
        .unwrap();
    (dir, Repository::new(store, index))
}
