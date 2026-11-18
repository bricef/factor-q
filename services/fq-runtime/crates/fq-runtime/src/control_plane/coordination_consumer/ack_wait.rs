//! The coordination consumer's ack window.

use std::time::Duration;

/// How long the broker waits for this consumer to acknowledge a delivery
/// before it delivers it again: five seconds, where the bus default
/// (`[bus] ack_wait_ms`) is thirty.
///
/// Strict order keeps one delivery outstanding, so a delivery a stopped
/// daemon never acknowledged holds every later event until its window
/// lapses, and a daemon often stops with one outstanding: it died folding
/// it, or it was stopped while one was on its way. Until then the next
/// start folds nothing: its ownership rows stand still, and so does the
/// coordination half of the read horizon, which gated reads of the
/// Invocation view wait on (`fq invocation drop` reads its drop back that
/// way).
///
/// **A fold slower than the window.** The handler writes SQLite and
/// returns. One that runs past the window has its event delivered again
/// behind it and folded twice, which leaves the rows as once. But a fold
/// that stays slower than the window, as on a slow or contended disk, is
/// worse than that: each event is delivered again while the copy before it
/// is still being handled, so each costs one more handling than the event
/// before (three events took nine handlings), and the consumer falls
/// further behind for as long as it lasts. A fold that fails instead is
/// bounded by the retry delay its NAK asks for. The cure, an in-progress
/// acknowledgement from the shared loop while a handler runs, is issue
/// 1090's.
pub const ACK_WAIT: Duration = Duration::from_secs(5);
