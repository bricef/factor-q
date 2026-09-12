//! Static assets served from the binary, so the dashboard stays fully
//! self-contained behind its auth front — no CDN, no asset directory to
//! deploy beside it. One route, `/assets/{name}`, over a fixed table:
//! an unknown name is a 404, never a directory listing or a disk read.
//!
//! The icon set is what lets a phone put the dashboard on its home
//! screen. Browser tabs take the SVG; iOS reads `apple-touch-icon.png`
//! (180px and opaque — iOS masks its own corners and paints
//! transparency black); Android reads the manifest's 192/512px PNGs.
//! The manifest keeps `display: browser` on purpose: the dogfood front
//! is basic-auth plus a session cookie set in the browser, and a
//! standalone web app on iOS gets its own cookie jar, so it would land
//! on the login prompt at every launch. `just dashboard-icon`
//! regenerates the set — the SVG's paths from DejaVu Sans Mono Bold
//! (`scripts/dashboard-icon.py`), the PNGs from the SVG through headless
//! chromium (`scripts/dashboard-icon.sh`).

use axum::extract::Path;
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};

/// `(name, content type, bytes)`. Names are the literal last path
/// segment; the content types are what the shell's `<link>` tags and
/// the manifest's `type` fields promise. datastar is the vendored
/// client (pinned v1.0.0, MIT; sha256 recorded in the PR that
/// introduced it).
const ASSETS: &[(&str, &str, &[u8])] = &[
    (
        "datastar.js",
        "text/javascript",
        include_bytes!("../../assets/datastar.js"),
    ),
    (
        "icon.svg",
        "image/svg+xml",
        include_bytes!("../../assets/icon.svg"),
    ),
    (
        "apple-touch-icon.png",
        "image/png",
        include_bytes!("../../assets/apple-touch-icon.png"),
    ),
    (
        "icon-192.png",
        "image/png",
        include_bytes!("../../assets/icon-192.png"),
    ),
    (
        "icon-512.png",
        "image/png",
        include_bytes!("../../assets/icon-512.png"),
    ),
    (
        "manifest.webmanifest",
        "application/manifest+json",
        include_bytes!("../../assets/manifest.webmanifest"),
    ),
];

/// The `/assets/{name}` handler: the table entry's bytes under its
/// content type, cacheable for a day, or a bare 404.
pub(crate) async fn asset(Path(name): Path<String>) -> Response {
    match ASSETS.iter().find(|(n, _, _)| *n == name) {
        Some((_, content_type, bytes)) => (
            [
                (header::CONTENT_TYPE, *content_type),
                (header::CACHE_CONTROL, "public, max-age=86400"),
            ],
            *bytes,
        )
            .into_response(),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}
