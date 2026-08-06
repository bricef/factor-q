//! What a `since` argument means to a read.
//!
//! Every operator read that narrows by time — `fq events query
//! --since`, `fq costs --since`, the Event atom's filter — hands
//! [`Views`](super::Views) a lower bound on time, and the stores
//! compare that bound **as text**: the projection writes each timestamp
//! with `to_rfc3339()` and asks `timestamp >= ?`. Two things follow,
//! and together they are why the grammar lives here rather than at each
//! caller.
//!
//! **One grammar, named once.** `fq events query --since` and `fq costs
//! --since` sit on the same page of QUICKSTART, so an operator meets
//! any disagreement between them by copying an argument from one to the
//! other. Both name [`lower_bound`], so they cannot drift apart.
//!
//! **Parse, then re-render.** What reaches the comparison is always
//! this module's rendering of the instant the operator named, never the
//! operator's own spelling — `…07.500Z` and `…07.500+00:00` are one
//! instant and two strings, and only one of them is the string the
//! column holds.
//!
//! The spellings accepted are a date, optionally refined by a time, and
//! read as UTC when they carry no offset. That is the set the lexical
//! comparison already admitted back when a bound was passed through
//! unparsed: a prefix of a stored timestamp *was* a valid lower bound,
//! which is why `--since 2026-04-25` has always meant "the 25th
//! onwards". Parsing it keeps that meaning and adds the offsets the
//! lexical compare could never have got right.

use std::borrow::Cow;

use chrono::{DateTime, NaiveDateTime, Utc};

/// The accepted spellings, in the words used to name them back to an
/// operator who got one wrong. One string so the CLI's `--since` help,
/// the atom's rejection and this module's own error all agree.
pub const ACCEPTED_SINCE: &str = "a date (2026-04-25), a UTC date and time \
     (2026-04-25T10, T10:30, T10:30:15, T10:30:15.500), or an RFC3339 instant \
     (2026-04-25T10:30:15Z, 2026-04-25T16:00:15+05:30)";

/// A `since` argument that names no instant.
#[derive(Debug, thiserror::Error)]
#[error("`{spelling}` is not a time — expected {ACCEPTED_SINCE}")]
pub struct SinceError {
    /// What the caller actually wrote, quoted back so the message is
    /// readable when it surfaces several layers from the argument.
    pub spelling: String,
}

/// The instant a `since` argument names.
///
/// A spelling carrying an offset is an RFC3339 instant and means
/// exactly what it says, wherever in the world it was written. A
/// spelling carrying none is read as UTC — an operator narrowing a
/// runtime whose every timestamp is UTC means UTC — and a spelling
/// truncated at a field boundary names the *earliest* instant it could
/// mean. So `2026-04-25` is that day's first moment, which is the
/// reading that makes it a lower bound on the whole day.
pub fn instant(spelling: &str) -> Result<DateTime<Utc>, SinceError> {
    if let Ok(instant) = DateTime::parse_from_rfc3339(spelling) {
        return Ok(instant.with_timezone(&Utc));
    }
    NaiveDateTime::parse_from_str(&filled_out(spelling), "%Y-%m-%dT%H:%M:%S%.f")
        .map(|naive| naive.and_utc())
        .map_err(|_| SinceError {
            spelling: spelling.to_string(),
        })
}

/// The instant [`instant`] found, rendered the way the stores write
/// their timestamps — the only form that may reach a `timestamp >= ?`.
pub fn lower_bound(spelling: &str) -> Result<String, SinceError> {
    instant(spelling).map(|instant| instant.to_rfc3339())
}

/// A spelling truncated at a field boundary, filled out to the earliest
/// instant it names: `2026-04-25` becomes `2026-04-25T00:00:00`, and
/// `2026-04-25T10` becomes `2026-04-25T10:00:00`. Anything else is
/// handed on unchanged, to be accepted or refused on its own terms.
fn filled_out(spelling: &str) -> Cow<'_, str> {
    /// The earliest instant of any period, as the tail each truncation
    /// is missing: byte 10 onwards completes a date, byte 13 an hour,
    /// byte 16 a minute. Lengths that fall mid-field are not boundaries
    /// and get no help — `2026-4-25` is a typo, not a shorthand.
    const EARLIEST: &str = "0000-01-01T00:00:00";
    match spelling.len() {
        boundary @ (10 | 13 | 16) => Cow::Owned(format!("{spelling}{}", &EARLIEST[boundary..])),
        _ => Cow::Borrowed(spelling),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(rfc3339: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(rfc3339)
            .expect("test fixture is RFC3339")
            .with_timezone(&Utc)
    }

    /// The spelling QUICKSTART puts in front of an operator (`fq costs
    /// --since 2026-04-25`), and the one a lexical `timestamp >= ?`
    /// admitted for free. A bare date is a lower bound on the whole
    /// day, so it must lower to that day's first moment — anything
    /// later would silently drop the morning's events.
    #[test]
    fn a_bare_date_is_that_days_first_moment() {
        assert_eq!(instant("2026-04-25").unwrap(), at("2026-04-25T00:00:00Z"));
        assert_eq!(
            lower_bound("2026-04-25").unwrap(),
            "2026-04-25T00:00:00+00:00"
        );
    }

    /// A time may stop at any field boundary, and each truncation names
    /// the earliest instant it could mean — the same rule as the date.
    #[test]
    fn a_time_without_an_offset_is_utc_and_may_stop_at_any_field() {
        assert_eq!(
            instant("2026-04-25T10").unwrap(),
            at("2026-04-25T10:00:00Z")
        );
        assert_eq!(
            instant("2026-04-25T10:30").unwrap(),
            at("2026-04-25T10:30:00Z")
        );
        assert_eq!(
            instant("2026-04-25T10:30:15").unwrap(),
            at("2026-04-25T10:30:15Z")
        );
        assert_eq!(
            instant("2026-04-25T10:30:15.500").unwrap(),
            at("2026-04-25T10:30:15.500Z")
        );
    }

    /// An offset is a spelling of an instant, not a different instant:
    /// all three of these name the same moment, so all three must lower
    /// to the one string the projection would have stored for it.
    #[test]
    fn an_offset_is_a_spelling_and_not_a_different_instant() {
        let stored = "2026-04-25T10:30:15.500+00:00";
        assert_eq!(lower_bound("2026-04-25T10:30:15.500Z").unwrap(), stored);
        assert_eq!(
            lower_bound("2026-04-25T10:30:15.500+00:00").unwrap(),
            stored
        );
        assert_eq!(
            lower_bound("2026-04-25T16:00:15.500+05:30").unwrap(),
            stored
        );
    }

    /// Every accepted spelling lowers to the way a `DateTime<Utc>`
    /// renders itself, because that is how the projection wrote the
    /// column it will be compared against.
    #[test]
    fn every_accepted_spelling_lowers_to_the_form_the_projection_stores() {
        for spelling in [
            "2026-04-25",
            "2026-04-25T10",
            "2026-04-25T10:30",
            "2026-04-25T10:30:15",
            "2026-04-25T10:30:15.500",
            "2026-04-25T10:30:15Z",
            "2026-04-25T16:00:15+05:30",
        ] {
            let bound = lower_bound(spelling).expect(spelling);
            assert_eq!(
                bound,
                instant(spelling).unwrap().to_rfc3339(),
                "`{spelling}` must lower to its instant, stored-form"
            );
            // And the rendering is idempotent: re-reading a bound the
            // store has seen names the same instant again.
            assert_eq!(lower_bound(&bound).unwrap(), bound);
        }
    }

    /// A spelling that names no instant is refused, and the refusal
    /// says both what was written and what would have worked — the
    /// operator is one edit away and should not have to guess.
    #[test]
    fn a_spelling_that_names_no_instant_is_refused_by_name() {
        for spelling in [
            "yesterday",
            "",
            "2026-4-25",
            "2026-04-25 10:30:00",
            "2026-04-25T10:3",
            "2026-04-25T10:30:15+0530",
            "last tuesday!",
        ] {
            let err = instant(spelling).expect_err(&format!("`{spelling}` names no instant"));
            let message = err.to_string();
            assert!(
                message.contains(spelling) && message.contains("2026-04-25"),
                "refusal must quote the spelling and name the accepted \
                 forms; got {message}"
            );
        }
    }
}
