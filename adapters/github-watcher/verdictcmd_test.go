package main

import (
	"bytes"
	"encoding/json"
	"strings"
	"testing"
	"time"
)

func runStream(t *testing.T, lines ...string) []verdictResult {
	t.Helper()
	var out bytes.Buffer
	in := strings.NewReader(strings.Join(lines, "\n") + "\n")
	if err := streamVerdicts(in, &out, testPolicy(t), testAreas(t), Rubric{}, nil); err != nil {
		t.Fatalf("streamVerdicts: %v", err)
	}
	var results []verdictResult
	dec := json.NewDecoder(&out)
	for dec.More() {
		var r verdictResult
		if err := dec.Decode(&r); err != nil {
			t.Fatalf("decode result: %v", err)
		}
		results = append(results, r)
	}
	return results
}

func TestVerdictCmdIsJSONLinesInOrder(t *testing.T) {
	results := runStream(t,
		`{"number":1,"files":["docs/a.md"]}`,
		``,
		`{"number":2,"files":["docs/a.md","ops/x.yml"]}`,
		`   `,
		`{"number":3,"files":["Cargo.lock"]}`,
	)
	want := []struct {
		number int
		tier   string
	}{{1, "unsupervised"}, {2, "never"}, {3, "supervised"}}
	if len(results) != len(want) {
		t.Fatalf("got %d results, want %d — blank lines must be skipped, not answered", len(results), len(want))
	}
	for i, w := range want {
		if results[i].Number != w.number || results[i].Tier != w.tier {
			t.Errorf("result %d = #%d/%s, want #%d/%s", i, results[i].Number, results[i].Tier, w.number, w.tier)
		}
		if results[i].Rule == "" || len(results[i].Checks) != len(CheckNames) {
			t.Errorf("result %d is missing its rule or checks: %+v", i, results[i])
		}
	}
}

// One malformed PR in a replay of two hundred should cost one row.
func TestVerdictCmdReportsABadLineWithoutKillingTheBatch(t *testing.T) {
	results := runStream(t, `{"number":1,"files":["docs/a.md"]}`, `not json`, `{"number":3,"files":["ops/x.yml"]}`)
	if len(results) != 3 {
		t.Fatalf("got %d results, want 3", len(results))
	}
	if results[1].Error == "" {
		t.Error("the malformed line produced no error field")
	}
	if results[0].Tier != "unsupervised" || results[2].Tier != "never" {
		t.Errorf("the good lines were disturbed: %+v", results)
	}
}

// The seam's job is to be the same verdict, so a fact that changes a
// check live must change it here too.
func TestVerdictCmdCarriesEveryFactAcrossTheSeam(t *testing.T) {
	facts := cleanFacts()
	facts.CommitCount = 5
	facts.Labels = []string{"hold"}
	line, err := json.Marshal(facts)
	if err != nil {
		t.Fatal(err)
	}
	result := runStream(t, string(line))[0]
	if result.ProvenanceForm != "footer" || result.AgentID != "m0-issue-fix" {
		t.Errorf("provenance did not cross the seam: form=%q agent=%q", result.ProvenanceForm, result.AgentID)
	}
	byName := map[string]Check{}
	for _, c := range result.Checks {
		byName[c.Name] = c
	}
	if byName[CheckSingleCommit].Pass || !strings.Contains(byName[CheckSingleCommit].Reason, "5 commit(s)") {
		t.Errorf("single-commit crossed the seam wrong: %+v", byName[CheckSingleCommit])
	}
	if byName[CheckNoHoldLabel].Pass {
		t.Errorf("hold label did not cross the seam: %+v", byName[CheckNoHoldLabel])
	}
	if !byName[CheckMinAge].Pass {
		t.Errorf("observed_at did not cross the seam: %+v", byName[CheckMinAge])
	}
}

// A caller that omits observed_at gets an age of zero, which reads as a
// failed age check rather than as an accidental pass.
func TestVerdictCmdDefaultsObservedAtToCreation(t *testing.T) {
	facts := cleanFacts()
	facts.ObservedAt = time.Time{}
	line, err := json.Marshal(facts)
	if err != nil {
		t.Fatal(err)
	}
	for _, c := range runStream(t, string(line))[0].Checks {
		if c.Name == CheckMinAge && c.Pass {
			t.Errorf("min-age passed with no observed_at: %s", c.Reason)
		}
	}
}

func TestTierCrossesJSONAsItsName(t *testing.T) {
	encoded, err := json.Marshal(TierNever)
	if err != nil {
		t.Fatal(err)
	}
	if string(encoded) != `"never"` {
		t.Errorf("Tier marshalled as %s, want \"never\"", encoded)
	}
	var back Tier
	if err := json.Unmarshal([]byte(`"unsupervised"`), &back); err != nil || back != TierUnsupervised {
		t.Errorf("round trip = %v/%v, want unsupervised/nil", back, err)
	}
	if err := json.Unmarshal([]byte(`"yolo"`), &back); err == nil {
		t.Error("an unknown tier decoded without complaint")
	}
	if err := json.Unmarshal([]byte(`2`), &back); err == nil {
		t.Error("a tier ordinal decoded; the wire vocabulary is names only")
	}
}
