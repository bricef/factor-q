//! `fq-lint` — structural source policy that clippy cannot express.
//!
//! # Why this exists alongside clippy
//!
//! Clippy is the right tool for anything item-shaped and semantic: it runs on
//! HIR after name resolution, so it sees types, and it already gates this
//! workspace (`[workspace.lints.clippy] all = "deny"`). Function length,
//! argument counts and complexity all belong to clippy — `too_many_lines`,
//! `too_many_arguments` and `cognitive_complexity` exist and are configurable
//! through `clippy.toml`. Do not reimplement those here.
//!
//! What clippy structurally cannot do is reason about a *file*. Its lint
//! passes walk items in a crate, not lines in a file, and there is no
//! file-length lint — nor any way to add one, because clippy's lints are
//! compiled into clippy. Custom lints mean `rustc_private` on nightly (via
//! `dylint` or a clippy fork), which would trade this repo's stable, pinned
//! toolchain for a nightly one that must track rustc exactly.
//!
//! This is the same boundary the repo already found with `include!`: clippy
//! could not see it, so `just lint-sources` is a source scan. `fq-lint` is the
//! structural counterpart, built on a real AST rather than on text.
//!
//! # The gate
//!
//! Files carry a production-line budget. New files may not exceed [`CAP`]
//! production lines; the god-files that predate the gate are pinned at their
//! size in `.file-size-baseline` and may only ever shrink. Motivated by Part 2
//! of `docs/reviews/2026-07-25-factor-q-cleanroom-review.md`: three split
//! issues stayed open across two reviews while every file they named grew.

mod analysis;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

/// Files not listed in the baseline may not exceed this many production lines.
const CAP: usize = 800;

/// How far a budget may drift above a file's real size before CI demands it be
/// lowered. This is what makes the gate a ratchet rather than a one-off
/// freeze: without it a file shrinks and quietly regrows into its old budget.
const STALENESS_SLACK: usize = 100;

const BASELINE_PATH: &str = ".file-size-baseline";

/// Test code by purpose, excluded wholesale. `tests/` and `benches/` are test
/// targets; `test_support/` is test infrastructure. Note that fq-dashboard's
/// `fixtures.rs` is deliberately *not* excluded — it compiles into the
/// shipping binary and drives `fq-dashboard render-fixtures`.
const EXCLUDED_DIRS: &[&str] = &["tests", "benches", "test_support"];

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let flags: Vec<&str> = args.iter().map(String::as_str).collect();

    if flags.contains(&"--help") || flags.contains(&"-h") {
        eprintln!(
            "usage: fq-lint [--bless | --metrics]\n\n  \
             (no flags)  check every file against .file-size-baseline\n  \
             --bless     lower budgets to match reality (never raises)\n  \
             --metrics   report function-length and arity facts (never fails)"
        );
        return ExitCode::SUCCESS;
    }

    let root = match repo_root() {
        Ok(r) => r,
        Err(e) => {
            eprintln!("error: {e}");
            return ExitCode::from(2);
        }
    };

    let sizes = match measure_tree(&root) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("error: {e}");
            return ExitCode::from(2);
        }
    };

    if flags.contains(&"--metrics") {
        report_metrics(&root, &sizes);
        return ExitCode::SUCCESS;
    }
    if flags.contains(&"--bless") {
        return bless(&root, &sizes);
    }
    check(&root, &sizes)
}

fn repo_root() -> Result<PathBuf, String> {
    let out = Command::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .output()
        .map_err(|e| format!("running git: {e}"))?;
    if !out.status.success() {
        return Err("not inside a git repository".into());
    }
    Ok(PathBuf::from(
        String::from_utf8_lossy(&out.stdout).trim().to_string(),
    ))
}

fn in_scope(path: &str) -> bool {
    if Path::new(path)
        .components()
        .any(|c| EXCLUDED_DIRS.contains(&c.as_os_str().to_string_lossy().as_ref()))
    {
        return false;
    }
    if path.ends_with("_test.go") {
        return false;
    }
    path.ends_with(".rs") || path.ends_with(".go")
}

/// One file's measurement. Go files have no inline test convention, so they
/// are counted whole; Rust files go through the AST.
struct Measured {
    production: usize,
    facts: Option<analysis::FileFacts>,
}

fn measure_tree(root: &Path) -> Result<BTreeMap<String, Measured>, String> {
    let out = Command::new("git")
        .args(["ls-files", "-z", "*.rs", "*.go"])
        .current_dir(root)
        .output()
        .map_err(|e| format!("running git ls-files: {e}"))?;

    let mut sizes = BTreeMap::new();
    for rel in String::from_utf8_lossy(&out.stdout).split('\0') {
        if rel.is_empty() || !in_scope(rel) {
            continue;
        }
        let src =
            std::fs::read_to_string(root.join(rel)).map_err(|e| format!("reading {rel}: {e}"))?;

        if rel.ends_with(".go") {
            sizes.insert(
                rel.to_string(),
                Measured {
                    production: src.split('\n').count(),
                    facts: None,
                },
            );
            continue;
        }

        // A parse failure is fatal, never a fallback to guessing: the whole
        // point of using a real parser is that the numbers are exact.
        let facts = analysis::analyze(&src)
            .map_err(|e| format!("{rel}: not valid Rust ({e}). fq-lint will not guess."))?;
        sizes.insert(
            rel.to_string(),
            Measured {
                production: facts.production_lines(),
                facts: Some(facts),
            },
        );
    }
    Ok(sizes)
}

fn over_cap(sizes: &BTreeMap<String, Measured>) -> BTreeMap<String, usize> {
    sizes
        .iter()
        .filter(|(_, m)| m.production > CAP)
        .map(|(p, m)| (p.clone(), m.production))
        .collect()
}

fn read_baseline(root: &Path) -> BTreeMap<String, usize> {
    let Ok(text) = std::fs::read_to_string(root.join(BASELINE_PATH)) else {
        return BTreeMap::new();
    };
    text.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .filter_map(|l| {
            let (path, budget) = l.rsplit_once(' ')?;
            Some((path.trim().to_string(), budget.parse().ok()?))
        })
        .collect()
}

fn write_baseline(root: &Path, entries: &BTreeMap<String, usize>) -> std::io::Result<()> {
    let mut out = format!(
        "# Per-file production-line budgets — the large-file ratchet.\n\
         #\n\
         # Generated and maintained by `just filesize-bless`; enforced by\n\
         # `just lint-filesize` (tools/fq-lint) in the source-policy CI job.\n\
         #\n\
         # Production lines = total minus `#[cfg(test)]` items, measured off a\n\
         # real syn AST rather than by matching source text.\n\
         #\n\
         # Every file here exceeds the {CAP}-line cap and is pinned at its size when\n\
         # the ratchet landed. These numbers may only ever go DOWN. Lowering one\n\
         # is automatic (`just filesize-bless`); raising one, or admitting a new\n\
         # file, means hand-editing this file so a human sees it in the diff.\n\
         #\n\
         # The three entries that motivated this gate have their own split issues:\n\
         #   services/fq-runtime/crates/fq-cli/src/lib.rs                       -> #189\n\
         #   services/fq-runtime/crates/fq-runtime/src/worker/reducer/runner.rs -> #78\n\
         #   services/fq-runtime/crates/fq-runtime/src/mcp.rs                   -> #191\n"
    );
    for (path, budget) in entries {
        out.push_str(&format!("{path} {budget}\n"));
    }
    std::fs::write(root.join(BASELINE_PATH), out)
}

fn bless(root: &Path, sizes: &BTreeMap<String, Measured>) -> ExitCode {
    let current = over_cap(sizes);
    let previous = read_baseline(root);

    let raised: Vec<_> = current
        .iter()
        .filter_map(|(p, &n)| previous.get(p).filter(|&&b| n > b).map(|&b| (p, b, n)))
        .collect();
    if !raised.is_empty() {
        eprintln!(
            "refusing to bless: these files GREW beyond their budget.\n\
             The ratchet only ever tightens — shrink the file, or hand-edit\n\
             {BASELINE_PATH} if a bigger budget is genuinely the right call.\n"
        );
        for (p, was, now) in raised {
            eprintln!("  {p}: {was} -> {now} (+{})", now - was);
        }
        return ExitCode::FAILURE;
    }

    // Blessing must never be able to legitimise a brand-new god-file, or the
    // cap would be advisory: an agent that tripped the gate could clear it by
    // running this command.
    let fresh: Vec<_> = current
        .iter()
        .filter(|(p, _)| !previous.contains_key(*p))
        .collect();
    if !fresh.is_empty() && !previous.is_empty() {
        eprintln!(
            "refusing to bless: these files newly exceed the {CAP}-line cap.\n\
             Split them. `--bless` only lowers and drops existing budgets — it\n\
             cannot admit a new file to {BASELINE_PATH}. If one genuinely\n\
             belongs there, add it by hand so a human sees it in the diff.\n"
        );
        for (p, n) in fresh {
            eprintln!("  {p}: {n} lines (cap {CAP})");
        }
        return ExitCode::FAILURE;
    }

    if let Err(e) = write_baseline(root, &current) {
        eprintln!("error: writing {BASELINE_PATH}: {e}");
        return ExitCode::from(2);
    }
    let lowered = current
        .iter()
        .filter(|(p, n)| previous.get(*p).is_some_and(|&b| **n < b))
        .count();
    let added = current
        .keys()
        .filter(|p| !previous.contains_key(*p))
        .count();
    let dropped = previous
        .keys()
        .filter(|p| !current.contains_key(*p))
        .count();
    println!(
        "blessed {} entries in {BASELINE_PATH} ({lowered} lowered, {added} added, {dropped} dropped)",
        current.len()
    );
    ExitCode::SUCCESS
}

fn check(root: &Path, sizes: &BTreeMap<String, Measured>) -> ExitCode {
    let baseline = read_baseline(root);
    let current = over_cap(sizes);

    let new_offenders: Vec<_> = current
        .iter()
        .filter(|(p, _)| !baseline.contains_key(*p))
        .collect();
    let grown: Vec<_> = current
        .iter()
        .filter_map(|(p, &n)| baseline.get(p).filter(|&&b| n > b).map(|&b| (p, b, n)))
        .collect();
    let stale: Vec<_> = baseline
        .iter()
        .filter_map(|(p, &b)| {
            let m = sizes.get(p)?;
            (b.saturating_sub(m.production) > STALENESS_SLACK).then_some((p, b, m.production))
        })
        .collect();
    let obsolete: Vec<_> = baseline
        .keys()
        .filter(|p| sizes.get(*p).is_none_or(|m| m.production <= CAP))
        .collect();

    if new_offenders.is_empty() && grown.is_empty() && stale.is_empty() && obsolete.is_empty() {
        println!(
            "file-size ratchet: {} files in scope, {} budgeted, all within budget",
            sizes.len(),
            baseline.len()
        );
        return ExitCode::SUCCESS;
    }

    if !new_offenders.is_empty() {
        eprintln!("\nerror: file exceeds the {CAP}-line production cap and has no budget:");
        for (p, n) in new_offenders {
            eprintln!("  {p}: {n} lines (cap {CAP})");
        }
        eprintln!(
            "\n  A new file crossed the cap. Split it — do not add a budget entry.\n  \
             Budgets exist only for the god-files that predate this gate."
        );
    }

    if !grown.is_empty() {
        eprintln!("\nerror: file grew beyond its budget:");
        for (p, was, now) in grown {
            eprintln!("  {p}: {was} -> {now} (+{})", now - was);
        }
        eprintln!(
            "\n  STOP — do not raise the budget, and do not restructure this file\n  \
             as a side effect of your change. These files are being split under\n  \
             their own issues (#78 runner.rs, #189 fq-cli/src/lib.rs, #191 mcp.rs).\n  \
             Put your new code in a new module, or say on the PR that the change\n  \
             genuinely needs to land in this file and let a human decide."
        );
    }

    if !stale.is_empty() {
        eprintln!(
            "\nerror: budget is stale by more than {STALENESS_SLACK} lines — the ratchet must tighten:"
        );
        for (p, was, now) in stale {
            eprintln!(
                "  {p}: budget {was}, actual {now} ({} lines of slack)",
                was - now
            );
        }
        eprintln!("\n  Fix: run `just filesize-bless` and commit the result.");
    }

    if !obsolete.is_empty() {
        eprintln!("\nerror: budget entry no longer needed (file is gone or under the cap):");
        for p in obsolete {
            eprintln!("  {p}");
        }
        eprintln!("\n  Fix: run `just filesize-bless` and commit the result.");
    }

    eprintln!(
        "\n(file-size ratchet — justfile: lint-filesize; rationale in tools/fq-lint and\n\
         docs/reviews/2026-07-25-factor-q-cleanroom-review.md Part 2)"
    );
    ExitCode::FAILURE
}

/// Non-enforcing report. Exists to show what the AST layer makes cheap —
/// these are the Part 5 metrics the review counted by hand. Function *length*
/// is deliberately left to `clippy::too_many_lines`, which counts code lines
/// rather than physical span and is the better measure.
fn report_metrics(_root: &Path, sizes: &BTreeMap<String, Measured>) {
    let mut prod: Vec<(&str, &analysis::FnFacts)> = Vec::new();
    let mut test_fns = 0usize;

    for (path, m) in sizes {
        let Some(facts) = &m.facts else { continue };
        for f in &facts.functions {
            if f.is_test {
                test_fns += 1;
            } else {
                prod.push((path, f));
            }
        }
    }

    println!("functions: {} production, {test_fns} test", prod.len());

    prod.sort_by_key(|(_, f)| std::cmp::Reverse(f.params));
    println!(
        "\nhighest arity (production) — the quantity behind the tree's `too_many_arguments` allows:"
    );
    for (path, f) in prod.iter().take(10) {
        println!(
            "  {:>2} params  {path}:{}  {}",
            f.params, f.first_line, f.name
        );
    }

    prod.sort_by_key(|(_, f)| std::cmp::Reverse(f.lines()));
    println!(
        "\nlargest by physical span (production) — this is what drives file size.\n\
         Note clippy::too_many_lines counts CODE lines instead, which is the better\n\
         measure of complexity and is the one that should gate:"
    );
    for (path, f) in prod.iter().take(10) {
        println!(
            "  {:>5} lines  {path}:{}  {}",
            f.lines(),
            f.first_line,
            f.name
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scope_excludes_test_targets_and_go_tests() {
        assert!(in_scope("services/fq-runtime/src/mcp.rs"));
        assert!(in_scope("adapters/github-watcher/main.go"));
        assert!(!in_scope("services/fq-runtime/tests/mcp_integration.rs"));
        assert!(!in_scope("services/fq-store/benches/bench.rs"));
        assert!(!in_scope("services/fq-runtime/src/test_support/sim.rs"));
        assert!(!in_scope("adapters/github-watcher/main_test.go"));
        assert!(!in_scope("README.md"));
    }

    #[test]
    fn dashboard_fixtures_are_production_code() {
        // Compiled into the shipping binary; drives `fq-dashboard
        // render-fixtures`. Excluding it would be wrong.
        assert!(in_scope("services/fq-dashboard/src/fixtures.rs"));
    }
}
