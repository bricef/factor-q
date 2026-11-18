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
/// way). The handler writes SQLite and returns; one that runs past the
/// window has its event delivered again behind it and folded twice, which
/// leaves the rows as once.
pub const ACK_WAIT: Duration = Duration::from_secs(5);
