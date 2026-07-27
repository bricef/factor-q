//! Module coupling — which top-level modules of a crate depend on which.
//!
//! # Why this sits next to the size ratchets
//!
//! A file-size ratchet with no coupling counterweight rewards the wrong
//! refactor. Splitting a god-file into two halves that import each other
//! heavily satisfies `lint-sizes` and leaves the tree worse: a module boundary
//! now exists that carries no semantic weight. Nothing in `just quality` could
//! previously tell that split apart from a real one — this can.
//!
//! # What an edge is
//!
//! One edge per *distinct target module*, built from every `crate::`- and
//! `super::`-rooted path in production code ([`crate::analysis::ModuleRef`]).
//! `use crate::worker::Handle` and a bare `crate::worker::spawn()` in an
//! expression are the same edge; the count behind each edge is kept so the
//! heaviest dependencies can be named.
//!
//! Granularity is the crate's **top-level** modules — `worker`, not
//! `worker::reducer::runner`. That is the altitude at which the answer changes
//! a decision: a cycle between `worker` and `events` is an architectural fact,
//! while one between two sibling files inside `worker` is a detail of how that
//! module is laid out internally.
//!
//! # What is deliberately not counted
//!
//! * **Test code.** `#[cfg(test)]` items, and the `tests/`, `benches/` and
//!   `test_support/` trees the size gates already exclude. Test-only coupling
//!   is real but is not the debt this is aimed at.
//! * **References to crate-root items.** `crate::SomeType` names a type
//!   re-exported at the root, not a module, and counting it would invent a
//!   node. Only heads that resolve to a real module become edges — the
//!   two-pass build below exists for exactly this.
//! * **Cross-crate dependencies.** Cargo already makes those explicit and
//!   acyclic; the interesting graph is the one inside a crate, which nothing
//!   checks.
//!
//! # Known floor
//!
//! Rust lets a module be reached without naming it: a re-export
//! (`pub use crate::a::T` in `b`, then `crate::b::T` elsewhere) attributes the
//! edge to `b` rather than `a`, and a trait method resolves without any path
//! at all. Every such case *undercounts*, so a reported edge is always real
//! and the totals are a floor. That is the right direction for an advisory
//! metric: it never invents coupling that is not there.

use std::collections::{BTreeMap, BTreeSet};

use crate::analysis::FileFacts;

/// Label for code living directly in `src/lib.rs` or `src/main.rs`.
pub const ROOT: &str = "(root)";

/// One top-level module, with the edges in both directions.
#[derive(Debug, Default, Clone)]
pub struct Module {
    /// Production lines across every file of this module, its submodules
    /// included.
    pub prod_lines: usize,
    pub files: usize,
    /// Target module to the number of referencing paths behind that edge.
    pub depends_on: BTreeMap<String, usize>,
    /// Modules that reference this one.
    pub dependents: BTreeSet<String>,
}

impl Module {
    /// Modules this one depends on — efferent coupling.
    pub fn fan_out(&self) -> usize {
        self.depends_on.len()
    }

    /// Modules that depend on this one — afferent coupling.
    pub fn fan_in(&self) -> usize {
        self.dependents.len()
    }

    /// High fan-in *and* high fan-out at once: expensive to change because
    /// much depends on it, and changed often because it depends on much.
    ///
    /// This is the shape rule that survives translation to Rust from Martin's
    /// instability metric, whose full form needs an abstractness axis that
    /// does not map cleanly here. The threshold is a judgement, not a
    /// measurement, which is why this reports and never gates.
    pub fn is_hub(&self) -> bool {
        self.fan_in() >= HUB_THRESHOLD && self.fan_out() >= HUB_THRESHOLD
    }
}

/// Fan in *and* out at or above this many modules marks a hub.
const HUB_THRESHOLD: usize = 4;

/// One crate's internal module graph.
#[derive(Debug, Clone)]
pub struct CrateGraph {
    /// Crate directory name — `fq-runtime` for
    /// `services/fq-runtime/crates/fq-runtime`.
    pub name: String,
    pub dir: String,
    pub modules: BTreeMap<String, Module>,
    /// Strongly connected components of more than one module. Rust forbids
    /// crate cycles but permits module cycles silently, so nothing else in
    /// the toolchain reports these.
    pub cycles: Vec<Vec<String>>,
}

/// Where a source file sits: its crate, and its module path below `src/`.
fn placement(path: &str) -> Option<(&str, Vec<String>)> {
    let (dir, rest) = match path.strip_prefix("src/") {
        Some(rest) => ("", rest),
        None => {
            let at = path.find("/src/")?;
            (&path[..at], &path[at + "/src/".len()..])
        }
    };
    let stem = rest.strip_suffix(".rs")?;
    let mut parts: Vec<String> = stem.split('/').map(str::to_string).collect();
    match parts.last().map(String::as_str) {
        // `foo/mod.rs` *is* module `foo`, not `foo::mod`.
        Some("mod") => {
            parts.pop();
        }
        // The crate root itself has no module path.
        Some("lib" | "main") if parts.len() == 1 => {
            parts.pop();
        }
        _ => {}
    }
    Some((dir, parts))
}

/// The module a reference lands in, or `None` when it stays inside the
/// referring module.
///
/// `crate::x` names a top-level module directly. `super::x` climbs: from
/// module path `worker::reducer` a single hop reaches `worker`, still the same
/// top-level module, and only a climb that reaches the crate root
/// (`supers == depth`) can name a different one. More hops than depth cannot
/// compile.
fn resolve(supers: usize, head: &str, depth: usize) -> Option<&str> {
    if supers == 0 || supers == depth {
        Some(head)
    } else {
        None
    }
}

/// Build one graph per crate from the measured production files.
pub fn build<'a>(
    files: impl IntoIterator<Item = (&'a str, usize, &'a FileFacts)>,
) -> Vec<CrateGraph> {
    // Placement first, for every file, because edges cannot be resolved until
    // the full set of module names in a crate is known — a `crate::Foo` that
    // matches no module is a root item, not a node.
    let mut placed: Vec<(&str, Vec<String>, usize, &FileFacts)> = Vec::new();
    for (path, prod_lines, facts) in files {
        if let Some((dir, parts)) = placement(path) {
            placed.push((dir, parts, prod_lines, facts));
        }
    }

    let mut by_crate: BTreeMap<&str, Vec<usize>> = BTreeMap::new();
    for (i, (dir, ..)) in placed.iter().enumerate() {
        by_crate.entry(dir).or_default().push(i);
    }

    let mut graphs = Vec::new();
    for (dir, indices) in by_crate {
        let mut modules: BTreeMap<String, Module> = BTreeMap::new();
        for &i in &indices {
            let (_, parts, prod_lines, _) = &placed[i];
            let name = parts.first().map_or(ROOT, String::as_str);
            let m = modules.entry(name.to_string()).or_default();
            m.prod_lines += prod_lines;
            m.files += 1;
        }

        for &i in &indices {
            let (_, parts, _, facts) = &placed[i];
            let source = parts.first().map_or(ROOT, String::as_str).to_string();
            for r in &facts.module_refs {
                if r.is_test {
                    continue;
                }
                let Some(target) = resolve(r.supers, &r.head, parts.len()) else {
                    continue;
                };
                // Not a module: a type or function re-exported at the crate
                // root. Counting it would invent a node that no file backs.
                if target == source || !modules.contains_key(target) {
                    continue;
                }
                let target = target.to_string();
                *modules
                    .get_mut(&source)
                    .expect("source module registered in the first pass")
                    .depends_on
                    .entry(target.clone())
                    .or_insert(0) += 1;
                modules
                    .get_mut(&target)
                    .expect("target checked present above")
                    .dependents
                    .insert(source.clone());
            }
        }

        let cycles = cycles(&modules);
        graphs.push(CrateGraph {
            name: dir.rsplit('/').next().unwrap_or(dir).to_string(),
            dir: dir.to_string(),
            modules,
            cycles,
        });
    }
    graphs
}

/// Strongly connected components of more than one module, via Tarjan.
///
/// An SCC rather than a mutual-pair scan because a three-module cycle
/// (`a -> b -> c -> a`) is the same defect and no pair of its edges is
/// mutual, so a pairwise check would report nothing at all.
fn cycles(modules: &BTreeMap<String, Module>) -> Vec<Vec<String>> {
    struct Walk<'a> {
        graph: &'a BTreeMap<String, Module>,
        index: BTreeMap<&'a str, usize>,
        low: BTreeMap<&'a str, usize>,
        on_stack: BTreeSet<&'a str>,
        stack: Vec<&'a str>,
        next: usize,
        out: Vec<Vec<String>>,
    }

    // Recursion is safe here: depth is bounded by the module count, which is
    // tens per crate, not thousands.
    fn connect<'a>(w: &mut Walk<'a>, v: &'a str) {
        w.index.insert(v, w.next);
        w.low.insert(v, w.next);
        w.next += 1;
        w.stack.push(v);
        w.on_stack.insert(v);

        let graph = w.graph;
        if let Some(node) = graph.get(v) {
            for target in node.depends_on.keys() {
                let t = target.as_str();
                if !w.index.contains_key(t) {
                    connect(w, t);
                    let reachable = w.low[t];
                    let current = w.low[v];
                    w.low.insert(v, current.min(reachable));
                } else if w.on_stack.contains(t) {
                    let seen = w.index[t];
                    let current = w.low[v];
                    w.low.insert(v, current.min(seen));
                }
            }
        }

        if w.low[v] == w.index[v] {
            let mut component = Vec::new();
            while let Some(member) = w.stack.pop() {
                w.on_stack.remove(member);
                component.push(member.to_string());
                if member == v {
                    break;
                }
            }
            if component.len() > 1 {
                component.sort();
                w.out.push(component);
            }
        }
    }

    let mut walk = Walk {
        graph: modules,
        index: BTreeMap::new(),
        low: BTreeMap::new(),
        on_stack: BTreeSet::new(),
        stack: Vec::new(),
        next: 0,
        out: Vec::new(),
    };
    for name in modules.keys() {
        if !walk.index.contains_key(name.as_str()) {
            connect(&mut walk, name);
        }
    }
    walk.out.sort();
    walk.out
}

/// Crates with a single module have no internal coupling to report.
fn worth_reporting(g: &CrateGraph) -> bool {
    g.modules.len() > 1
}

/// Human-readable report, mirroring the table in
/// `docs/reviews/2026-07-27-code-quality-metrics.md`.
pub fn report(graphs: &[CrateGraph]) {
    let reported: Vec<&CrateGraph> = graphs.iter().filter(|g| worth_reporting(g)).collect();
    if reported.is_empty() {
        println!("module coupling: no crate has more than one module");
        return;
    }

    for g in reported {
        let cycle_note = match g.cycles.len() {
            0 => String::new(),
            n => format!("  —  {n} cycle{}", if n == 1 { "" } else { "s" }),
        };
        println!("\n{} — {} modules{cycle_note}", g.name, g.modules.len());
        println!(
            "  {:<24} {:>8} {:>8} {:>7}  depends on",
            "module", "prod", "fan-out", "fan-in"
        );

        let mut rows: Vec<(&String, &Module)> = g.modules.iter().collect();
        rows.sort_by_key(|(name, m)| (std::cmp::Reverse(m.prod_lines), (*name).clone()));
        for (name, m) in rows {
            let deps: Vec<&str> = m.depends_on.keys().map(String::as_str).collect();
            let mark = if m.is_hub() { " *" } else { "  " };
            println!(
                "  {:<24}{mark}{:>6} {:>8} {:>7}  {}",
                name,
                m.prod_lines,
                m.fan_out(),
                m.fan_in(),
                deps.join(", ")
            );
        }

        if g.modules.values().any(Module::is_hub) {
            println!(
                "  * fan-in and fan-out both >= {HUB_THRESHOLD}: expensive to change and changed often."
            );
        }
        for c in &g.cycles {
            // Not `a -> b -> a`: a strongly connected component is a set in
            // which every member reaches every other, by some route. Printing
            // it as a chain would name one arbitrary path and read as though
            // breaking that single edge were enough.
            println!(
                "  cycle group ({} modules, each reaches all the others): {}",
                c.len(),
                c.join(", ")
            );
        }
    }
    println!("\nAdvisory — this exits 0 and gates nothing. Production code only;");
    println!("edges come from `crate::`/`super::` paths, so re-exports undercount.");
}

/// Machine-readable report, for the base-vs-head diff behind the PR comment.
///
/// Stability matters more than shape here: the PR comment compares two runs of
/// this output, so a field rename silently changes every delta.
pub fn report_json(graphs: &[CrateGraph]) {
    let crates: Vec<serde_json::Value> = graphs
        .iter()
        .filter(|g| worth_reporting(g))
        .map(|g| {
            let modules: Vec<serde_json::Value> = g
                .modules
                .iter()
                .map(|(name, m)| {
                    serde_json::json!({
                        "name": name,
                        "prod_lines": m.prod_lines,
                        "files": m.files,
                        "fan_out": m.fan_out(),
                        "fan_in": m.fan_in(),
                        "depends_on": m.depends_on.keys().collect::<Vec<_>>(),
                    })
                })
                .collect();
            serde_json::json!({
                "name": g.name,
                "dir": g.dir,
                "modules": modules,
                "cycles": g.cycles,
            })
        })
        .collect();
    let doc = serde_json::json!({ "crates": crates });
    println!(
        "{}",
        serde_json::to_string_pretty(&doc).expect("plain data")
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::analysis::analyze;

    fn graph(files: &[(&str, &str)]) -> CrateGraph {
        let facts: Vec<(String, FileFacts)> = files
            .iter()
            .map(|(p, src)| ((*p).to_string(), analyze(src).expect("valid Rust")))
            .collect();
        let mut graphs = build(
            facts
                .iter()
                .map(|(p, f)| (p.as_str(), f.production_lines(), f)),
        );
        assert_eq!(graphs.len(), 1, "fixtures live in one crate");
        graphs.remove(0)
    }

    #[test]
    fn placement_maps_files_to_module_paths() {
        assert_eq!(placement("k/src/lib.rs"), Some(("k", vec![])));
        assert_eq!(placement("k/src/main.rs"), Some(("k", vec![])));
        assert_eq!(
            placement("k/src/worker.rs"),
            Some(("k", vec!["worker".into()]))
        );
        assert_eq!(
            placement("k/src/worker/mod.rs"),
            Some(("k", vec!["worker".into()]))
        );
        assert_eq!(
            placement("k/src/worker/reducer/runner.rs"),
            Some((
                "k",
                vec!["worker".into(), "reducer".into(), "runner".into()]
            ))
        );
        assert_eq!(placement("k/build.rs"), None, "not under src/");
    }

    #[test]
    fn a_use_becomes_an_edge_in_both_directions() {
        let g = graph(&[
            ("k/src/a.rs", "use crate::b::T;\n"),
            ("k/src/b.rs", "pub struct T;\n"),
        ]);
        assert_eq!(g.modules["a"].fan_out(), 1);
        assert_eq!(g.modules["a"].fan_in(), 0);
        assert_eq!(g.modules["b"].fan_in(), 1);
        assert_eq!(g.modules["b"].fan_out(), 0);
    }

    #[test]
    fn a_reference_to_a_root_item_is_not_an_edge() {
        // `crate::Config` names a type re-exported at the root. The old
        // hand-rolled scan invented a `ChatResponse` module exactly here.
        let g = graph(&[
            ("k/src/lib.rs", "pub struct Config;\n"),
            ("k/src/a.rs", "use crate::Config;\n"),
        ]);
        assert!(!g.modules.contains_key("Config"), "no phantom module");
        assert_eq!(g.modules["a"].fan_out(), 0);
    }

    #[test]
    fn intra_module_references_are_not_edges() {
        let g = graph(&[
            ("k/src/a/one.rs", "use crate::a::two::T;\n"),
            ("k/src/a/two.rs", "pub struct T;\n"),
            ("k/src/b.rs", "pub struct U;\n"),
        ]);
        assert_eq!(g.modules["a"].fan_out(), 0, "a -> a is not coupling");
    }

    #[test]
    fn a_super_hop_that_stays_inside_the_module_is_not_an_edge() {
        // From `a::two`, `super::one` is `a::one` — same top-level module.
        let g = graph(&[
            ("k/src/a/two.rs", "use super::one::T;\n"),
            ("k/src/a/one.rs", "pub struct T;\n"),
            ("k/src/b.rs", "pub struct U;\n"),
        ]);
        assert_eq!(g.modules["a"].fan_out(), 0);
    }

    #[test]
    fn a_super_hop_reaching_the_root_is_an_edge() {
        // From `a` (depth 1), `super::b` is the crate root's `b`.
        let g = graph(&[
            ("k/src/a.rs", "use super::b::T;\n"),
            ("k/src/b.rs", "pub struct T;\n"),
        ]);
        assert_eq!(g.modules["a"].depends_on.keys().collect::<Vec<_>>(), ["b"]);
    }

    #[test]
    fn edge_weight_counts_paths_not_modules() {
        let g = graph(&[
            (
                "k/src/a.rs",
                "use crate::b::T;\nfn f() {\n    crate::b::go();\n}\n",
            ),
            ("k/src/b.rs", "pub struct T;\n"),
        ]);
        assert_eq!(g.modules["a"].fan_out(), 1, "still one dependency");
        assert_eq!(g.modules["a"].depends_on["b"], 2, "two paths behind it");
    }

    #[test]
    fn test_only_imports_do_not_couple() {
        let g = graph(&[
            (
                "k/src/a.rs",
                "#[cfg(test)]\nmod tests {\n    use crate::b::T;\n}\n",
            ),
            ("k/src/b.rs", "pub struct T;\n"),
        ]);
        assert_eq!(g.modules["a"].fan_out(), 0);
    }

    #[test]
    fn finds_a_two_module_cycle() {
        let g = graph(&[
            ("k/src/a.rs", "use crate::b::T;\n"),
            ("k/src/b.rs", "use crate::a::U;\n"),
        ]);
        assert_eq!(g.cycles, vec![vec!["a".to_string(), "b".to_string()]]);
    }

    #[test]
    fn finds_a_three_module_cycle_no_pair_of_which_is_mutual() {
        // The case a mutual-pair scan reports as clean.
        let g = graph(&[
            ("k/src/a.rs", "use crate::b::T;\n"),
            ("k/src/b.rs", "use crate::c::T;\n"),
            ("k/src/c.rs", "use crate::a::T;\n"),
        ]);
        assert_eq!(
            g.cycles,
            vec![vec!["a".to_string(), "b".to_string(), "c".to_string()]]
        );
    }

    #[test]
    fn an_acyclic_diamond_is_not_a_cycle() {
        let g = graph(&[
            ("k/src/a.rs", "use crate::b::T;\nuse crate::c::T;\n"),
            ("k/src/b.rs", "use crate::d::T;\n"),
            ("k/src/c.rs", "use crate::d::T;\n"),
            ("k/src/d.rs", "pub struct T;\n"),
        ]);
        assert!(g.cycles.is_empty());
    }

    #[test]
    fn submodule_lines_roll_up_into_the_top_level_module() {
        let g = graph(&[
            ("k/src/a/mod.rs", "pub mod one;\n"),
            ("k/src/a/one.rs", "pub struct T;\npub struct U;\n"),
            ("k/src/b.rs", "pub struct V;\n"),
        ]);
        assert_eq!(g.modules["a"].files, 2);
        assert_eq!(g.modules["a"].prod_lines, 2 + 3);
    }

    #[test]
    fn hub_needs_both_directions() {
        let mut m = Module::default();
        for i in 0..HUB_THRESHOLD {
            m.depends_on.insert(format!("out{i}"), 1);
        }
        assert!(!m.is_hub(), "fan-out alone is a leaf, not a hub");
        for i in 0..HUB_THRESHOLD {
            m.dependents.insert(format!("in{i}"));
        }
        assert!(m.is_hub());
    }
}
