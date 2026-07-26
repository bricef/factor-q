//! The projection watermark: the last event-log (`fq-events` stream)
//! sequence the projection has applied.
//!
//! Reads compose with it for read-your-writes (the domain model's
//! "fold as of watermark W"): a command's receipt names the sequence
//! its events landed at; a subsequent read waits — bounded — until
//! the fold includes them. The mark is in-memory by design: on
//! restart the durable consumer resumes from its ack floor and the
//! mark rebuilds as events apply. Durable applied-state is #139's
//! reproject concern, not this one.

use std::time::Duration;

use tokio::sync::watch;

/// Create the pair: the projection consumer advances the sender as
/// it applies events; readers wait on the [`Watermark`].
pub fn channel() -> (WatermarkSender, Watermark) {
    let (tx, rx) = watch::channel(0);
    (WatermarkSender { tx }, Watermark { rx })
}

/// The producer half, held by the projection consumer.
pub struct WatermarkSender {
    tx: watch::Sender<u64>,
}

impl WatermarkSender {
    /// Record that the projection has applied the event at `seq`.
    /// Monotonic: an older sequence (a NAK redelivery, a replay)
    /// never regresses the mark.
    pub fn advance(&self, seq: u64) {
        self.tx.send_if_modified(|current| {
            if seq > *current {
                *current = seq;
                true
            } else {
                false
            }
        });
    }

    /// Another reader handle onto the same mark.
    pub fn subscribe(&self) -> Watermark {
        Watermark {
            rx: self.tx.subscribe(),
        }
    }
}

/// The reader half: where the projection's fold currently stands.
#[derive(Clone)]
pub struct Watermark {
    rx: watch::Receiver<u64>,
}

impl Watermark {
    /// The last applied event-log sequence (0 before anything has
    /// applied since startup).
    pub fn current(&self) -> u64 {
        *self.rx.borrow()
    }

    /// Wait until the projection has applied at least `min_seq`,
    /// bounded by `bound`. Returns the applied sequence observed.
    /// Fails closed: a timeout reports the lag, and a stopped
    /// projection (sender dropped) is its own distinct error rather
    /// than an indefinite wait.
    pub async fn wait_for(&self, min_seq: u64, bound: Duration) -> Result<u64, WatermarkError> {
        let mut rx = self.rx.clone();
        match tokio::time::timeout(bound, rx.wait_for(|applied| *applied >= min_seq)).await {
            Ok(Ok(applied)) => Ok(*applied),
            Ok(Err(_sender_dropped)) => Err(WatermarkError::Stopped {
                wanted: min_seq,
                applied: self.current(),
            }),
            Err(_elapsed) => Err(WatermarkError::Lag {
                wanted: min_seq,
                applied: self.current(),
                bound,
            }),
        }
    }
}

/// Why a bounded wait on the watermark did not complete.
#[derive(Debug, thiserror::Error)]
pub enum WatermarkError {
    #[error(
        "projection lag: wanted at least sequence {wanted}, applied {applied} \
         after waiting {bound:?}"
    )]
    Lag {
        wanted: u64,
        applied: u64,
        bound: Duration,
    },
    #[error(
        "projection stopped: the watermark cannot advance (wanted {wanted}, \
         applied {applied})"
    )]
    Stopped { wanted: u64, applied: u64 },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn satisfied_watermark_returns_immediately() {
        let (tx, wm) = channel();
        tx.advance(5);
        let applied = wm.wait_for(5, Duration::from_millis(10)).await.unwrap();
        assert_eq!(applied, 5);
        // And anything below the mark is equally free.
        assert_eq!(wm.wait_for(1, Duration::from_millis(10)).await.unwrap(), 5);
    }

    #[tokio::test]
    async fn waiters_wake_when_the_mark_advances() {
        let (tx, wm) = channel();
        let waiter = tokio::spawn({
            let wm = wm.clone();
            async move { wm.wait_for(3, Duration::from_secs(5)).await }
        });
        tokio::task::yield_now().await;
        tx.advance(1);
        tx.advance(3);
        assert_eq!(waiter.await.unwrap().unwrap(), 3);
    }

    #[tokio::test]
    async fn lag_is_reported_with_both_positions() {
        let (tx, wm) = channel();
        tx.advance(2);
        let err = wm
            .wait_for(10, Duration::from_millis(20))
            .await
            .unwrap_err();
        match err {
            WatermarkError::Lag {
                wanted, applied, ..
            } => {
                assert_eq!((wanted, applied), (10, 2));
            }
            other => panic!("expected Lag, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_dropped_sender_is_stopped_not_a_hang() {
        let (tx, wm) = channel();
        tx.advance(2);
        drop(tx);
        let err = wm.wait_for(10, Duration::from_secs(5)).await.unwrap_err();
        assert!(
            matches!(
                err,
                WatermarkError::Stopped {
                    wanted: 10,
                    applied: 2
                }
            ),
            "got {err:?}"
        );
    }

    #[tokio::test]
    async fn the_mark_never_regresses() {
        let (tx, wm) = channel();
        tx.advance(7);
        tx.advance(3); // a redelivery of an older message
        assert_eq!(wm.current(), 7);
    }

    #[test]
    fn concurrent_advances_settle_on_the_maximum() {
        let (tx, wm) = channel();
        let tx = std::sync::Arc::new(tx);
        let handles: Vec<_> = (1..=64u64)
            .map(|seq| {
                let tx = tx.clone();
                std::thread::spawn(move || tx.advance(seq))
            })
            .collect();
        for handle in handles {
            handle.join().unwrap();
        }
        assert_eq!(wm.current(), 64);
    }
}

/// Several consumers' marks, waited on together: a view whose fold
/// spans stores fed by different consumers (the Invocation view reads
/// projection AND coordination state) is only honestly at sequence S
/// when EVERY feeding consumer has resolved S. `wait_for` spends one
/// bound across all marks — the waits overlap in wall time, so the
/// bound is a ceiling, not a sum.
#[derive(Clone)]
pub struct Horizon {
    marks: Vec<Watermark>,
}

impl Horizon {
    pub fn new(marks: Vec<Watermark>) -> Self {
        Self { marks }
    }

    /// Wait until every mark has resolved at least `min_seq`, bounded
    /// by `bound` overall. Fails closed with the LOWEST applied
    /// position — the honest answer to "how far is the fold, really".
    pub async fn wait_for(&self, min_seq: u64, bound: Duration) -> Result<u64, WatermarkError> {
        let deadline = tokio::time::Instant::now() + bound;
        let mut lowest = u64::MAX;
        for mark in &self.marks {
            let remaining = deadline
                .checked_duration_since(tokio::time::Instant::now())
                .unwrap_or(Duration::ZERO);
            let applied = mark
                .wait_for(min_seq, remaining)
                .await
                .map_err(|e| match e {
                    WatermarkError::Lag {
                        wanted, applied, ..
                    } => WatermarkError::Lag {
                        wanted,
                        applied: applied.min(self.lowest_applied()),
                        bound,
                    },
                    stopped => stopped,
                })?;
            lowest = lowest.min(applied);
        }
        Ok(if lowest == u64::MAX { 0 } else { lowest })
    }

    fn lowest_applied(&self) -> u64 {
        self.marks.iter().map(|m| m.current()).min().unwrap_or(0)
    }
}

#[cfg(test)]
mod horizon_tests {
    use super::*;

    #[tokio::test]
    async fn released_only_when_every_mark_arrives() {
        let (tx_a, wm_a) = channel();
        let (tx_b, wm_b) = channel();
        let horizon = Horizon::new(vec![wm_a, wm_b]);
        tx_a.advance(5);
        // b is behind: the horizon must not release.
        let err = horizon
            .wait_for(5, Duration::from_millis(30))
            .await
            .unwrap_err();
        assert!(
            matches!(err, WatermarkError::Lag { applied, .. } if applied < 5),
            "got {err:?}"
        );
        tx_b.advance(5);
        assert_eq!(
            horizon.wait_for(5, Duration::from_secs(1)).await.unwrap(),
            5
        );
    }

    #[tokio::test]
    async fn one_bound_spans_all_marks() {
        let (_tx_a, wm_a) = channel();
        let (_tx_b, wm_b) = channel();
        let horizon = Horizon::new(vec![wm_a, wm_b]);
        let started = std::time::Instant::now();
        let _ = horizon.wait_for(1, Duration::from_millis(80)).await;
        assert!(
            started.elapsed() < Duration::from_millis(200),
            "the bound is a ceiling across marks, not per mark"
        );
    }
}
