//! The replay floor against the pinned broker: committed v2 history
//! (the #646 corpus) followed by current events, and the shapes the
//! search has to get right — all readable, none readable, an empty
//! stream, gaps and poison at the boundary, and every boundary
//! position of a stream the halving has to land on exactly.

use super::*;
use crate::bus::{EventBus, STREAM_NAME};
use crate::test_support::corpus::{corpus_bytes, publish_corpus, publish_raw};

async fn connect() -> (fq_test_support::NatsServer, EventBus) {
    let server = crate::test_support::nats::test_nats();
    let bus = EventBus::connect(server.url()).await.expect("connect NATS");
    (server, bus)
}

async fn floor_of(bus: &EventBus) -> ReplayFloor {
    let mut stream = bus.jetstream().get_stream(STREAM_NAME).await.unwrap();
    replay_floor(&mut stream).await.expect("the floor is found")
}

/// The issue's stream: v2 history, then current events. The floor is
/// the first current message; what lies below it is skipped.
#[tokio::test]
async fn older_history_then_current_events_floors_at_the_first_readable_message() {
    let (_server, bus) = connect().await;
    let older = publish_corpus(&bus, "v2").await;
    let current = publish_corpus(&bus, "v3").await;

    let floor = floor_of(&bus).await;
    assert_eq!(floor.floor, current[0], "the first v3 message: {floor:?}");
    assert_eq!(floor.first_sequence, older[0]);
    assert_eq!(floor.last_sequence, *current.last().unwrap());
    assert!(floor.skips_history());
    assert!(!floor.nothing_to_replay());
    assert_eq!(floor.deliver_from(), DeliverFrom::Sequence(current[0]));
}

/// A stream this build reads whole floors at its first sequence:
/// nothing is skipped, and the replay is the replay it always was.
#[tokio::test]
async fn an_all_readable_stream_floors_at_its_first_sequence() {
    let (_server, bus) = connect().await;
    let current = publish_corpus(&bus, "v3").await;

    let floor = floor_of(&bus).await;
    assert_eq!(floor.floor, current[0], "{floor:?}");
    assert_eq!(floor.floor, floor.first_sequence);
    assert!(!floor.skips_history());
    assert!(!floor.nothing_to_replay());
}

/// A stream this build reads none of floors past its last sequence:
/// there is nothing to replay, and the durable starts where the next
/// message will land.
#[tokio::test]
async fn an_all_unreadable_stream_floors_past_its_last_sequence() {
    let (_server, bus) = connect().await;
    let older = publish_corpus(&bus, "v2").await;

    let floor = floor_of(&bus).await;
    assert_eq!(floor.floor, older.last().unwrap() + 1, "{floor:?}");
    assert!(floor.nothing_to_replay());
    assert!(floor.skips_history());
    assert_eq!(
        floor.deliver_from(),
        DeliverFrom::Sequence(older.last().unwrap() + 1)
    );
}

/// An empty stream has no floor to find: the replay starts where the
/// next message will land, and a start there delivers it.
#[tokio::test]
async fn an_empty_stream_floors_at_the_next_sequence() {
    let (_server, bus) = connect().await;

    let floor = floor_of(&bus).await;
    assert_eq!(floor.floor, floor.last_sequence + 1, "{floor:?}");
    assert!(floor.nothing_to_replay());
    assert!(
        floor.deliver_from() != DeliverFrom::New,
        "a floor is a position, never a from-new start: {floor:?}"
    );

    // The first message published lands at the floor.
    let (_, bytes) = corpus_bytes("v3").swap_remove(0);
    let seq = publish_raw(&bus, bytes).await;
    assert_eq!(seq, floor.floor, "the next message lands at the floor");
}

/// A deleted sequence or poison at the boundary is probed forward: the
/// floor is the first *readable message*, never a position the stream
/// no longer holds or one the consumer would ack and skip.
#[tokio::test]
async fn a_gap_or_poison_at_the_boundary_is_probed_forward() {
    let (_server, bus) = connect().await;
    let v2 = corpus_bytes("v2");
    let v3 = corpus_bytes("v3");
    let older_a = publish_raw(&bus, v2[0].1.clone()).await;
    let older_b = publish_raw(&bus, v2[1].1.clone()).await;
    let poison = publish_raw(&bus, b"not an event in any version".to_vec()).await;
    let deleted = publish_raw(&bus, v3[0].1.clone()).await;
    let first_readable = publish_raw(&bus, v3[1].1.clone()).await;
    let _after = publish_raw(&bus, v3[2].1.clone()).await;
    assert_eq!(
        [older_a, older_b, poison, deleted, first_readable],
        [1, 2, 3, 4, 5],
        "the private stream numbers from one"
    );

    // Before anything is deleted: the poison at 3 is stepped over and
    // the floor is the v3 message at 4.
    assert_eq!(floor_of(&bus).await.floor, deleted);

    let stream = bus.jetstream().get_stream(STREAM_NAME).await.unwrap();
    assert!(stream.delete_message(deleted).await.unwrap());
    assert_eq!(
        floor_of(&bus).await.floor,
        first_readable,
        "the gap where the first v3 message was is probed forward"
    );

    // A gap on the older side of the boundary changes nothing.
    assert!(stream.delete_message(older_b).await.unwrap());
    assert_eq!(floor_of(&bus).await.floor, first_readable);
}

/// The halving lands exactly, whatever the boundary: on one private
/// stream, purged between cases, every split of sixteen messages into
/// older-then-current floors at the first current message, or past
/// the end when there is none.
#[tokio::test]
async fn the_search_lands_on_every_boundary() {
    let (_server, bus) = connect().await;
    let v2 = corpus_bytes("v2");
    let v3 = corpus_bytes("v3");
    let stream = bus.jetstream().get_stream(STREAM_NAME).await.unwrap();
    const N: usize = 16;
    for older in [0usize, 1, 2, 7, 8, 9, 15, 16] {
        stream.purge().await.expect("purge");
        let mut seqs = Vec::with_capacity(N);
        for i in 0..N {
            let bytes = if i < older {
                v2[i % v2.len()].1.clone()
            } else {
                v3[i % v3.len()].1.clone()
            };
            seqs.push(publish_raw(&bus, bytes).await);
        }
        let floor = floor_of(&bus).await;
        let expected = if older < N {
            seqs[older]
        } else {
            seqs[N - 1] + 1
        };
        assert_eq!(
            floor.floor,
            expected,
            "{older} older messages before {} current ones: {floor:?}",
            N - older
        );
        assert_eq!(floor.first_sequence, seqs[0]);
        assert_eq!(floor.last_sequence, seqs[N - 1]);
    }
}
