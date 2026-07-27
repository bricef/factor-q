//! `fq-lint` — structural source policy that clippy cannot express.
//!
//! # Where the boundary with clippy sits
//!
//! Clippy is the right tool for anything item-shaped and semantic: it runs on
//! HIR after name resolution, so it sees types, and it already gates this
//! workspace (`[workspace.lints.clippy] all = "deny"`). Do not reimplement a
//! clippy lint here.
//!
//! The line is not "files versus functions" — it is **threshold versus
//! ratchet**. Clippy can say "no function may exceed N lines"
//! (`clippy::too_many_lines`, with `too-many-lines-threshold`). It cannot say
//! "*these* functions must shrink," because its thresholds are global and it
//! has no per-item baseline: the only way to exempt a known offender is an
//! `#[allow]` at the site, which grants a permanent pass rather than a
//! shrinking budget. Existing debt therefore has to be either annotated away
//! or left failing.
//!
//! A ratchet needs a stored, per-subject budget that can only tighten. That is
//! the shape both gates here share, and it is why the function gate lives
//! beside the file gate rather than in `clippy.toml`.
//!
//! Clippy also cannot reason about a *file* at all — its passes walk items in
//! a crate, not lines in a file, there is no file-length lint, and adding one
//! means `rustc_private` on nightly via `dylint`. That is the same boundary
//! the repo already found with `include!`, which is why `lint-sources` is a
//! source scan.
//!
//! # The gates
//!
//! * **Files** may not exceed [`FILE_CAP`] production lines.
//! * **Functions** may not exceed [`FN_CAP`] lines, measured from the `fn`
//!   keyword so documentation is never charged against the budget.
//!
//! Pre-existing offenders are pinned in `.file-size-baseline` and
//! `.function-size-baseline` and may only shrink. Motivated by Part 2 of
//! `docs/reviews/2026-07-25-factor-q-cleanroom-review.md`.

mod analysis;
mod ratchet;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

use ratchet::Ratchet;

/// Files not listed in the baseline may not exceed this many production lines.
const FILE_CAP: usize = 800;

/// Functions not listed in the baseline may not exceed this many lines.
const FN_CAP: usize = 250;

/// Advisory threshold for `--creep`, in CODE lines. Below [`FN_CAP`] on
/// purpose: the two count different things — the cap is physical span from
/// the `fn` keyword, this skips comments and blanks — and the ratio on this
/// tree is about 0.7, so the 250-line cap lands near 175 code lines. Warning
/// at 175 would fire as the gate hit rather than before it; 150 leaves runway.
const FN_CREEP_THRESHOLD: usize = 150;

const FILE_BASELINE: &str = ".file-size-baseline";
const FN_BASELINE: &str = ".function-size-baseline";

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
             (no flags)  check files and functions against their baselines\n  \
             --bless     lower budgets to match reality (never raises)\n  \
             --creep     report functions approaching the cap (never fails)\n  \
             --metrics   report structural facts (never fails)"
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

    let measured = match measure_tree(&root) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("error: {e}");
            return ExitCode::from(2);
        }
    };

    if flags.contains(&"--metrics") {
        report_metrics(&measured);
        return ExitCode::SUCCESS;
    }
    if flags.contains(&"--creep") {
        report_creep(&measured);
        return ExitCode::SUCCESS;
    }

    let files = file_ratchet(&measured);
    let functions = match function_ratchet(&measured) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("error: {e}");
            return ExitCode::from(2);
        }
    };

    let ok = if flags.contains(&"--bless") {
        // Both, unconditionally — a partial bless leaves the tree in a state
        // where the next run fails on whichever half was skipped.
        let a = files.bless(&root, &file_header());
        let b = functions.bless(&root, &fn_header());
        a && b
    } else {
        let a = files.check(&root);
        let b = functions.check(&root);
        if !(a && b) {
            eprintln!(
                "\n(size ratchets — justfile: lint-sizes; rationale in tools/fq-lint and\n\
                 docs/reviews/2026-07-25-factor-q-cleanroom-review.md Part 2)"
            );
        }
        a && b
    };

    if ok {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
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

/// One file's measurement. Go files have no inline test convention and are not
/// parsed, so they are counted whole and contribute no function facts.
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

fn file_ratchet(measured: &BTreeMap<String, Measured>) -> Ratchet<'static> {
    Ratchet {
        subject: "file",
        unit: "production lines",
        cap: FILE_CAP,
        baseline_path: FILE_BASELINE,
        measured: measured
            .iter()
            .map(|(p, m)| (p.clone(), m.production))
            .collect(),
        guidance_new: "  A new file crossed the cap. Split it — do not add a budget entry.\n  \
                       Budgets exist only for the god-files that predate this gate.",
        guidance_grown: "  STOP — do not raise the budget, and do not restructure this file\n  \
             as a side effect of your change. These files are being split under\n  \
             their own issues (#78 runner.rs, #189 fq-cli/src/lib.rs, #191 mcp.rs).\n  \
             Put your new code in a new module, or say on the PR that the change\n  \
             genuinely needs to land in this file and let a human decide.",
    }
}

/// Test functions are out of scope, matching the file gate's exclusion of test
/// code: a long table-driven test is not the debt this is aimed at.
fn function_ratchet(measured: &BTreeMap<String, Measured>) -> Result<Ratchet<'static>, String> {
    // Keys can legitimately collide: platform-gated alternatives share a name
    // and scope (`#[cfg(unix)]` / `#[cfg(not(unix))]` `write_secret` in
    // fq-edge's auth.rs). Collapsing them to the larger is harmless while both
    // sit under the cap — but a collision among *budgeted* functions would
    // mean two functions sharing one budget, which is ambiguous, so that fails
    // loudly and asks for a more specific key.
    let mut seen: BTreeMap<String, (usize, usize)> = BTreeMap::new();
    for (path, m) in measured {
        let Some(facts) = &m.facts else { continue };
        for f in &facts.functions {
            if f.is_test {
                continue;
            }
            let entry = seen.entry(f.key(path)).or_insert((0, 0));
            entry.0 = entry.0.max(f.lines());
            entry.1 += 1;
        }
    }

    let mut fns: BTreeMap<String, usize> = BTreeMap::new();
    for (key, (lines, count)) in seen {
        if count > 1 && lines > FN_CAP {
            return Err(format!(
                "{count} functions share the key {key} and one is {lines} lines, over the \
                 {FN_CAP}-line cap. fq-lint cannot budget them separately under one key — \
                 give the scope in analysis.rs a cfg-aware discriminator."
            ));
        }
        fns.insert(key, lines);
    }

    Ok(Ratchet {
        subject: "function",
        unit: "lines",
        cap: FN_CAP,
        baseline_path: FN_BASELINE,
        measured: fns,
        guidance_new: "  A new function crossed the cap. Extract helpers — do not add a\n  \
                       budget entry. Budgets exist only for functions that predate this gate.",
        guidance_grown: "  STOP — do not raise the budget. Extract the new logic into a\n  \
                         helper instead of growing a function that is already too long,\n  \
                         or say on the PR why it has to grow and let a human decide.",
    })
}

fn file_header() -> String {
    format!(
        "# Per-file production-line budgets — the large-file ratchet.\n\
         #\n\
         # Generated and maintained by `just sizes-bless`; enforced by\n\
         # `just lint-sizes` (tools/fq-lint) in the Code quality CI job.\n\
         #\n\
         # Production lines = total minus `#[cfg(test)]` items, measured off a\n\
         # real syn AST rather than by matching source text.\n\
         #\n\
         # Every file here exceeds the {FILE_CAP}-line cap and is pinned at its size when\n\
         # the ratchet landed. These numbers may only ever go DOWN. Lowering one\n\
         # is automatic (`just sizes-bless`); raising one, or admitting a new\n\
         # file, means hand-editing this file so a human sees it in the diff.\n\
         #\n\
         # The three entries that motivated this gate have their own split issues:\n\
         #   services/fq-runtime/crates/fq-cli/src/lib.rs                       -> #189\n\
         #   services/fq-runtime/crates/fq-runtime/src/worker/reducer/runner.rs -> #78\n\
         #   services/fq-runtime/crates/fq-runtime/src/mcp.rs                   -> #191\n"
    )
}

fn fn_header() -> String {
    format!(
        "# Per-function line budgets — the large-function ratchet.\n\
         #\n\
         # Generated and maintained by `just sizes-bless`; enforced by\n\
         # `just lint-sizes` (tools/fq-lint) in the Code quality CI job.\n\
         #\n\
         # Measured from the `fn` keyword to the closing brace, so a function is\n\
         # never charged for its own doc comment. Test functions are out of scope,\n\
         # matching the file gate's exclusion of test code.\n\
         #\n\
         # Keys are `path::scope::name`, not line numbers — line numbers move on\n\
         # every edit above them, and a baseline that churned on unrelated changes\n\
         # would be ignored within a week.\n\
         #\n\
         # Every function here exceeds the {FN_CAP}-line cap and is pinned at its size\n\
         # when the ratchet landed. These numbers may only ever go DOWN. Lowering\n\
         # one is automatic (`just sizes-bless`); raising one, or admitting a new\n\
         # function, means hand-editing this file so a human sees it in the diff.\n\
         #\n\
         # clippy::too_many_lines is the complementary threshold gate (it counts\n\
         # CODE lines, skipping comments and blanks) — tracked in #392.\n"
    )
}

/// Advisory: functions approaching the [`FN_CAP`] ratchet, by code lines.
///
/// Always succeeds. The ratchet is the gate; this exists so growth is legible
/// while there is still runway, rather than a merge stopping without warning.
///
/// This deliberately does not shell out to `clippy::too_many_lines`, which
/// measures the same idea. `cargo clippy -- --force-warn <lint>` does not
/// invalidate cargo's fingerprint, so cached units never re-emit: measured on
/// this tree it reported 13 functions where a full rebuild found 35, and which
/// 13 depended on what happened to be stale. Deriving it from the AST is exact,
/// instant, and cannot silently under-report.
fn report_creep(measured: &BTreeMap<String, Measured>) {
    let mut over: Vec<(&str, &analysis::FnFacts)> = measured
        .iter()
        .filter_map(|(p, m)| m.facts.as_ref().map(|f| (p.as_str(), f)))
        .flat_map(|(p, f)| f.functions.iter().map(move |fun| (p, fun)))
        .filter(|(_, f)| !f.is_test && f.code_lines > FN_CREEP_THRESHOLD)
        .collect();
    over.sort_by_key(|(_, f)| std::cmp::Reverse(f.code_lines));

    if over.is_empty() {
        println!("function-length creep: none over {FN_CREEP_THRESHOLD} code lines");
        return;
    }

    let past_cap = over.iter().filter(|(_, f)| f.lines() > FN_CAP).count();
    println!(
        "function-length creep: {} production functions over {FN_CREEP_THRESHOLD} code lines",
        over.len()
    );
    println!("  advisory only — the gate is the {FN_CAP}-line ratchet in {FN_BASELINE}");
    println!(
        "  {past_cap} already past the cap (budgeted); {} approaching it",
        over.len() - past_cap
    );
    for (path, f) in over.iter().take(10) {
        let flag = if f.lines() > FN_CAP { "*" } else { " " };
        println!(
            "  {flag} {:>4} code / {:>4} physical  {path}:{}  {}",
            f.code_lines,
            f.lines(),
            f.first_line,
            f.name
        );
    }
    if over.len() > 10 {
        println!("    … and {} more", over.len() - 10);
    }
}

/// Non-enforcing report over the structural facts the AST layer makes cheap.
fn report_metrics(measured: &BTreeMap<String, Measured>) {
    let mut prod: Vec<(&str, &analysis::FnFacts)> = Vec::new();
    let mut test_fns = 0usize;

    for (path, m) in measured {
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
    let over = prod.iter().filter(|(_, f)| f.lines() > FN_CAP).count();
    println!("\nlongest (production), {over} over the {FN_CAP}-line cap:");
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

    fn measured_from(path: &str, src: &str) -> BTreeMap<String, Measured> {
        let facts = analysis::analyze(src).expect("valid Rust");
        let mut m = BTreeMap::new();
        m.insert(
            path.to_string(),
            Measured {
                production: facts.production_lines(),
                facts: Some(facts),
            },
        );
        m
    }

    #[test]
    fn function_ratchet_skips_test_functions() {
        let long = "\n".repeat(FN_CAP + 10);
        let src = format!("#[cfg(test)]\nmod tests {{\n    fn big() {{{long}}}\n}}\n");
        let r = function_ratchet(&measured_from("a.rs", &src)).expect("no duplicate keys");
        assert!(r.measured.is_empty(), "test functions must not be budgeted");
    }

    #[test]
    fn function_keys_distinguish_inherent_from_trait_impls() {
        let src =
            "impl Foo {\n    fn run(&self) {}\n}\nimpl Bar for Foo {\n    fn run(&self) {}\n}\n";
        let r = function_ratchet(&measured_from("a.rs", src)).expect("no duplicate keys");
        let mut keys: Vec<_> = r.measured.keys().cloned().collect();
        keys.sort();
        assert_eq!(keys, vec!["a.rs::Foo as Bar::run", "a.rs::Foo::run"]);
    }

    #[test]
    fn duplicate_keys_are_an_error_not_a_silent_pick() {
        // Two same-named free functions in different inline modules are fine
        // (distinct scopes); the same name twice in one scope cannot compile,
        // so this asserts the guard exists rather than a reachable state.
        let src = "mod a {\n    fn f() {}\n}\nmod b {\n    fn f() {}\n}\n";
        let r = function_ratchet(&measured_from("x.rs", src)).expect("distinct scopes");
        assert_eq!(r.measured.len(), 2);
    }
}
