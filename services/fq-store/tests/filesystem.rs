//! The filesystem backend, proven against the shared `ContentStore`
//! conformance suite. A future backend re-runs this exact suite by invoking
//! `content_store_conformance!` the same way.

use fq_store::content_store_conformance;
use fq_store::fs::{ChunkParams, FilesystemStore};

// Small chunk parameters so even modest property inputs produce many blocks,
// exercising multi-block objects, dedup, and cross-block range reads.
content_store_conformance!(FilesystemStore::with_params(
    tempfile::tempdir().unwrap().keep(),
    ChunkParams {
        min: 256,
        avg: 1024,
        max: 4096,
    },
));
