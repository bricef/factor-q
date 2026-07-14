//! Build script: stamps the git SHA into the binary (mirrors
//! fq-cli/build.rs). The dashboard compares its own SHA against the
//! daemon's over the frozen `ReadService::version` probe to detect
//! build skew (#168), and `--version` prints it so deploy.sh can
//! verify bundle coherence. Degrades to "unknown" when git or the
//! `.git` directory is unavailable (e.g. building from a tarball).

use std::process::Command;

fn main() {
    let sha = git(&["rev-parse", "--short=12", "HEAD"]).unwrap_or_else(|| "unknown".to_string());
    let dirty = git(&["status", "--porcelain"])
        .map(|out| !out.is_empty())
        .unwrap_or(false);
    let sha = if dirty { format!("{sha}-dirty") } else { sha };

    println!("cargo:rustc-env=FQ_GIT_SHA={sha}");

    // Re-run when HEAD moves, not on every source edit.
    if let Some(git_dir) = git(&["rev-parse", "--absolute-git-dir"]) {
        println!("cargo:rerun-if-changed={git_dir}/HEAD");
    }
}

/// Run `git <args>` and return trimmed stdout, or `None` on any failure.
fn git(args: &[&str]) -> Option<String> {
    let output = Command::new("git").args(args).output().ok()?;
    if !output.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&output.stdout).trim().to_string())
}
