//! The committed event corpus, read for tests that need history in a
//! version this build does not read — the same files
//! `tests/event_corpus.rs` replays through the parse boundary, here
//! published raw onto a private broker's stream so a replay can meet
//! them where a production stream would hold them.

use std::path::{Path, PathBuf};

use crate::bus::EventBus;

/// Where the corpus for `version` (`"v1"`, `"v2"`, `"v3"`) lives.
pub fn corpus_dir(version: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/corpus/events")
        .join(version)
}

/// Every corpus file for `version`, as `(file name, bytes)`, in name
/// order so a test reads them in one order everywhere.
pub fn corpus_bytes(version: &str) -> Vec<(String, Vec<u8>)> {
    let dir = corpus_dir(version);
    let mut files: Vec<PathBuf> = std::fs::read_dir(&dir)
        .unwrap_or_else(|e| panic!("corpus dir {dir:?}: {e}"))
        .map(|entry| entry.unwrap().path())
        .filter(|p| p.extension().is_some_and(|ext| ext == "json"))
        .collect();
    files.sort();
    assert!(!files.is_empty(), "the {version} corpus is empty");
    files
        .into_iter()
        .map(|path| {
            let name = path.file_name().unwrap().to_string_lossy().into_owned();
            let bytes = std::fs::read(&path).unwrap_or_else(|e| panic!("{path:?}: {e}"));
            (name, bytes)
        })
        .collect()
}

/// The subject corpus files are published on: inside the event
/// stream's capture, and nothing a consumer filters on.
pub const CORPUS_SUBJECT: &str = "fq.agent.corpus-agent.corpus";

/// Publish `bytes` as they are — no envelope, no version stamp — and
/// return the stream sequence they landed at.
pub async fn publish_raw(bus: &EventBus, bytes: Vec<u8>) -> u64 {
    bus.jetstream()
        .publish(CORPUS_SUBJECT, bytes::Bytes::from(bytes))
        .await
        .expect("publish raw bytes")
        .await
        .expect("raw bytes stored")
        .sequence
}

/// Publish every file of `version`'s corpus, in order; the sequences.
pub async fn publish_corpus(bus: &EventBus, version: &str) -> Vec<u64> {
    let mut seqs = Vec::new();
    for (_, bytes) in corpus_bytes(version) {
        seqs.push(publish_raw(bus, bytes).await);
    }
    seqs
}
