//! Build-time version metadata and the `fq version` verb.
//!
//! Split out of `lib.rs` (#189). Everything here describes **this
//! client's** build — [`print_version`] reads no daemon and makes no
//! edge call. The daemon's build travels the other way: it stamps its
//! own `system.startup` event and banner from `fq-daemon`'s copy of
//! this metadata, and an operator asks for it with `fq status`, which
//! renders what `control.status` reports.

/// Build-time version metadata, emitted by `build.rs`.
const FQ_GIT_SHA: &str = env!("FQ_GIT_SHA");
const FQ_BUILD_EPOCH: &str = env!("FQ_BUILD_EPOCH");
const FQ_TARGET: &str = env!("FQ_TARGET");
/// Semver + commit, as valid semver build metadata, so a build is
/// identifiable by more than its semver.
///
/// Unused in this crate — `fq version` prints the fields separately
/// and `fq status` renders the daemon's own string. The live copy is
/// `fq_daemon::version::FQ_VERSION`, which is what reaches the
/// `system.startup` event and the banner, and what a deploy check
/// compares against the expected commit.
#[allow(dead_code)]
pub(crate) const FQ_VERSION: &str = concat!(env!("CARGO_PKG_VERSION"), "+", env!("FQ_GIT_SHA"));

/// Print version + build information: semver, commit, build date, target.
pub(crate) fn print_version(json: bool) {
    let build_date = FQ_BUILD_EPOCH
        .parse::<i64>()
        .ok()
        .and_then(|secs| chrono::DateTime::from_timestamp(secs, 0))
        .map(|dt| dt.format("%Y-%m-%d").to_string())
        .unwrap_or_else(|| "unknown".to_string());

    if json {
        let info = serde_json::json!({
            "version": env!("CARGO_PKG_VERSION"),
            "commit": FQ_GIT_SHA,
            "build_date": build_date,
            "target": FQ_TARGET,
        });
        println!("{}", serde_json::to_string_pretty(&info).unwrap());
    } else {
        println!("fq {}", env!("CARGO_PKG_VERSION"));
        println!("  commit:      {FQ_GIT_SHA}");
        println!("  build date:  {build_date}");
        println!("  target:      {FQ_TARGET}");
    }
}
