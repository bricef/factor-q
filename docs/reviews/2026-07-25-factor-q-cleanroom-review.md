# factor-q — cleanroom review

*Reviewed 2026-07-25 against `main` @ `1d8456e`. Independent read of the working tree: 41,198 production LOC / 41,840 test LOC of Rust, ~28k lines of Markdown, 654 commits since 2026-03-12. Security findings marked **PoC** were reproduced empirically, not inferred.*

I read the prior [2026-07-14 cleanroom review](./2026-07-14-cleanroom-code-review.md) **after** forming my own view, specifically so this document adds rather than repeats. Where we agree I say so briefly and move on; the bulk here is new ground — the strategic layer, the code that landed in the last ten days, and four security findings that review did not surface.

---

## Verdict

The engineering quality is genuinely exceptional and I want to say that plainly before criticising anything. A 1.02 test-to-production ratio, zero `TODO`/`FIXME` markers in 41k lines, 17 `#[allow]` attributes total, no `unsafe` outside test scaffolding and one `SIGPIPE` call, and module headers that cite the ADR or the specific incident that motivated them — that combination is rare in funded teams and near-unheard-of in solo projects. The verification stack (trace oracle, seeded DST, differential models, conformance suite run over the wire) is better than the industry norm. `docs/design/committed/design-principles.md` is a better articulation of engineering values than most companies produce.

**The risk to this project is not code quality. It is three things:**

1. **The requirements layer is unfalsifiable where it matters most.** The Q ladder justifies the entire roadmap and cannot currently be measured.
2. **The product's defining feature is still unbuilt** while 83k lines went into substrate — and the substrate has no natural stopping point.
3. **The review loop is losing to velocity.** Structural debt flagged on 14 July grew by 16–33% in the ten days after. That is a systems problem, not a discipline problem, and it will compound.

Plus a security section: the declared security model and the operating reality of the flagship deployment have diverged materially, and I found one verified sandbox escape.

---

## Part 1 — Approach and requirements

### 1.1 Q200 is load-bearing and unfalsifiable (highest-priority strategic issue)

`VISION.md` makes Q200 the north star: *"for every day of human effort invested in factor-q, the system produces the equivalent of 200 days of work."* Every milestone, and therefore every sequencing decision, hangs off this ladder. But:

- **The denominator is undefined.** Does building factor-q count as "human effort invested in factor-q"? If yes, Q is currently deeply negative and stays negative for years — the ladder is unclimbable by construction. If no, the metric measures operating leverage on a system whose construction cost is excluded, which is a different and much weaker claim than the one the name implies. This ambiguity is not addressed anywhere in the docs.
- **The numerator has no instrument.** "The equivalent of 200 days of work" requires a counterfactual estimate of how long a human would have taken. `STATUS.md` acknowledges this — the M0 plan's "proxy instrumentation (read relative to an expert+frontier baseline)" is listed as pending work needed "to make M1 (Q1) decidable." So M1 is currently undecidable, and it is the *first* numeric rung.
- **The rungs are not commensurable.** M0 is a qualitative capability gate ("can work on itself") declared met on a qualitative signal: 20+ merged PRs, maintainer-confirmed. M1/M2/M3 are ratios. There is no common instrument, so "M0 → M1" is not a step along one axis; it is a change of axis.
- **The M0 evidence has a selection problem.** 20+ accepted PRs is the numerator. The denominator — PRs attempted, abandoned, or reverted; hours spent debugging agent runs; corrective commits — is not reported. Your own memory of this work notes that delegated runs "miss acceptance criteria and show logical errors that don't occur in interactive Claude sessions." That belongs in the M0 evidence.

**This matters more than it looks.** An unfalsifiable north star does not just fail to guide — it actively licenses whatever you were going to do anyway, because no roadmap item can be shown to be off-track. That is likely a contributing cause of issue 1.2 below.

**Recommendation.** Pick one:

- **(a)** Build the instrument before M1, and define the denominator explicitly in `VISION.md`. A crude, honest instrument (task-level: estimate-before, wall-clock-after, human-intervention-count, cost) beats a precise one you never build.
- **(b)** Demote Q from measurement to slogan, and replace the ladder with a capability ladder whose rungs are decidable by inspection: *"an agent lands a PR unattended"* (met), *"two agents cooperate on one task"*, *"a non-code workload runs end to end"*, *"a week passes with no human intervention in the loop"*. You lose the elegance of the fusion analogy and gain a roadmap you can be wrong about.

I would take (b). The fusion metaphor is genuinely good branding and terrible instrumentation, and you can keep the name without keeping the number.

### 1.2 The product identity is the unbuilt part

README's first sentence sells "multi-agent systems." `VISION.md` makes graph-based composition a core principle. Design Principle 5 states *"the graph is the substrate for composition."*

ADR-0007's own Context section says it plainly: *"factor-q's identity is a multi-agent runtime, but multi-agent is unbuilt: there is no agent-to-agent handoff primitive and no graph executor (ADR-0012's format is a spec without an engine)."*

I confirmed this by inspection. There is no graph executor, no `AgentSpawn`/`AgentMap`/`AgentLoop`, no fan-in. The only `fan_out` in the tree is a dispatcher *test helper* for concurrent invocations, which is a different thing. Meanwhile: full MCP client (2,191 lines), CAS with FastCDC and BLAKE3, verified lock-free online GC, event-sourced grants with biscuit tokens and delegation/revocation, a read-only dashboard, a tarpc read service, an authenticated TLS edge, a Go cron adapter, a Go GitHub watcher.

**Every one of those is well-built. Several of them are not yet needed.**

The pattern is legible and it is the classic infrastructure trap: substrate work is tractable, has clear correctness criteria, produces satisfying verification artefacts, and never runs out. Orchestration is ill-defined, has no oracle, and is where the thesis actually lives. Velocity flows to where the gradient is clearest, which is not where the value is.

Two things sharpen this:

- **The storage/vector foundation is the current active plan** (M3 extraction → M4 embedding → M5 service wiring), and it gates Memory *and* Skills. That is three more substrate layers ahead of orchestration. On current sequencing, the defining feature is at least two plan-cycles out.
- **Principle 6 ("the simplest thing that works, behind a verified, swappable seam") argues against your own sequencing here.** The principle says build the end-to-end experience on reference implementations first and deepen later. Applied honestly to orchestration, that means: a two-node hardcoded handoff, no graph format, no fragment library, shipped this month — then swap in the real executor behind the seam. The CAS got a FastCDC content-defined chunking implementation before the graph got a `spawn` call.

**Recommendation.** Time-box a deliberately crude orchestration spike — one agent invoking another, results returned, budget inherited, no graph format, no YAML, no fragment library — and put it ahead of M3. Not because the crude version is good, but because until something end-to-end exists you have no evidence about what the graph layer actually needs, and ADR-0007's design is currently unvalidated by contact with a running system. If the spike is ugly, that is Principle 6 working as intended.

### 1.3 The dogfood loop is your strongest asset and your largest evidential bias

The autonomous loop is real and it works — 53 agent-authored commits in the history, PRs landing behind a human merge gate. That is a genuine achievement and most projects claiming this cannot show it.

But it is evidence about exactly one workload: **a Rust monorepo with a fast, deterministic, machine-checkable oracle (`just ci`).** That oracle is doing enormous quiet work. It gives every agent run a ground-truth pass/fail signal in minutes, which is what makes unattended iteration viable at all.

Two of your three stated target use cases have no such oracle:

- **Regulatory document analysis** — correctness is contested, slow to verify, and often requires a domain expert. There is no `just ci`.
- **Automated systems operations** — the oracle is production, and being wrong is expensive and sometimes irreversible.

There is currently zero evidence for either. The harness has been tuned for months against the one workload that is structurally easiest, and the tuning is invisible — it lives in prompt shapes, error-message design, tool ergonomics, and the `${workspace}` model, all of which quietly assume "there is a repo and a test command."

**Recommendation.** Before M1, run one non-code workload end to end. The `doc-drift` agent is close but still code-adjacent. Something like: ingest a regulatory corpus, answer a fixed question set, have a human score it. The point is not the output — it is to discover which of your primitives are actually general and which are `just ci` in disguise. My prediction is that context management and the absence of a verification oracle both bite hard, and both are currently unscheduled.

### 1.4 BSL is buying protection you don't need at a price you can't afford

BSL 1.1, personal non-commercial use free, organisational use requires a commercial licence via `licensing@factorq.top`, four-year Apache conversion.

The trade as it stands: you are protecting commercial revenue that does not exist, from competitors who do not exist, in a project with no tagged release, by deterring precisely the contributors and early adopters an unfunded solo project most needs. A `licensing@` address on a custom domain implies a commercial entity behind it and raises diligence questions for any org that might otherwise trial it.

I am not saying BSL is wrong — the Sentry/HashiCorp/MariaDB reasoning is real and the four-year conversion is the honest version of it. I am saying the cost is being paid now and the benefit is contingent on a future that a licence choice does not by itself create. Your own Working Backwards exercise identified the *workflow optimisation service* as the strongest near-term wedge — that is a services business, and services businesses are not protected by source licences.

**Recommendation.** Write down the specific competitive scenario BSL prevents. If you can name it concretely, keep BSL. If the honest answer is "someone might one day fork it and sell hosting," note that this requires the project to first be successful enough to be worth forking, and that Apache-2.0 until that point is a cheap option to hold.

### 1.5 What the requirements layer gets right

Worth stating explicitly because it's unusual: `design-principles.md` is genuinely excellent. Principle 3's distinction between *restriction* and *construction* — with the concrete worked example of the phase-1 shell that had a path allow-list on the file tool but not on the subprocess it could spawn — is a better articulation of capability security than most security engineering writing. Principle 2 ("no confabulation where data exists") is a real insight about LLM-as-user that I have not seen stated this clearly elsewhere. Principle 8's test ("if you would ever run the system with a different value to see what happens, it is configuration") is exactly right and is broadly honoured in the config surface.

The gap is not the principles. It is that principles are only load-bearing if violations get caught — see Part 2 and finding 3.3.

---

## Part 2 — The review loop is losing to velocity

This is the finding I would act on first, because it is the one that makes every other finding recur.

The 14 July review named three god-files as the single largest structural debt, described the refactors as **"mechanical"** because internal section banners already mark the split lines, and gave specific target module layouts.

Ten days later:

| File | 14 Jul | 25 Jul | Δ |
|---|---:|---:|---:|
| `worker/reducer/runner.rs` | 6,351 | 7,387 | **+1,036 (+16%)** |
| `fq-cli/src/lib.rs` (was `main.rs`) | 4,411 | 5,877 | **+1,466 (+33%)** |
| `mcp.rs` | 2,088 | 2,191 | +103 (+5%) |

Total change in that window: **37,809 insertions, 12,740 deletions, 320 files.**

Every one of the three grew. Meanwhile the review's *tactical* recommendations largely landed — the single workspace shipped (#194), `fq-test-support` was extracted, the SECURITY.md/NATS-auth doc contradiction was fixed, `build_invocation_setup` exists now (the resume-path duplication fix). So the loop is not ignoring reviews. It is **selectively executing the parts that decompose into discrete issues, and structurally unable to execute the parts that don't.**

That is a predictable property of the system you have built, and it is worth naming as such rather than as a lapse:

- Issue-shaped work is what the `github-watcher` → `m0-issue-fix` loop consumes. "Split runner.rs into five modules" is a poor issue: large diff, high conflict surface against every in-flight change, no local test that proves it worked, and it competes with feature work for the same merge gate.
- LLM agents editing a file overwhelmingly **append to existing structures** rather than restructure. Every agent-authored commit tends to make the god-files slightly more god-like. This is not a factor-q flaw — it is a property of the medium — but the dogfood loop means it now applies to your codebase at machine speed.
- The result: **the throughput you gained applies to features and small fixes, and structural debt now accrues faster than the one mechanism that could pay it down.**

Concretely, `runner.rs` now holds the host loop, resume/replay, MCP sampling, elicitation, elicitation schema validation, and the tool dispatch path, with five functions over 250 lines (`run_loop_inner` 368, `resume` 364, `run_tool` 322, `dispatch_llm` 285, `run_loop_for` 268) and 14 `#[allow(clippy::too_many_arguments)]` across the tree. `resume`'s correctness depends on staying observationally equivalent to the fresh path — the highest-stakes invariant in the system — and it lives in the file that is growing fastest.

**Recommendations, in order:**

1. **Make it a gate, not a preference.** Add a CI check with a per-file line budget, ratcheted down (fail if a file over N lines grows). It is crude and it is the only thing that reliably works, because it converts "should refactor" into "cannot merge." Set the initial budget at current size so nothing breaks, then ratchet.
2. **Do the `runner.rs` split by hand, this week, as one PR, before the next feature.** Agents cannot do this one. The banners are already there; the review already specified the target layout.
3. **Treat structural work as a distinct queue with its own budget** — e.g. one structural PR merged per N feature PRs. Otherwise it always loses, because feature work always has a requester and refactors never do.
4. **Add a "restructure, don't append" instruction to the fleet agents' prompts** and see whether it moves the needle. It probably won't much, but it's cheap, and the delta is useful information about what the loop can and can't be asked to do.

---

## Part 3 — Security findings

`SECURITY.md` is honest and I want to credit that: it names the unenforced `sandbox.env`/`sandbox.network`, the committed NATS token, the unauthenticated `fq-cas serve`, and the shared `GH_TOKEN`. That is more candour than most alpha projects manage. The findings below are ones it does **not** cover.

### 3.1 Sandbox escape: dangling symlink defeats `check_write` — **PoC, high**

**Location:** `fq-tools/src/sandbox.rs:289` (`canonicalise_for_write`), reached from `check_write` at `:203`, consumed by `builtin/file_write.rs:69`.

**The bug.** `canonicalise_for_write` branches on `target.exists()`. In Rust, `Path::exists()` calls `fs::metadata()`, which **follows symlinks** — so a symlink pointing at a path that does not yet exist reports `false`. Control therefore falls to the else-branch, which canonicalises the *parent* and **lexically joins** the filename:

```rust
let canonical_parent = std::fs::canonicalize(parent)?;
Ok(canonical_parent.join(filename))   // no symlink resolution on the final component
```

The result is `<workspace>/notes.txt`, which sits inside the allowed prefix, so `check_within` approves it. `file_write` then calls `tokio::fs::write` on the *approved* path, which opens with `O_CREAT|O_WRONLY|O_TRUNC` and **follows the symlink**, landing the bytes outside the sandbox.

**Reproduced** with a faithful re-implementation of the check (POSIX semantics are identical between Rust `std::fs` and the emulation):

```text
allowed write prefix : /tmp/poc/scratch/workspace
agent creates symlink: .../workspace/notes.txt -> .../secrets/authorized_keys
os.path.exists(link) : False   <-- dangling, so False
SANDBOX VERDICT      : ALLOWED -> /tmp/poc/scratch/workspace/notes.txt
victim contents      : PWNED
>>> ESCAPE CONFIRMED: bytes landed at .../secrets/authorized_keys, outside the sandbox <<<
```

**Why the tests miss it.** Both relevant tests create the target first:

- `sandbox.rs:589` — `write_existing_symlink_pointing_outside_is_denied` (the name states the assumption)
- `file_write.rs:225` — `rejects_symlink_escape`, which does `fs::write(&target, "old")` before symlinking

Every symlink test in the tree points at an **existing** target. The dangling case is the one that isn't covered, and it's the one that's exploitable. `check_read` is not affected — it requires the path to exist, so `canonicalize` resolves it and the check fails correctly.

**Fix.** Do not trust a lexical join on the final component. Either:

- `symlink_metadata()` on the target and reject if it's a symlink (cheap, closes this case, still TOCTOU-racy — but no worse than today); **or**
- open the parent with `openat`, then `openat(parent_fd, filename, O_CREAT|O_EXCL|O_NOFOLLOW)`, which closes the class rather than the instance and removes the TOCTOU window at the same time. This is the Principle 3 answer: construct so the check is unnecessary.

Add a `write_dangling_symlink_pointing_outside_is_denied` test alongside the existing one.

### 3.2 The `env` allowlist provides no confidentiality — **PoC, high in context**

**Location:** `builtin/exec.rs:321` (`env_clear()` + baseline + allowlist), documented at `:48–54`.

The module doc presents this as a safeguard: *"the child does NOT inherit the parent's environment... An agent that doesn't list `HOME` in its sandbox will have no `HOME` set in the child."* The **hygiene** claim is true. The **confidentiality** claim it implies is not.

The child runs as the same uid as `fqd` and is its direct descendant, so it can read the daemon's environment straight out of `/proc`:

```text
child's own env     : ['LC_CTYPE', 'PATH']
secret in child env : False
read /proc/762/environ -> ['FQ_NATS_TOKEN=fq-dev-token',
                           'ANTHROPIC_API_KEY=sk-ant-SUPER-SECRET']
```

Yama `ptrace_scope=1` does not help — it permits tracing descendants, and the exec'd child is one by construction.

This matters because `ops/dogfood/env.example` puts `ANTHROPIC_API_KEY` and `GH_TOKEN` into exactly that environment, and `exec.rs`'s "Known gaps" section lists PATH, network, cgroups and seccomp but **not credential exfiltration via `/proc`** — so a reader of that section would reasonably conclude secrets are contained.

**Fix, cheapest first.**

1. `prctl(PR_SET_DUMPABLE, 0)` on the daemon before spawning children. Makes `/proc/<pid>/environ` root-only. One line, closes the direct read.
2. Better: don't hold provider keys in the daemon's environment at all — read them from a file at call time, or from a credential process. The daemon already has a config-driven `api_key_env` indirection; extending it to `api_key_file` is small.
3. Update the "Known gaps" list either way, since (1) does not stop a child from finding secrets by other means once it has arbitrary exec.

### 3.3 The dogfood loop's only enforced control is the human merge gate — **architectural, high**

This is the finding I'd most want you to sit with, because it's the point where the operating reality has diverged furthest from the stated model.

`ops/dogfood/agents/backlog-groomer.md` runs weekly, unattended:

```yaml
tools:   [builtin__exec]
sandbox:
  exec_cwd: [${workspace}]
  network:  [github.com, api.github.com]
budget: 12.00
max_iterations: 150
```

Trace the actual capability:

- **`network:` is decorative.** `exec.rs:74` states it outright: *"An agent definition's `sandbox.network` allowlist is parsed but never consulted here, so declaring it restricts nothing."* The runtime honestly reports it as `unenforced_network()`. The agent has unrestricted egress.
- **The prompt instructs the agent to use a shell.** *"Anything needing a pipeline goes through `bash -c` sparingly."* `exec.rs`'s header says the tool is *"Named `exec` (not `shell`) on purpose... there is no opportunity for shell injection."* That property is real in the tool and dissolved in the deployment: `bash` is on the pinned `PATH`, and the agent is told to reach for it.
- **It holds the owner's write-scoped `GH_TOKEN`**, and per 3.2 can also recover `ANTHROPIC_API_KEY`.
- **Its restrictions are natural-language.** The "Hard rules" section — *"Mutating operations are `gh issue edit/close/comment` only. You do not push code, open PRs..."* — is a prompt. Nothing in the typed-operation layer prevents `gh pr merge`, `git push`, or `curl`.
- **Its input is attacker-controlled.** Step 3: `gh issue view N --json title,body,labels` across all open issues. Any GitHub user can file an issue. `SECURITY.md` explicitly invites vulnerability reports *as normal GitHub issues* — so the untrusted-input channel is one you actively advertise.
- **Its output feeds a second agent.** The prompt states the premise: *"The tracker is the fleet's spec source: fleet agents implement what issue bodies say."* The groomer rewrites issue bodies and applies `fleet:refined`; `m0-issue-fix` then implements what those bodies say.

So the chain is: **public issue text → privileged agent constrained only by prompt → rewritten spec → implementing agent → PR → human merge gate.**

The `status:ready` label gate protects the *m0-issue-fix* intake (labels need write access). It does not protect the groomer, which reads everything by design.

**This inverts Principle 3 and ADR-0016 in the project's own flagship deployment.** Principle 3: *"An allow-list defaulting to nothing fails safe... a deny-list defaulting to everything fails open."* Principle 3, again: *"Restriction hands the agent broad, ambient authority — a real shell, a real filesystem, raw sockets — and adds checks to contain it."* That is a precise description of the groomer. ADR-0016 mandates typed narrow operations over free-form APIs; the groomer's entire interface is free-form `bash`. And Principle 5 warns that *"natural-language prompts that smuggle workflow control flow are a sign the graph layer is being used wrong"* — the "Hard rules" section is exactly that, doing security work rather than control-flow work.

I don't think this happened through carelessness. It happened because **the constructed primitives the principles demand do not exist yet**, so the loop was built out of the one broad primitive that does. That is a reasonable interim call. What's missing is that it isn't written down as one — `SECURITY.md` doesn't mention it, and the principles document reads as though construction is the operating norm.

**Recommendations:**

1. **Write the residual down.** Add to `SECURITY.md`: the fleet agents run with an unenforced network declaration, shell access via `bash -c`, ambient owner credentials, and prompt-level-only restrictions; the human merge gate is the sole enforced control; agents ingest untrusted public text. Right now a reader of `design-principles.md` would form the opposite impression.
2. **Split the groomer's authority from its reading.** Have it emit a proposed-changes document; a separate, tiny, typed applier makes the `gh` calls. That is one narrow typed operation and it removes the whole free-form path — a concrete instance of Principle 3 that's small enough to actually build.
3. **Give it its own credential.** A fine-grained PAT scoped to issues-only on one repo, distinct from the token `m0-issue-fix` uses to push. Removes the shared-blast-radius problem regardless of everything else.
4. **Prioritise `sandbox.network` enforcement (#208/#209).** For an agent handling untrusted input with a live API token, egress control is the highest-value single control available, and it's the one currently declared-but-absent.

### 3.4 Edge tokens have no expiry, no revocation, no audit — **medium-high**

**Location:** `fq-edge/src/auth.rs:67` (`mint_token`), `:207` (`attenuate`), `:273` (`verify_token`).

`mint_token` writes exactly two fact types — `principal` and `grant`:

```rust
builder.fact(fact("principal", &[string(principal)]))?;
for (verb, domain) in grants { builder.fact(fact("grant", ...))?; }
```

No expiry check (`check if time($t), $t < ...`), no token id, no nonce. `verify_token` authorises with `allow if true;` and then evaluates grants. Consequently:

- **The admin token printed at first run is a perpetual, all-authority bearer credential.** `mint_admin_token` grants `("*", "*")`.
- **There is no revocation path.** The only way to invalidate a token is to rotate the biscuit root — which lives alongside the TLS cert in `EdgeIdentity`, so rotating it forces every client to re-pin the fingerprint. Revoking one leaked token means re-onboarding every client.
- **Attenuated tokens are invisible.** `attenuate` is offline by design (the doc calls this out as a feature, and it is one). The corollary is that the daemon has no record of which derived tokens exist. There is no way to answer "what credentials are outstanding."

Biscuit supports all three (expiry facts, revocation ids, a revocation list) and you are already using its datalog properly elsewhere — `validate_grant_segment` guarding the spliced attenuation source is a genuinely good catch that many implementations miss.

**Fix.** Add an expiry fact and a random revocation id at mint; have `verify_token` check both, with the revocation list read from the control-plane store. Ship a default TTL (days for admin, hours for scoped) with the value in `fq.toml` per Principle 8.

Minor, same file: fingerprint comparison at `client.rs:59` is `==` on `[u8; 32]`, not constant-time. Low exploitability for a client-side check of a value the attacker would have to be the server to influence, but there's no cost to `subtle::ConstantTimeEq` and it removes the question.

### 3.5 `install.sh` fails open on checksum verification — **medium, one-line fix**

**Location:** `install.sh:72`.

```sh
if curl -fsSL "${url}.sha256" -o "$tmp/bundle.sha256" 2>/dev/null; then
    ... verify ...
fi
# falls through and installs unverified if the .sha256 fetch failed
```

Verification is conditional on the `.sha256` fetch succeeding. Anyone who can cause that one request to fail — a MITM, a CDN blip, a 404 from a mispublished release — gets an unverified install, silently. The README advertises this script as `curl | sh`.

This is the fail-open pattern Principle 3 explicitly rules out, and it's inconsistent with your own better practice: `.nats-checksums` states *"`just install-nats` refuses a download that doesn't match, so a tampered or corrupted release asset can never be executed."* Same author, same repo, opposite posture.

**Fix.** Fail hard if the `.sha256` cannot be fetched. Until the first release ships, this costs nothing to correct.

### 3.6 No dependency vulnerability scanning; 510 crates — **medium**

No `cargo audit`, no `cargo deny`, no Dependabot config, no CodeQL. 510 entries in `Cargo.lock`, plus a Go toolchain across two adapters.

The exposure is concrete rather than theoretical: `main-artifacts.yml` builds static musl binaries on every merge to `main` and publishes them to a rolling `main-latest` pre-release, which `deploy.sh` pulls onto the dogfood host — a machine holding a GitHub token with write access to this repo. A compromised transitive dependency reaches that host on the next merge.

**Fix.** `cargo audit` and `cargo deny` as CI jobs (an hour of work), plus `.github/dependabot.yml` for cargo, gomod and actions. Given `just ci` already aims to be isomorphic with CI, add them as `just` targets so they run locally too.

Related: `services/fq-dashboard/assets/datastar.js` is vendored with a `// Datastar v1.0.0` comment and no checksum, no SRI, no provenance record. You pin `nats-server` by SHA256 in a dedicated file — apply the same standard to the 34KB of third-party JavaScript you serve from the binary.

### 3.7 Budget enforcement rests on unpinned third-party data — **medium**

**Location:** `pricing.rs:34`.

```rust
pub const LITELLM_PRICING_URL: &str =
    "https://raw.githubusercontent.com/BerriAI/litellm/main/model_prices_and_context_window.json";
```

Principle 4 makes cost *"a first-order safety concern... co-equal with destroying data, leaking credentials."* The ground truth for that control is fetched at startup from an unpinned file on the `main` branch of a third-party repository, with no checksum, no signature, and no sanity bounds.

The failure mode is specific and quiet: an entry whose price is `0` (upstream error, or upstream compromise) passes the `enforce_pricing` guard — `lookup()` returns `Some` — and then every call costs `$0.00`, so the budget ceiling never trips and the agent runs to `max_iterations` at full spend. The `enforce_pricing` backstop at `runner.rs:2663` catches *missing* pricing, which is the right guard and a genuinely good piece of defence-in-depth. It does not catch *wrong* pricing.

Credit where due: the surrounding design is careful — the startup pricing guarantee fails the daemon fast rather than at invocation time, the config supports per-model overrides, and the docs explain exactly why an unpriced model is refused.

**Fix.** Pin to a tag or commit rather than `main`; record a checksum of the cached copy; add a sanity floor (reject a non-zero-token model priced at exactly zero) and optionally an order-of-magnitude drift warning against the previous cache. All cheap; the first two are the same pattern as `.nats-checksums`.

### 3.8 The four sandbox dimensions are not independent

Worth stating explicitly somewhere user-facing: `fs_read`, `fs_write`, `exec_cwd` and `env` read like four orthogonal grants, and the docs present them that way. They aren't. **`exec_cwd` dominates all of them.** An agent granted exec can `cat` any readable file (bypassing `fs_read`), write any writable path (bypassing `fs_write`), read the daemon's secrets via `/proc` (bypassing `env`, per 3.2), and reach any host (there being no network enforcement at all).

So the practical grant lattice has two levels, not four: *exec* and *not-exec*. An agent definition that grants `exec_cwd` alongside a tight `fs_write` is expressing an intent the runtime cannot honour. That deserves a line in the agent-authoring guide and in `SECURITY.md`, and arguably a warning at load time in the same style as `unenforced_network()`.

---

## Part 4 — Correctness and design

### 4.1 `SCHEMA_VERSION` is write-only, and the projection drops what it can't parse — **medium-high**

`events.rs:27` defines `SCHEMA_VERSION: u32 = 2` and stamps it into every envelope. I traced every consumer: **nothing on any read path ever inspects it.** The only occurrences outside `events.rs` are test fixtures constructing `schema_version: 1` literals.

Meanwhile `control_plane/projection/consumer.rs:12` documents the poison-message policy: *"Parse errors are logged and acked. An event whose JSON we can't parse... un-acked would just create a redelivery loop."* Acking is the right call for a genuine poison message. It is the wrong call for a systematic version mismatch.

Compose the two against ADR-0026 (event log as system of record) and the stated rebuild property (*"the projection is rebuildable by deleting the file and replaying from the stream"*):

> Ship a schema v3 whose change isn't serde-compatible. Delete `projection.db`. Replay the 30-day stream. Every v2 event fails to parse, is logged, and is acked. The projection rebuilds **successfully and silently incomplete.** `fq events query`, `fq costs`, and the dashboard now report against a truncated history, with no error surfaced anywhere.

That is a silent-data-loss path in the one property the event-sourcing architecture exists to provide.

The contrast is sharp and instructive: your **SQLite** stores get this right. `control_plane/store.rs:294` reads a recorded schema version, compares it against `CONTROL_PLANE_SCHEMA_VERSION`, and has explicit compatibility handling. The event log — the actual system of record — has the version field and none of the machinery.

**Fix.**

1. Dispatch on `schema_version` at the deserialization boundary; treat "version I don't know" as a distinct, loud error from "malformed JSON."
2. Never silently ack a version mismatch. Halt the consumer with a diagnosable error, or route to an explicit dead-letter with an operator-visible count. `fq doctor` should report it.
3. Add a golden corpus: serialised v1 and v2 events checked into the repo, with a CI test that replays them through the current projection. Right now a breaking schema change passes CI, because every test constructs events with the current version.

This one is cheap relative to its blast radius, and the golden-corpus test is the sort of thing your verification culture is already excellent at — it's just missing here.

### 4.2 Ack-after-durable-start is well-reasoned; the residual is known

`dispatcher.rs` has an unusually careful ack policy, with the 2026-07-06 redelivery-storm incident documented inline and the reasoning for ack-after-durable-start (rather than ack-on-dispatch or ack-on-completion) spelled out across three failure windows. The escalating NAK backoff and `max_ack_pending` sizing are both thought through. This is good work.

The residual — a crash in the window between the WAL write and the ack producing both a WAL recovery and a JetStream redelivery — is correctly identified and has an active plan (`2026-07-18-exactly-once-trigger-dispatch.md`, ADR-0032 draft). No new finding; I flag it only to say I looked and agree with the framing.

### 4.3 Structural notes on the execution path

Beyond raw size (Part 2), the shape of `runner.rs` concerns me for one specific reason: `resume` (364 lines) must remain observationally equivalent to the fresh path. The 14 July review flagged that this equivalence rested on hand-copied code; `build_invocation_setup` has since been extracted, which addresses the direct duplication. But both paths still live in the fastest-growing file in the repo, and the equivalence is enforced by tests rather than by construction.

Given Principle 6's emphasis on verified seams, this is a candidate for making structural: a single `InvocationSetup` type that both paths must construct, with the reducer unable to observe which path produced it. That converts a tested property into a type-level one. Worth considering as part of the split rather than after it.

---

## Part 5 — Code quality

Brief, because it is largely excellent and the prior review covered the structural side well.

**Measured strengths.**

| Signal | Value | Note |
|---|---|---|
| Test:production LOC | 41,840 : 41,198 (1.02) | Rare at this scale |
| `TODO`/`FIXME`/`HACK`/`XXX` | **0** | Across 82,901 lines |
| `#[allow(...)]` attributes | 17 | 14 are `too_many_arguments` |
| `unsafe` in production | 1 | `libc::signal(SIGPIPE)`; rest is test scaffolding |
| Panic sites in production | ~90 in 41k LOC | Concentrated in `runner.rs` (18) |

Beyond the numbers: module headers cite the ADR or the specific incident that motivated the code (`exec.rs`'s drain-grace rationale citing #176 is a good example — it explains a non-obvious design choice by naming the failure that forced it). Adapter types are confined to single modules. Newtypes validated at serde boundaries. `thiserror` in libraries, `anyhow` in binaries, consistently. The `fq.toml` template is one of the better-documented config surfaces I've read — the comment explaining *why* `max_concurrent_invocations` defaults to 1 and what must hold before raising it is the kind of thing that prevents an outage.

**Remaining issues**, all previously flagged and mostly still open:

- The three god-files, now larger (Part 2).
- Five functions over 250 lines in `runner.rs`; 14 `too_many_arguments` allows.
- Blocking `std::fs` in async paths (workspace provisioning, `discovery.rs`'s glob walk) — flagged 14 July, worth confirming whether it landed.
- `unsafe { env::set_var }` in parallel tests (`genai.rs:626,774,992`, `config.rs:1073`) — a genuine data race that Rust 2024 made `unsafe` precisely to surface. Test-only, but it can produce flakes that cost more to diagnose than the fix costs.

**A note on the test suite.** The verification architecture is the best thing in this repo — trace oracle, seeded DST with printed seeds, differential models, a conformance suite run both in-process and over the wire. Seven real bugs found by the reducer-verification work is a strong result and exactly the argument for the investment. The gap I'd name is **coverage shape rather than coverage depth**: the symlink tests all assume existing targets (3.1), the schema tests all construct current-version events (4.1). Both blind spots have the same signature — the suite thoroughly explores the state space it imagined and does not probe the boundary of that imagination. Property-based generation over path shapes (dangling links, `..` in final position, non-UTF-8 components) and over schema versions would likely find more of the same class.

---

## What I'd do first

Ordered by (impact × tractability), not by severity alone:

1. **Fix the dangling-symlink write escape** (3.1). Half a day including the `openat` version and the missing test. It's a verified escape in the security primitive the whole isolation story rests on.
2. **Make `install.sh` fail closed** (3.5). One line. Do it before the first release, after which the exposure becomes real.
3. **Add `cargo audit` / `cargo deny` / Dependabot** (3.6). An hour. Your auto-deploy path makes this materially more urgent than the raw dependency count suggests.
4. **`prctl(PR_SET_DUMPABLE, 0)` and correct the `exec.rs` known-gaps list** (3.2). Small, and it removes a claim that currently misleads.
5. **Split `runner.rs` by hand, as one PR, ahead of the next feature** (Part 2). This is the one an agent cannot do for you, and it gets harder every week.
6. **Add the file-size CI ratchet** (Part 2). Crude, and the only thing that reliably converts intent into outcome.
7. **Fix the schema-version read path and add a golden event corpus** (4.1). Silent lossy rebuild contradicts the system-of-record property that justifies the whole event-sourcing design.
8. **Write the dogfood security residual into `SECURITY.md`, and give the groomer its own scoped token** (3.3, items 1 and 3). Cheap. The typed-applier split (item 2) is the real fix but is a project.
9. **Add expiry and revocation to edge tokens** (3.4). Half a day; the machinery is already in biscuit.
10. **Decide the Q question** (1.1). Either build the instrument or replace the ladder. Do this before M1 planning, not during.
11. **Time-box a crude orchestration spike ahead of M3** (1.2). The highest-value item on this list and the one most likely to be deferred, because it's the only one without a clear finish line. That asymmetry is exactly the problem it addresses.

---

### Closing

The most useful thing I can tell you is that **the code is not your bottleneck.** The quality bar here is high enough that continuing to raise it has diminishing returns, and the two things that will actually determine whether factor-q succeeds are both outside the code: whether the Q ladder becomes measurable, and whether orchestration gets built before the substrate absorbs another two quarters.

The security findings are real and several are genuinely serious, but they are all tractable in days. The strategic findings are not tractable in days, which is precisely why they're the ones worth arguing about.

One thing I'd underline: the process finding in Part 2 is not a criticism of discipline — your discipline is visibly excellent. It's a structural property of building a system whose primary output is issue-shaped work. The loop you've built is very good at what it does, and structural refactoring is categorically not what it does. That constraint will not relax as the loop improves; if anything it tightens, because throughput on issue-shaped work rises while the appetite for large disruptive diffs falls. Designing around it now is much cheaper than discovering it at 150k lines.
