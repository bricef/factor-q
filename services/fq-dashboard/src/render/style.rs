//! The dashboard's one stylesheet, inlined into every page by
//! [`super::page_opts`]. Plain CSS in a plain string: it used to live
//! inside the shell's `format!` literal, where every brace had to be
//! doubled and every rule counted against the shell's file budget.
//! Nothing here is interpolated — the shell drops the whole constant
//! into `<style>…</style>` verbatim.
//!
//! Desktop-first: the rules at the top are the layout, and the single
//! `@media (max-width: 40rem)` block at the bottom holds every phone
//! rule. A desktop window never matches it, so changing the phone
//! layout cannot move a desktop pixel — the desktop screenshots in
//! `scripts/dashboard-screenshots.sh` pin that.

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
/* ---- Phones. One breakpoint, one block. Every rule below is scoped to
   viewports at most 40rem (640px) wide, so a desktop window never sees
   any of it and the rules above stay the source of truth; this block
   only overrides what a narrow touch screen needs. The shell's
   <meta name="viewport"> is what makes a phone report its real width
   here instead of laying the page out at a 980px desktop emulation. */
@media (max-width: 40rem) {
  /* Reclaim the side margins; one size up from the 13px monospace
     default so body text reads at arm's length. */
  body { margin: 0.75rem; font-size: 14px; }
  /* The nav becomes a full-width tab bar pinned to the top, so a long
     page (events, a transcript) can change section without scrolling
     back up. The negative margins let its background span the body's
     side margins while stuck; space-between spreads the five short
     labels across the width, wrapping only on a screen narrower than
     they are. It sits outside #main, so live-region morphs never
     touch it. */
  nav { position: sticky; top: 0; z-index: 1; display: flex; flex-wrap: wrap; justify-content: space-between; column-gap: 0.75rem; margin: -0.75rem -0.75rem 0.5rem; padding: 0 0.75rem; background: #14161a; border-bottom: 1px solid #21252b; }
  nav a { margin-right: 0; padding: 0.6rem 0; }
  /* Taller hit boxes for inline links and fold toggles: vertical
     padding on an inline element grows its tap target without moving
     the line it sits on. A link in a text row (the invocation filters,
     the costs window, "← all agents") wraps as a unit, never mid-label:
     "hide / failed" split over two lines reads as two controls. */
  a { padding: 0.3rem 0; }
  p a { white-space: nowrap; }
  summary { padding: 0.35rem 0; }
  /* Tables scroll sideways instead of crushing their columns: a
     block-level table with overflow-x is its own scroll region, so the
     page itself never scrolls horizontally and column widths stay
     honest. Label cells never wrap. Data cells wrap only in key/value
     tables (one <th> per row), where a 36-char id must break to fit;
     a table whose first row is a header row keeps each row on one line
     and scrolls instead — its minimum width overflows a phone either
     way, and one-line rows are the readable form of that. */
  table { display: block; overflow-x: auto; }
  th, td { padding: 0.4rem 0.5rem; }
  th { white-space: nowrap; }
  td { overflow-wrap: anywhere; }
  table:has(tr:first-child > th:nth-child(2)) td { white-space: nowrap; }
  /* The spend chart keeps every bar and drops the interior date labels:
     thirty daily slots at ~11px each cannot carry two digits apiece, so
     only the first and last carry the axis and the tallest bar keeps
     its value. Slots may shrink below their label (min-width: 0) so the
     bars stay equal; a spilling label is clipped at the chart's edge in
     the one case (tallest bar first or last) where it would otherwise
     push the page wider than the screen. */
  .chart { gap: 2px; overflow: hidden; }
  .chart .cslot { min-width: 0; max-width: none; }
  .chart .cslot span { display: none; }
  .chart .cslot:first-child span, .chart .cslot:last-child span { display: block; }
  /* 100vh on a phone is the viewport with the browser chrome hidden,
     so a vh-sized panel pushes the line under it below the toolbar;
     svh sizes for the chrome-visible case. */
  #turns { max-height: calc(100svh - 16rem); }
}
"#;
