//! The transcript page and its live tail.
//!
//! Split from `pages.rs` when the edge re-point pushed that file past
//! the 800-line cap. This is the one page that streams: everything
//! else dials, reads once and renders, while the tail holds a long
//! poll open and forwards turns as they land. That difference is why
//! it is the seam — the cursor discipline below belongs to it alone.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::Html;
use fq_edge::EdgeClient;
use fq_ops::{Domain, OpId};
use fq_runtime::surface::{InvocationViewKey, TurnFilter};
use fq_runtime::views::InvocationDetailView;

use super::{
    CallError, Page, call, edge_or_unreachable, now_ms, unreachable_page, with_skew_banner,
};
use crate::{AppState, render};

/// How long the tail asks the daemon to hold one long poll open.
/// `turn.stream` answers as soon as a turn lands, so this is the idle
/// ceiling, not the latency.
const TURN_POLL_WAIT_MS: u64 = 30_000;

/// The transcript's live tail: an SSE stream of datastar element
/// patches. Long-polls `turn.stream` and forwards each new turn as an
/// append into `#turns`; when the run's Outcome arrives it patches
/// `#status` and closes the stream. tarpc has no server-streaming, so
/// poll-and-forward is the tarpc-shaped bridge (design discussion on
/// #105).
///
/// **The cursor is a log sequence, not a list index.** It used to be
/// the count of entries already rendered, which only worked because
/// the read service re-read the whole transcript each tick and sliced
/// it. `turn.stream` is cursored on the event log — every item carries
/// its sequence (D5) — so the page pins the tail's sequence *before*
/// it reads its snapshot and hands that number here, which is the same
/// gap-free seam `fq invocation transcript --follow` uses. A turn that
/// lands between the two reads is therefore shown once, not lost and
/// not duplicated.
pub(crate) async fn transcript_stream(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Query(q): Query<HashMap<String, String>>,
) -> axum::response::sse::Sse<
    impl futures::Stream<Item = Result<axum::response::sse::Event, std::convert::Infallible>>,
> {
    use axum::response::sse::{Event, KeepAlive, Sse};
    use datastar::prelude::{ElementPatchMode, PatchElements};

    fn status_error(msg: &str) -> Event {
        PatchElements::new(format!(
            r#"<p id="status" class="bad">stream error — {} (reload to retry)</p>"#,
            render::esc(msg)
        ))
        .write_as_axum_sse_event()
    }

    struct Poll {
        client: Option<EdgeClient>,
        state: Arc<AppState>,
        id: String,
        cursor: u64,
        truncate: Option<usize>,
        queue: std::collections::VecDeque<Event>,
        done: bool,
    }

    let full = q.get("full").is_some_and(|v| v == "1");
    let init = Poll {
        client: None,
        state: state.clone(),
        id,
        // `after` is the sequence the page pinned; absent (a hand-typed
        // URL) means "from the tail", never "from the beginning" — a
        // tail that replayed the whole log would double every entry
        // already on the page.
        cursor: q
            .get("after")
            .and_then(|v| v.parse().ok())
            .unwrap_or(u64::MAX),
        truncate: (!full).then_some(fq_runtime::transcript::DEFAULT_TRUNCATE_BYTES),
        queue: std::collections::VecDeque::new(),
        done: false,
    };

    let stream = futures::stream::unfold(init, |mut s| async move {
        loop {
            if let Some(event) = s.queue.pop_front() {
                return Some((Ok(event), s));
            }
            if s.done {
                return None;
            }

            if s.client.is_none() {
                match EdgeClient::connect(
                    &s.state.edge_addr,
                    s.state.edge_fingerprint,
                    &s.state.edge_token,
                )
                .await
                {
                    Ok(c) => s.client = Some(c),
                    Err(err) => {
                        s.queue.push_back(status_error(&format!("edge: {err}")));
                        s.done = true;
                        continue;
                    }
                }
            }
            let filter = match serde_json::to_value(TurnFilter {
                invocation_id: s.id.clone(),
                limit: None,
            }) {
                Ok(filter) => filter,
                Err(err) => {
                    s.queue.push_back(status_error(&format!("encode: {err}")));
                    s.done = true;
                    continue;
                }
            };
            let batch = s
                .client
                .as_ref()
                .expect("client dialled above")
                .next_batch(
                    OpId::Stream(Domain::Turn),
                    filter,
                    s.cursor,
                    TURN_POLL_WAIT_MS,
                )
                .await;
            let batch = match batch {
                Ok(Ok(batch)) => batch,
                Ok(Err(err)) => {
                    s.queue.push_back(status_error(&err.to_string()));
                    s.done = true;
                    continue;
                }
                Err(err) => {
                    s.queue.push_back(status_error(&format!("rpc: {err}")));
                    s.done = true;
                    continue;
                }
            };
            s.cursor = batch.next_from_seq;

            let mut entries: Vec<fq_runtime::transcript::TranscriptEntry> = Vec::new();
            for item in &batch.items {
                match serde_json::from_value::<fq_runtime::turn::TurnState>(item.item.clone()) {
                    Ok(turn) => entries.push(turn.transcript_entry()),
                    Err(err) => {
                        s.queue.push_back(status_error(&format!("decode: {err}")));
                        s.done = true;
                    }
                }
            }
            if s.done {
                continue;
            }
            if let Some(max) = s.truncate {
                fq_runtime::transcript::truncate_entries(&mut entries, max);
            }
            // #turns is a column-reverse panel (newest-first DOM):
            // PREPENDING in chronological order lands each newer entry
            // at the visual bottom, and the panel's scroll stays
            // pinned there.
            for entry in &entries {
                s.queue.push_back(
                    PatchElements::new(render::transcript_entry_html(entry, now_ms()))
                        .selector("#turns")
                        .mode(ElementPatchMode::Prepend)
                        .write_as_axum_sse_event(),
                );
            }
            if let Some(phase) = render::transcript_outcome(&entries) {
                s.queue.push_back(
                    PatchElements::new(render::transcript_status_html(Some(phase)))
                        .write_as_axum_sse_event(),
                );
                s.done = true;
            }
        }
    });
    Sse::new(stream).keep_alive(KeepAlive::default())
}

/// The whole conversation, not a page of it — `turn.list` pages at 200
/// by default, which would silently clip a long run's tail.
const TRANSCRIPT_TURN_LIMIT: u32 = u32::MAX;

pub(crate) async fn transcript_page(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Query(q): Query<HashMap<String, String>>,
) -> Page {
    let full = q.get("full").is_some_and(|v| v == "1");
    let client = match edge_or_unreachable(&state, "transcript").await {
        Ok(c) => c,
        Err(page) => return page,
    };
    let filter = match serde_json::to_value(TurnFilter {
        invocation_id: id.clone(),
        limit: Some(TRANSCRIPT_TURN_LIMIT),
    }) {
        Ok(filter) => filter,
        Err(err) => return unreachable_page(&state, "transcript", &format!("encode: {err}")),
    };

    // Pin the tail BEFORE reading the snapshot: `from_seq = u64::MAX`
    // seeks without consuming, and the sequence it reports is where the
    // live tail resumes. Reading the snapshot first would leave a
    // window in which a turn lands and is then never streamed.
    let seam = match client
        .next_batch(OpId::Stream(Domain::Turn), filter.clone(), u64::MAX, 0)
        .await
    {
        Ok(Ok(batch)) => Some(batch.next_from_seq),
        // A transcript with no turns yet has nothing to seek; the page
        // still renders, and the tail starts from the tail.
        Ok(Err(_)) | Err(_) => None,
    };

    let turns: Vec<fq_runtime::turn::TurnState> =
        match call(&client, OpId::List(Domain::Turn), filter).await {
            Ok(turns) => turns,
            Err(CallError::NotFound) => Vec::new(),
            Err(CallError::Failed(err)) => return unreachable_page(&state, "transcript", &err),
        };
    if turns.is_empty() {
        return (
            StatusCode::NOT_FOUND,
            Html(render::page(
                "transcript",
                state.refresh_secs,
                r#"<p class="muted">no transcript for that id (no dispatch rows recorded).</p>"#,
            )),
        );
    }
    // Log order is chronological and `turn.list` answers in sequence
    // order, so no re-sort is needed to reproduce the timeline.
    let mut entries: Vec<fq_runtime::transcript::TranscriptEntry> = turns
        .iter()
        .map(fq_runtime::turn::TurnState::transcript_entry)
        .collect();
    // The read service truncated payloads server-side; `turn.list`
    // answers with whole turns, so the same cap is applied here with
    // the same function. The page renders what it always rendered.
    if !full {
        fq_runtime::transcript::truncate_entries(
            &mut entries,
            fq_runtime::transcript::DEFAULT_TRUNCATE_BYTES,
        );
    }

    let title = format!("transcript {}", &id.chars().take(8).collect::<String>());
    // Best-effort: the one-line summary (#216) rides the invocation
    // detail view. A failure here must not take the transcript down —
    // the page renders without the line.
    let summary = match call::<InvocationDetailView>(
        &client,
        OpId::Get(Domain::Invocation),
        serde_json::json!(InvocationViewKey {
            invocation_id: id.clone()
        }),
    )
    .await
    {
        Ok(detail) => detail.summary,
        Err(_) => None,
    };
    let mut body = render::transcript(&entries, now_ms(), full, &id, summary.as_deref());
    let live = render::transcript_outcome(&entries).is_none();
    // Live runs stream: datastar opens the SSE tail from the sequence
    // pinned above and appends turns in place — no page reloads, no
    // scroll resets. Finished runs render static. No-JS browsers fall
    // back to the <noscript> meta-refresh.
    let extra_head = if live {
        r#"<script type="module" src="/assets/datastar.js"></script>"#
    } else {
        ""
    };
    if live {
        body.push_str(&format!(
            r#"<div data-on-load="@get('/invocations/{}/transcript/stream?after={}&full={}')"></div>"#,
            render::esc(&id),
            seam.unwrap_or(u64::MAX),
            u8::from(full),
        ));
    }
    state.last_seen_ms.store(now_ms(), Ordering::Relaxed);
    (
        StatusCode::OK,
        Html(render::page_opts(
            &title,
            None,
            extra_head,
            &with_skew_banner(&state, &body),
        )),
    )
}
