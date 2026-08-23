//! Build-time version metadata and the `fq version` verb.
//!
//! Split out of `lib.rs` (#189). [`FQ_VERSION`] carries the commit as semver
//! build metadata, so the **running** daemon reports which build it is — the
//! `system.startup` event and banner carry the SHA, not just the semver.

/// Build-time version metadata, emitted by `build.rs`.
const FQ_GIT_SHA: &str = env!("FQ_GIT_SHA");
const FQ_BUILD_EPOCH: &str = env!("FQ_BUILD_EPOCH");
const FQ_TARGET: &str = env!("FQ_TARGET");
/// Semver + commit (valid semver build metadata), so the **running**
/// daemon reports which build it is — the `system.startup` event and
/// banner carry the SHA, not just the semver. Lets a deploy check
/// confirm the live process is on the expected commit.
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
