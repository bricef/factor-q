//! Build script: stamps git + build metadata into the binary so `fq
//! version` (and `fq --version`) report exactly which build is running.
//! Every value degrades gracefully to "unknown"/empty when git or the
//! `.git` directory is unavailable (e.g. building from a crate tarball).

use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

fn main() {
    let sha = git(&["rev-parse", "--short=12", "HEAD"]).unwrap_or_else(|| "unknown".to_string());
    let dirty = git(&["status", "--porcelain"])
        .map(|out| !out.is_empty())
        .unwrap_or(false);
    let sha = if dirty { format!("{sha}-dirty") } else { sha };

    // Build timestamp (Unix seconds). Honour SOURCE_DATE_EPOCH for
    // reproducible builds; otherwise stamp the current time.
    let epoch = std::env::var("SOURCE_DATE_EPOCH")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or_else(|| {
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0)
        });

    let semver = std::env::var("CARGO_PKG_VERSION").unwrap_or_default();
    let target = std::env::var("TARGET").unwrap_or_default();

    println!("cargo:rustc-env=FQ_GIT_SHA={sha}");
    println!("cargo:rustc-env=FQ_BUILD_EPOCH={epoch}");
    println!("cargo:rustc-env=FQ_TARGET={target}");
    println!("cargo:rustc-env=FQ_VERSION_LONG={semver} ({sha} {target})");

    // Re-run when HEAD moves (keeps the SHA fresh) and when the
    // reproducibility knob changes — not on every source edit.
    if let Some(git_dir) = git(&["rev-parse", "--absolute-git-dir"]) {
        println!("cargo:rerun-if-changed={git_dir}/HEAD");
    }
    println!("cargo:rerun-if-env-changed=SOURCE_DATE_EPOCH");
}

/// Run `git <args>` and return trimmed stdout, or `None` on any failure.
fn git(args: &[&str]) -> Option<String> {
    let output = Command::new("git").args(args).output().ok()?;
    if !output.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&output.stdout).trim().to_string())
}
