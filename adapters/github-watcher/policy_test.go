package main

import (
	"os"
	"path/filepath"
	"strings"
	"testing"
	"time"
)

const testAreasYAML = `
docs:
  - 'docs/**'
top-level-markdown:
  - '*.md'
adapters:
  - 'adapters/**'
ops:
  - 'ops/**'
sandbox:
  - 'services/fq-runtime/crates/fq-tools/**'
services:
  - 'services/**'
merge-verdicts:
  - '.github/merge-policy.yml'
  - 'adapters/github-watcher/verdict*.go'
go:
  - 'adapters/**'
  - 'justfile'
`

const testPolicyYAML = `
default_tier: supervised
min_age_minutes: 30
areas:
  docs: unsupervised
  top-level-markdown: unsupervised
  adapters: unsupervised
  services: supervised
  sandbox: never
  ops: never
  merge-verdicts: never
`

func testAreas(t *testing.T) Areas {
	t.Helper()
	areas, err := ParseAreas([]byte(testAreasYAML))
	if err != nil {
		t.Fatalf("ParseAreas: %v", err)
	}
	return areas
}

func testPolicy(t *testing.T) Policy {
	t.Helper()
	policy, err := ParsePolicy([]byte(testPolicyYAML), testAreas(t))
	if err != nil {
		t.Fatalf("ParsePolicy: %v", err)
	}
	return policy
}

func TestAreasMatchIsSortedAndGlobbed(t *testing.T) {
	areas := testAreas(t)
	cases := []struct {
		path string
		want []string
	}{
		{"docs/adrs/ADR-0036.md", []string{"docs"}},
		{"README.md", []string{"top-level-markdown"}},
		// `*` does not cross a separator, so nested markdown is not
		// top-level markdown.
		{"ops/dogfood/README.md", []string{"ops"}},
		{"adapters/github-watcher/watcher.go", []string{"adapters", "go"}},
		{"adapters/github-watcher/verdict.go", []string{"adapters", "go", "merge-verdicts"}},
		{"services/fq-runtime/crates/fq-tools/src/sandbox.rs", []string{"sandbox", "services"}},
		{"Cargo.lock", nil},
	}
	for _, tc := range cases {
		t.Run(tc.path, func(t *testing.T) {
			got := areas.Match(tc.path)
			if strings.Join(got, ",") != strings.Join(tc.want, ",") {
				t.Errorf("Match(%q) = %v, want %v", tc.path, got, tc.want)
			}
		})
	}
}

func TestParsePolicyRefusesVersionSkew(t *testing.T) {
	areas := testAreas(t)
	cases := []struct {
		name string
		yaml string
		want string
	}{
		{"unknown area", "default_tier: supervised\nmin_age_minutes: 30\nareas:\n  dcos: unsupervised\n", `area "dcos" is not declared in areas.yml`},
		{"unknown tier", "default_tier: supervised\nmin_age_minutes: 30\nareas:\n  docs: unsuperivsed\n", `unknown tier "unsuperivsed"`},
		{"unknown default tier", "default_tier: yolo\nmin_age_minutes: 30\nareas:\n  docs: unsupervised\n", "default_tier: unknown tier"},
		{"missing default tier", "min_age_minutes: 30\nareas:\n  docs: unsupervised\n", "default_tier is required"},
		{"missing min age", "default_tier: supervised\nareas:\n  docs: unsupervised\n", "min_age_minutes is required"},
		{"negative min age", "default_tier: supervised\nmin_age_minutes: -1\nareas:\n  docs: unsupervised\n", "min_age_minutes must be >= 0"},
		{"empty areas", "default_tier: supervised\nmin_age_minutes: 30\nareas: {}\n", "areas maps nothing"},
		{"not yaml", "default_tier: [\n", "parse merge-policy.yml"},
	}
	for _, tc := range cases {
		t.Run(tc.name, func(t *testing.T) {
			_, err := ParsePolicy([]byte(tc.yaml), areas)
			if err == nil {
				t.Fatalf("ParsePolicy(%q) = nil error, want a refusal", tc.name)
			}
			if !strings.Contains(err.Error(), tc.want) {
				t.Errorf("error = %v, want it to contain %q", err, tc.want)
			}
		})
	}
}

func TestParsePolicyReadsTheWholeFile(t *testing.T) {
	policy := testPolicy(t)
	if policy.DefaultTier != TierSupervised {
		t.Errorf("default tier = %s, want supervised", policy.DefaultTier)
	}
	if policy.MinAge != 30*time.Minute {
		t.Errorf("min age = %s, want 30m", policy.MinAge)
	}
	if got := policy.Areas["sandbox"]; got != TierNever {
		t.Errorf("sandbox tier = %s, want never", got)
	}
	// An area declared in areas.yml but unmapped stays unmapped — the CI
	// filters answer a different question and must not decide a tier.
	if _, mapped := policy.Areas["go"]; mapped {
		t.Error("the CI filter `go` must not be mapped by the merge policy")
	}
}

func TestParseAreasRefusesAnEmptyOrInvalidDeclaration(t *testing.T) {
	cases := map[string]string{
		"empty":       "",
		"no globs":    "docs: []\n",
		"bad glob":    "docs:\n  - 'docs/[a'\n",
		"wrong shape": "docs:\n  paths:\n    - 'docs/**'\n",
	}
	for name, yaml := range cases {
		t.Run(name, func(t *testing.T) {
			if _, err := ParseAreas([]byte(yaml)); err == nil {
				t.Fatal("ParseAreas = nil error, want a refusal")
			}
		})
	}
}

// The shipped declarations are gated here rather than only at runtime: a
// policy naming an area that a later edit to areas.yml renamed would
// otherwise be discovered by the sweep refusing to run in production.
func TestShippedPolicyLoadsAgainstShippedAreas(t *testing.T) {
	root := filepath.Join("..", "..")
	areasRaw, err := os.ReadFile(filepath.Join(root, AreasPath))
	if err != nil {
		t.Fatalf("read %s: %v", AreasPath, err)
	}
	policyRaw, err := os.ReadFile(filepath.Join(root, PolicyPath))
	if err != nil {
		t.Fatalf("read %s: %v", PolicyPath, err)
	}
	areas, err := ParseAreas(areasRaw)
	if err != nil {
		t.Fatalf("ParseAreas(%s): %v", AreasPath, err)
	}
	policy, err := ParsePolicy(policyRaw, areas)
	if err != nil {
		t.Fatalf("ParsePolicy(%s): %v", PolicyPath, err)
	}
	// The sweep's own inputs must be `never`, or a PR could widen its own
	// merge rights.
	for _, path := range []string{
		PolicyPath, AreasPath,
		"adapters/github-watcher/verdict.go",
		"adapters/github-watcher/policy.go",
		"adapters/github-watcher/mergeverdict.go",
		"adapters/github-watcher/mergeverdict_github.go",
	} {
		d := Verdict(policy, areas, PRFacts{Files: []string{path}})
		if d.Tier != TierNever {
			t.Errorf("%s is tier %s, want never (%s)", path, d.Tier, d.Rule)
		}
	}
}
