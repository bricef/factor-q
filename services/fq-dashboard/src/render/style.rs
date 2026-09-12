//! The dashboard's one stylesheet, inlined into every page by
//! [`super::page_opts`]. Plain CSS in a plain string: it used to live
//! inside the shell's `format!` literal, where every brace had to be
//! doubled and every rule counted against the shell's file budget.
//! Nothing here is interpolated — the shell drops the whole constant
//! into `<style>…</style>` verbatim.

/// The complete `<style>` body. Ends with a newline so the shell's
/// `</style>` lands on its own line, as it always has.
pub(super) const STYLESHEET: &str = r#"/* Dark by default (owner preference). Semantics keep their hue —
   ok green / warn amber / bad red — tuned for contrast on the dark
   ground; a light theme can arrive later as a prefers-color-scheme
   override. */
:root { color-scheme: dark; }
body { font-family: monospace; margin: 1.5rem; color: #d4d7dc; background: #14161a; }
h1 { font-size: 1.2rem; } h2 { font-size: 1rem; margin-top: 1.5rem; }
a { color: #7aa2e8; }
table { border-collapse: collapse; margin: 0.5rem 0; }
th, td { border: 1px solid #3a3f47; padding: 0.25rem 0.6rem; text-align: left; }
th { background: #21252b; }
nav a { margin-right: 1rem; }
.ok { color: #5fbf77; } .warn { color: #d9a04c; } .bad { color: #e06c6c; }
.muted { color: #7d838c; }
/* Numeric table cells: right-aligned, digits lined up. */
td.n { text-align: right; font-variant-numeric: tabular-nums; }
/* Share-of-spend bar (costs page). Single-series magnitude only — the
   percentage text beside it carries the value; the bar is the glance. */
.bar { display: inline-block; vertical-align: baseline; width: 72px; height: 7px; background: #21252b; margin-right: 0.5rem; }
.bar i { display: block; height: 100%; background: #7aa2e8; opacity: 0.55; }
tr.sub td, tr.sub th { border-top: 2px solid #3a3f47; }
/* Spend-over-time bars (costs page). Single series, one hue; the
   value lives in the hover title and on the tallest bar's label —
   quiet buckets render as gaps, not lies. */
.chart { display: flex; align-items: flex-end; gap: 3px; margin: 0.6rem 0 0.2rem; }
.chart .cslot { display: flex; flex-direction: column; align-items: center; justify-content: flex-end; flex: 1; max-width: 36px; }
.chart .cslot i { display: block; width: 100%; background: #7aa2e8; opacity: 0.55; }
.chart .cslot b { font-size: 0.75rem; margin-bottom: 2px; }
.chart .cslot span { font-size: 0.7rem; color: #7d838c; margin-top: 3px; }
pre { background: #1c2026; border: 1px solid #333941; padding: 0.5rem; white-space: pre-wrap; overflow-wrap: anywhere; margin: 0.3rem 0; max-width: 72rem; }
details { margin: 0.3rem 0; } summary { cursor: pointer; color: #9aa1ab; }
.turn { border-left: 3px solid #3a3f47; padding-left: 0.8rem; margin: 1.2rem 0; }
.turn h3 { font-size: 1rem; margin: 0.2rem 0; }
.turn.err { border-left-color: #e06c6c; }
/* The transcript timeline scrolls inside its own panel and OPENS AT
   THE BOTTOM: column-reverse anchors scroll to the newest entry with
   zero JS, and keeps it pinned as the SSE stream adds turns. Contract:
   the DOM holds entries NEWEST-FIRST (the server renders reversed and
   the stream prepends); column-reverse flips them back so the visual
   order stays oldest-at-top. */
#turns { display: flex; flex-direction: column-reverse; overflow-y: auto; max-height: calc(100vh - 16rem); border-top: 1px solid #21252b; border-bottom: 1px solid #21252b; }
"#;
