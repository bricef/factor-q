package main

import (
	"strings"
	"testing"
	"time"
)

func TestVerdictTakesTheMostRestrictiveTier(t *testing.T) {
	policy, areas := testPolicy(t), testAreas(t)
	cases := []struct {
		name     string
		files    []string
		wantTier Tier
		wantRule string // a substring of the explanation
	}{
		{"docs only", []string{"docs/a.md", "README.md"}, TierUnsupervised, "`README.md` decides"},
		{"adapter only", []string{"adapters/fq-cron/main.go"}, TierUnsupervised, "area `adapters` is `unsupervised`"},
		{"mixed tiers: one ops file decides the whole PR",
			[]string{"docs/a.md", "adapters/fq-cron/main.go", "ops/dogfood/compose.yml"},
			TierNever, "`ops/dogfood/compose.yml` decides"},
		{"mixed tiers: services beats docs",
			[]string{"docs/a.md", "services/fq-store/src/lib.rs"},
			TierSupervised, "area `services` is `supervised`"},
		{"a file in two mapped areas takes the stricter one",
			[]string{"services/fq-runtime/crates/fq-tools/src/sandbox.rs"},
			TierNever, "area `sandbox` is `never`"},
		{"unmapped path falls to the default tier",
			[]string{"Cargo.lock"},
			TierSupervised, "matches no area the policy maps"},
		{"the policy file itself is never",
			[]string{"docs/a.md", ".github/merge-policy.yml"},
			TierNever, "area `merge-verdicts` is `never`"},
		{"the sweep's own source is never",
			[]string{"adapters/github-watcher/verdict.go"},
			TierNever, "area `merge-verdicts` is `never`"},
		{"no changed files", nil, TierSupervised, "no changed files"},
	}
	for _, tc := range cases {
		t.Run(tc.name, func(t *testing.T) {
			got := Verdict(policy, areas, PRFacts{Files: tc.files})
			if got.Tier != tc.wantTier {
				t.Errorf("tier = %s, want %s (rule: %s)", got.Tier, tc.wantTier, got.Rule)
			}
			if !strings.Contains(got.Rule, tc.wantRule) {
				t.Errorf("rule = %q, want it to contain %q", got.Rule, tc.wantRule)
			}
			if len(got.Files) != len(tc.files) {
				t.Errorf("file table has %d rows, want %d", len(got.Files), len(tc.files))
			}
		})
	}
}

func TestVerdictFileTableIsSortedAndTiered(t *testing.T) {
	policy, areas := testPolicy(t), testAreas(t)
	got := Verdict(policy, areas, PRFacts{Files: []string{"ops/x.yml", "docs/a.md", "Cargo.lock"}})
	want := []struct {
		path string
		tier Tier
	}{
		{"Cargo.lock", TierSupervised},
		{"docs/a.md", TierUnsupervised},
		{"ops/x.yml", TierNever},
	}
	for i, w := range want {
		if got.Files[i].Path != w.path || got.Files[i].Tier != w.tier {
			t.Errorf("row %d = %s/%s, want %s/%s", i, got.Files[i].Path, got.Files[i].Tier, w.path, w.tier)
		}
	}
	if len(got.Files[0].Areas) != 0 {
		t.Errorf("Cargo.lock areas = %v, want none (default tier)", got.Files[0].Areas)
	}
}

// cleanFacts is a PR that passes every structural check, so each case
// below can break exactly one and see exactly one failure.
func cleanFacts() PRFacts {
	created := time.Date(2026, 9, 21, 10, 0, 0, 0, time.UTC)
	return PRFacts{
		Number:         900,
		HeadSHA:        "0123456789abcdef",
		BaseRef:        "main",
		Body:           "The change\n\n" + provenanceFooter("m0-issue-fix", "inv-test", 879, "status:ready", created),
		Files:          []string{"docs/a.md"},
		CommitCount:    1,
		MergeableState: "clean",
		CreatedAt:      created,
		ObservedAt:     created.Add(2 * time.Hour),
		ClosingIssues:  []ClosingIssue{{Number: 879, Labels: []string{"status:in-review"}}},
	}
}

func checkByName(t *testing.T, d Decision, name string) Check {
	t.Helper()
	for _, c := range d.Checks {
		if c.Name == name {
			return c
		}
	}
	t.Fatalf("no check named %q in %v", name, d.Checks)
	return Check{}
}

func TestStructuralChecks(t *testing.T) {
	policy, areas := testPolicy(t), testAreas(t)
	cases := []struct {
		name   string
		check  string
		break_ func(*PRFacts)
		want   string // a substring of the reason
	}{
		{"no provenance", CheckProvenance, func(f *PRFacts) { f.Body = "hand-written" }, "provenance form: none"},
		{"closes nothing", CheckClosingIssue, func(f *PRFacts) { f.ClosingIssues = nil }, "closes no issue"},
		{"closes two issues", CheckClosingIssue, func(f *PRFacts) {
			f.ClosingIssues = append(f.ClosingIssues, ClosingIssue{Number: 880})
		}, "closes 2 issues (#879, #880)"},
		{"issue is not in review", CheckClosingIssue, func(f *PRFacts) {
			f.ClosingIssues = []ClosingIssue{{Number: 879, Labels: []string{"status:in-progress"}}}
		}, `does not carry "status:in-review"`},
		{"dirty merge state", CheckMergeable, func(f *PRFacts) { f.MergeableState = "dirty" }, "mergeable_state is dirty"},
		{"unknown merge state", CheckMergeable, func(f *PRFacts) { f.MergeableState = "" }, "mergeable_state is unknown"},
		{"reworked", CheckSingleCommit, func(f *PRFacts) { f.CommitCount = 4 }, "4 commit(s)"},
		{"changes requested", CheckNoChangesReq, func(f *PRFacts) { f.ChangesRequest = true }, "a review requests changes"},
		{"held", CheckNoHoldLabel, func(f *PRFacts) { f.Labels = []string{"hold"} }, `"hold" label is present`},
		{"too young", CheckMinAge, func(f *PRFacts) { f.ObservedAt = f.CreatedAt.Add(time.Minute) }, "opened 1m0s ago (minimum 30m0s)"},
	}
	for _, tc := range cases {
		t.Run(tc.name, func(t *testing.T) {
			facts := cleanFacts()
			tc.break_(&facts)
			d := Verdict(policy, areas, facts)
			got := checkByName(t, d, tc.check)
			if got.Pass {
				t.Errorf("check %q passed, want it to fail", tc.check)
			}
			if !strings.Contains(got.Reason, tc.want) {
				t.Errorf("reason = %q, want it to contain %q", got.Reason, tc.want)
			}
			for _, other := range d.Checks {
				if other.Name != tc.check && !other.Pass {
					t.Errorf("check %q also failed (%s); the case should break exactly one", other.Name, other.Reason)
				}
			}
			// The tier is decided by files alone: a failing check never
			// moves it, because the sweep is advisory.
			if d.Tier != TierUnsupervised {
				t.Errorf("tier = %s, want unsupervised — checks must not change the tier", d.Tier)
			}
		})
	}
}

func TestStructuralChecksAllPassOnACleanPR(t *testing.T) {
	d := Verdict(testPolicy(t), testAreas(t), cleanFacts())
	if len(d.Checks) != len(CheckNames) {
		t.Fatalf("got %d checks, want %d", len(d.Checks), len(CheckNames))
	}
	for i, c := range d.Checks {
		if c.Name != CheckNames[i] {
			t.Errorf("check %d is %q, want %q — the order is the replay CSV's column order", i, c.Name, CheckNames[i])
		}
		if !c.Pass {
			t.Errorf("check %q failed on a clean PR: %s", c.Name, c.Reason)
		}
	}
}

func TestVerdictUsesTheWatchersLabelVocabulary(t *testing.T) {
	policy := testPolicy(t)
	policy.InReviewLabel, policy.HoldLabel = "review-me", "wait"
	facts := cleanFacts()
	facts.ClosingIssues = []ClosingIssue{{Number: 879, Labels: []string{"review-me"}}}
	facts.Labels = []string{"wait"}
	d := Verdict(policy, testAreas(t), facts)
	if c := checkByName(t, d, CheckClosingIssue); !c.Pass {
		t.Errorf("closing-issue check failed with a renamed label: %s", c.Reason)
	}
	if c := checkByName(t, d, CheckNoHoldLabel); c.Pass {
		t.Errorf("hold check passed with a renamed hold label present: %s", c.Reason)
	}
}

func TestProvenanceCheckReportsEveryForm(t *testing.T) {
	footer := provenanceFooter("footer-agent", "inv", 887, "status:ready", time.Now())
	line := "provenance: agent=line-agent invocation=inv"
	for _, tc := range []struct{ body, form string }{
		{line, "line"}, {footer, "footer"}, {line + "\n" + footer, "both"}, {"plain", "none"},
	} {
		d := Verdict(testPolicy(t), testAreas(t), PRFacts{Body: tc.body})
		check := checkByName(t, d, CheckProvenance)
		if !strings.Contains(check.Reason, "form: "+tc.form) {
			t.Errorf("body form %s reported as %q", tc.form, check.Reason)
		}
		if check.Pass != (tc.form != "none") {
			t.Errorf("body form %s pass = %v", tc.form, check.Pass)
		}
	}
}
