//! The check for a dead letter already written, asked directly, against a
//! real broker, for what [`AdvisoryWatch::handle_advisory`] cannot be made
//! to meet on cue: the broker failing between the read of the trigger's
//! message, which comes first, and the check's own reads.

use crate::bus::EventBus;

use super::super::named_once_tests::{broker, dispatcher_dead_letter, trigger, untagged_trigger};
use super::super::test_kit::exhaust;
use super::*;

/// The advisory `exhaust` handed back, as the watch reads one.
fn advisory(raw: &[u8]) -> MaxDeliverAdvisory {
    serde_json::from_slice(raw).expect("the broker's advisory")
}

/// `agent`'s trigger at `seq` of `bus`'s trigger stream, as the check is
/// handed one the watch has read: named `id`, or with no id.
async fn named(bus: &EventBus, agent: &str, seq: u64, id: Option<Uuid>) -> NamedTrigger {
    let published = bus
        .jetstream()
        .get_stream(TRIGGER_STREAM_NAME)
        .await
        .expect("the trigger stream")
        .get_raw_message(seq)
        .await
        .expect("the trigger's message")
        .time;
    NamedTrigger {
        agent: AgentId::new(agent).unwrap(),
        id,
        published,
    }
}

/// **A failed read of the trigger stream's creation never answers
/// "already written".** An id-less trigger's dead letter is on the log,
/// recording the incarnation the watch holds, so the check finds it, and
/// with no trigger id to confirm the match asks the broker whether the
/// stream is still that incarnation. The stream is gone by then, so the
/// broker cannot say. Inside the watch's limit that is an error, for the
/// advisory to be decided again; past it the check is unmade, and the dead
/// letter is written saying why. Taking the failed read as "still this
/// incarnation" would answer "already written" on a guess, the one answer
/// the check must not give unless it is certain.
#[tokio::test]
async fn a_failed_read_of_the_streams_creation_never_answers_already_written() {
    let (_server, bus, agent) = broker().await;
    let seq = untagged_trigger(&bus, &agent).await;
    let advisory = advisory(&exhaust(&bus, &agent).await);
    let read = named(&bus, &agent, seq, None).await;
    dispatcher_dead_letter(&bus, &agent, seq, Uuid::now_v7()).await;
    bus.jetstream()
        .delete_stream(TRIGGER_STREAM_NAME)
        .await
        .expect("delete the trigger stream");
    let watch = AdvisoryWatch::without_records(bus.clone());
    let trigger = || NamedTrigger {
        agent: read.agent.clone(),
        id: None,
        published: read.published,
    };

    match watch
        .written_before(&advisory, Some(trigger()), false)
        .await
    {
        Err(AdvisoryWatchError::TriggerStream(why)) => {
            assert!(why.starts_with("reading its creation: "), "{why}")
        }
        Ok(Check::Written) => panic!("inside the limit: answered already written"),
        Ok(Check::NotWritten) => panic!("inside the limit: answered not written"),
        Ok(Check::Unmade(why)) => panic!("inside the limit: unmade ({why})"),
        Err(other) => panic!("inside the limit: {other:?}"),
    }
    match watch
        .written_before(&advisory, Some(trigger()), true)
        .await
        .expect("past the limit, an answer")
    {
        Check::Unmade(why) => assert!(
            why.starts_with("trigger stream read error: reading its creation: "),
            "{why}"
        ),
        Check::Written => panic!("past the limit: answered already written"),
        Check::NotWritten => panic!("past the limit: answered not written, with no reason"),
    }
}

/// **A check the broker cannot answer in time is decided again inside the
/// limit, and unmade past it**, both the named check and the nameless one.
/// The broker is stopped while the checks read, so no read can finish
/// however long it is given: what this shows does not rest on a read being
/// slower than its limit, as a limit of no time did, which a read can beat
/// (the timer rounds up to its next millisecond). Inside the watch's limit
/// the check is an error, for the advisory to be decided again, and
/// nothing is taken as written; past it the check is unmade, and says the
/// read took longer than its limit.
#[tokio::test]
async fn a_check_the_broker_cannot_answer_in_time_decides_again_and_past_the_limit_is_unmade() {
    let (server, bus, agent) = broker().await;
    let published = trigger(&bus, &agent).await;
    let advisory = advisory(&exhaust(&bus, &agent).await);
    dispatcher_dead_letter(&bus, &agent, published.stream_seq, published.id).await;
    let read = named(&bus, &agent, published.stream_seq, Some(published.id)).await;
    let handed = || {
        Some(NamedTrigger {
            agent: read.agent.clone(),
            id: read.id,
            published: read.published,
        })
    };
    let limit = Duration::from_millis(100);
    let watch = AdvisoryWatch::without_records(bus.clone()).with_scan_limit(limit);
    let out_of_time =
        format!("reading the log for a dead letter already written took longer than {limit:?}");

    server.pause();
    let named_inside = watch.written_before(&advisory, handed(), false).await;
    let named_past = watch.written_before(&advisory, handed(), true).await;
    let nameless_inside = watch.written_before(&advisory, None, false).await;
    let nameless_past = watch.written_before(&advisory, None, true).await;
    server.resume();

    for (check, inside, past) in [
        ("named", named_inside, named_past),
        ("nameless", nameless_inside, nameless_past),
    ] {
        match inside {
            Err(AdvisoryWatchError::Log(why)) => assert_eq!(why, out_of_time, "{check}"),
            Err(other) => panic!("{check}, inside the limit: {other:?}"),
            Ok(Check::Written) => panic!("{check}, inside the limit: answered already written"),
            Ok(Check::NotWritten) => panic!("{check}, inside the limit: answered not written"),
            Ok(Check::Unmade(why)) => panic!("{check}, inside the limit: unmade ({why})"),
        }
        match past {
            Ok(Check::Unmade(why)) => {
                assert_eq!(
                    why,
                    format!("event log read error: {out_of_time}"),
                    "{check}"
                )
            }
            Ok(Check::Written) => panic!("{check}, past the limit: answered already written"),
            Ok(Check::NotWritten) => panic!("{check}, past the limit: answered not written"),
            Err(err) => panic!("{check}, past the limit: {err:?}"),
        }
    }
}
