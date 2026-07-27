//! The ratchet mechanism, independent of what is being measured.
//!
//! A ratchet is a cap plus a baseline of pre-existing offenders. Three rules,
//! and the third is the one that makes it a *ratchet* rather than a freeze:
//!
//! 1. Anything over the cap that is not in the baseline fails. New offenders
//!    are never admitted automatically.
//! 2. Anything in the baseline that grew past its budget fails.
//! 3. Anything in the baseline that has shrunk well below its budget fails,
//!    demanding the budget be lowered. Without this a subject shrinks and then
//!    quietly regrows into its old allowance.
//!
//! Blessing lowers and drops entries. It never raises one and never admits a
//! new one — otherwise the cap would be advisory, since anything that tripped
//! the gate could be cleared by running the blessing command.

use std::collections::BTreeMap;
use std::path::Path;

/// How far a budget may drift above reality before CI demands it be lowered.
pub const STALENESS_SLACK: usize = 100;

pub struct Ratchet<'a> {
    /// Human label for the subject, e.g. "file" or "function".
    pub subject: &'a str,
    /// Unit shown in messages, e.g. "production lines".
    pub unit: &'a str,
    pub cap: usize,
    pub baseline_path: &'a str,
    /// Every measured subject, keyed by its stable identity.
    pub measured: BTreeMap<String, usize>,
    /// Extra guidance printed when a subject exceeds the cap with no budget.
    pub guidance_new: &'a str,
    /// Extra guidance printed when a budgeted subject grew.
    pub guidance_grown: &'a str,
}

impl Ratchet<'_> {
    fn over_cap(&self) -> BTreeMap<&str, usize> {
        self.measured
            .iter()
            .filter(|&(_, &n)| n > self.cap)
            .map(|(k, &n)| (k.as_str(), n))
            .collect()
    }

    pub fn read_baseline(&self, root: &Path) -> BTreeMap<String, usize> {
        let Ok(text) = std::fs::read_to_string(root.join(self.baseline_path)) else {
            return BTreeMap::new();
        };
        text.lines()
            .map(str::trim)
            .filter(|l| !l.is_empty() && !l.starts_with('#'))
            .filter_map(|l| {
                let (key, budget) = l.rsplit_once(' ')?;
                Some((key.trim().to_string(), budget.parse().ok()?))
            })
            .collect()
    }

    fn write_baseline(
        &self,
        root: &Path,
        header: &str,
        entries: &BTreeMap<&str, usize>,
    ) -> std::io::Result<()> {
        let mut out = header.to_string();
        for (key, budget) in entries {
            out.push_str(&format!("{key} {budget}\n"));
        }
        std::fs::write(root.join(self.baseline_path), out)
    }

    /// Returns true on success.
    pub fn check(&self, root: &Path) -> bool {
        let baseline = self.read_baseline(root);
        let current = self.over_cap();

        let new_offenders: Vec<_> = current
            .iter()
            .filter(|(k, _)| !baseline.contains_key(**k))
            .collect();
        let grown: Vec<_> = current
            .iter()
            .filter_map(|(k, &n)| baseline.get(*k).filter(|&&b| n > b).map(|&b| (*k, b, n)))
            .collect();
        let stale: Vec<_> = baseline
            .iter()
            .filter_map(|(k, &b)| {
                let now = *self.measured.get(k)?;
                (b.saturating_sub(now) > STALENESS_SLACK).then_some((k.as_str(), b, now))
            })
            .collect();
        let obsolete: Vec<_> = baseline
            .keys()
            .filter(|k| self.measured.get(*k).is_none_or(|&n| n <= self.cap))
            .collect();

        if new_offenders.is_empty() && grown.is_empty() && stale.is_empty() && obsolete.is_empty() {
            println!(
                "{} ratchet: {} measured, {} budgeted, all within budget",
                self.subject,
                self.measured.len(),
                baseline.len()
            );
            return true;
        }

        if !new_offenders.is_empty() {
            eprintln!(
                "\nerror: {} exceeds the cap of {} {} and has no budget:",
                self.subject, self.cap, self.unit
            );
            for (k, n) in new_offenders {
                eprintln!("  {k}: {n} {} (cap {})", self.unit, self.cap);
            }
            eprintln!("\n{}", self.guidance_new);
        }

        if !grown.is_empty() {
            eprintln!("\nerror: {} grew beyond its budget:", self.subject);
            for (k, was, now) in grown {
                eprintln!("  {k}: {was} -> {now} (+{})", now - was);
            }
            eprintln!("\n{}", self.guidance_grown);
        }

        if !stale.is_empty() {
            eprintln!(
                "\nerror: budget is stale by more than {STALENESS_SLACK} {} — the ratchet must tighten:",
                self.unit
            );
            for (k, was, now) in stale {
                eprintln!("  {k}: budget {was}, actual {now} ({} of slack)", was - now);
            }
            eprintln!("\n  Fix: run `just sizes-bless` and commit the result.");
        }

        if !obsolete.is_empty() {
            eprintln!(
                "\nerror: budget entry no longer needed ({} is gone or under the cap):",
                self.subject
            );
            for k in obsolete {
                eprintln!("  {k}");
            }
            eprintln!("\n  Fix: run `just sizes-bless` and commit the result.");
        }
        false
    }

    /// Returns true on success.
    pub fn bless(&self, root: &Path, header: &str) -> bool {
        let current = self.over_cap();
        let previous = self.read_baseline(root);

        let raised: Vec<_> = current
            .iter()
            .filter_map(|(k, &n)| previous.get(*k).filter(|&&b| n > b).map(|&b| (*k, b, n)))
            .collect();
        if !raised.is_empty() {
            eprintln!(
                "refusing to bless: these {}s GREW beyond their budget.\n\
                 The ratchet only ever tightens — shrink them, or hand-edit\n\
                 {} if a bigger budget is genuinely the right call.\n",
                self.subject, self.baseline_path
            );
            for (k, was, now) in raised {
                eprintln!("  {k}: {was} -> {now} (+{})", now - was);
            }
            return false;
        }

        let fresh: Vec<_> = current
            .iter()
            .filter(|(k, _)| !previous.contains_key(**k))
            .collect();
        if !fresh.is_empty() && !previous.is_empty() {
            eprintln!(
                "refusing to bless: these {}s newly exceed the cap of {} {}.\n\
                 Fix them. `--bless` only lowers and drops existing budgets — it\n\
                 cannot admit a new entry to {}. If one genuinely belongs\n\
                 there, add it by hand so a human sees it in the diff.\n",
                self.subject, self.cap, self.unit, self.baseline_path
            );
            for (k, n) in fresh {
                eprintln!("  {k}: {n} {} (cap {})", self.unit, self.cap);
            }
            return false;
        }

        if let Err(e) = self.write_baseline(root, header, &current) {
            eprintln!("error: writing {}: {e}", self.baseline_path);
            return false;
        }
        let lowered = current
            .iter()
            .filter(|(k, n)| previous.get(**k).is_some_and(|&b| **n < b))
            .count();
        let added = current
            .keys()
            .filter(|k| !previous.contains_key(**k))
            .count();
        let dropped = previous
            .keys()
            .filter(|k| !current.contains_key(k.as_str()))
            .count();
        println!(
            "blessed {} entries in {} ({lowered} lowered, {added} added, {dropped} dropped)",
            current.len(),
            self.baseline_path
        );
        true
    }
}
