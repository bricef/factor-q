//! Unit tests for [`super`]. Extracted from the parent module so the
//! file that ships is the file you read (#390); `super::*` keeps the
//! same access it had inline.

use super::*;
use crate::fs::ChunkParams;

#[tokio::test]
async fn read_full_and_ranges() {
    let dir = tempfile::tempdir().unwrap();
    let store = FilesystemStore::with_params(dir.path(), ChunkParams::small());
    let content: Vec<u8> = (0..5000u32).map(|i| i as u8).collect();
    let cid = store.put(&content).await.unwrap();

    assert_eq!(read(&store, &cid, None, None).await.unwrap(), content);
    assert_eq!(
        read(&store, &cid, Some(100), Some(50)).await.unwrap(),
        &content[100..150]
    );
    assert_eq!(
        read(&store, &cid, Some(4000), None).await.unwrap(),
        &content[4000..]
    );
    assert!(
        read(&store, &cid, Some(99999), None)
            .await
            .unwrap()
            .is_empty()
    );
}
