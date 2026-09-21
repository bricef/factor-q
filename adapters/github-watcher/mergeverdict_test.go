package main

import (
	"context"
	"errors"
	"fmt"
	"strings"
	"testing"
	"time"
)

// fakeVerdictSource is an in-memory MergeVerdictSource recording every
// write, so a test can assert both what the sweep decided and how many
// times it said so.
type fakeVerdictSource struct {
	prs       []PullRequest
	facts     map[int]PRFacts
	files     map[string][]byte // "<path>@<ref>" -> contents
	labels    []string
	prLabels  map[int][]string
	comments  map[int]*PRComment
	nextID    int64
	ops       []string
	listErr   error
	factsErr  error
	labelsErr error
}

func newFakeVerdictSource() *fakeVerdictSource {
	return &fakeVerdictSource{
		facts:    map[int]PRFacts{},
		files:    map[string][]byte{AreasPath + "@main": []byte(testAreasYAML), PolicyPath + "@main": []byte(testPolicyYAML)},
		labels:   append([]string{}, TierLabels...),
		prLabels: map[int][]string{},
		comments: map[int]*PRComment{},
		nextID:   1,
	}
}

func (f *fakeVerdictSource) ListOpenPullRequests(context.Context) ([]PullRequest, error) {
	f.ops = append(f.ops, "list")
	return f.prs, f.listErr
}

func (f *fakeVerdictSource) PRFactsFor(_ context.Context, pr PullRequest) (PRFacts, error) {
	f.ops = append(f.ops, fmt.Sprintf("facts:%d", pr.Number))
	if f.factsErr != nil {
		return PRFacts{}, f.factsErr
	}
	return f.facts[pr.Number], nil
}

func (f *fakeVerdictSource) RepoFileAtRef(_ context.Context, path, ref string) ([]byte, error) {
	f.ops = append(f.ops, "read:"+path+"@"+ref)
	data, ok := f.files[path+"@"+ref]
	if !ok {
		return nil, fmt.Errorf("no such file %s@%s", path, ref)
	}
	return data, nil
}

func (f *fakeVerdictSource) RepoLabels(context.Context) ([]string, error) {
	return f.labels, f.labelsErr
}

func (f *fakeVerdictSource) SetPRLabels(_ context.Context, pr int, add string, remove []string) error {
	f.ops = append(f.ops, fmt.Sprintf("label:%d:+%s:-%s", pr, add, strings.Join(remove, ",")))
	f.prLabels[pr] = []string{add}
	return nil
}

func (f *fakeVerdictSource) FindPRComment(_ context.Context, pr int, marker string) (PRComment, bool, error) {
	c, ok := f.comments[pr]
	if !ok || !strings.Contains(c.Body, marker) {
		return PRComment{}, false, nil
	}
	return *c, true, nil
}

func (f *fakeVerdictSource) CreatePRComment(_ context.Context, pr int, body string) error {
	f.ops = append(f.ops, fmt.Sprintf("comment-create:%d", pr))
	f.comments[pr] = &PRComment{ID: f.nextID, Body: body}
	f.nextID++
	return nil
}

func (f *fakeVerdictSource) UpdatePRComment(_ context.Context, id int64, body string) error {
	f.ops = append(f.ops, fmt.Sprintf("comment-update:%d", id))
	for _, c := range f.comments {
		if c.ID == id {
			c.Body = body
			return nil
		}
	}
	return errors.New("no such comment")
}

func (f *fakeVerdictSource) addPR(number int, headSHA string, fleet bool, files []string) {
	body := "The change"
	if fleet {
		body += "\n\n" + provenanceMarker + "\nprovenance"
	}
	f.prs = append(f.prs, PullRequest{Number: number, HeadSHA: headSHA, BaseRef: "main", Body: body})
	facts := cleanFacts()
	facts.Number, facts.HeadSHA, facts.Body, facts.Files = number, headSHA, body, files
	f.facts[number] = facts
}

func newTestSweeper(src MergeVerdictSource) *MergeVerdictSweeper {
	cfg := testConfig()
	cfg.InReviewLabel, cfg.HoldLabel = "status:in-review", "hold"
	s := NewMergeVerdictSweeper(src, cfg, discardLogger())
	s.Now = func() time.Time { return time.Date(2026, 9, 21, 12, 0, 0, 0, time.UTC) }
	return s
}

func (f *fakeVerdictSource) opsMatching(prefix string) []string {
	var out []string
	for _, op := range f.ops {
		if strings.HasPrefix(op, prefix) {
			out = append(out, op)
		}
	}
	return out
}

func TestSweepScoresFleetPRsOnly(t *testing.T) {
	src := newFakeVerdictSource()
	src.addPR(900, "aaa", true, []string{"docs/a.md"})
	src.addPR(901, "bbb", false, []string{"ops/x.yml"})
	newTestSweeper(src).Sweep(context.Background())

	if got := src.opsMatching("facts:"); len(got) != 1 || got[0] != "facts:900" {
		t.Errorf("facts fetched for %v, want only the fleet PR", got)
	}
	if got := src.prLabels[900]; len(got) != 1 || got[0] != "merge:unsupervised" {
		t.Errorf("#900 labels = %v, want [merge:unsupervised]", got)
	}
	if _, labelled := src.prLabels[901]; labelled {
		t.Error("#901 is not a fleet PR and must not be labelled")
	}
	if _, commented := src.comments[901]; commented {
		t.Error("#901 is not a fleet PR and must not be commented on")
	}
}

func TestSweepAppliesOneLabelAndClearsTheOthers(t *testing.T) {
	src := newFakeVerdictSource()
	src.addPR(900, "aaa", true, []string{"docs/a.md", "ops/dogfood/compose.yml"})
	newTestSweeper(src).Sweep(context.Background())

	want := "label:900:+merge:never:-merge:unsupervised,merge:supervised"
	if got := src.opsMatching("label:"); len(got) != 1 || got[0] != want {
		t.Errorf("label ops = %v, want [%s]", got, want)
	}
	body := src.comments[900].Body
	for _, want := range []string{
		mergeVerdictMarker,
		"merge:never",
		"`ops/dogfood/compose.yml` decides",
		"| `docs/a.md` |",
		"| `provenance` | ✅ |",
		"Nothing merges automatically",
	} {
		if !strings.Contains(body, want) {
			t.Errorf("comment does not contain %q:\n%s", want, body)
		}
	}
}

func TestSweepIsIdempotentPerHeadSHA(t *testing.T) {
	src := newFakeVerdictSource()
	src.addPR(900, "aaa", true, []string{"docs/a.md"})
	sweeper := newTestSweeper(src)
	ctx := context.Background()

	sweeper.Sweep(ctx)
	sweeper.Sweep(ctx)
	sweeper.Sweep(ctx)

	if got := len(src.opsMatching("facts:")); got != 1 {
		t.Errorf("facts fetched %d times, want 1 — an unchanged PR must cost nothing", got)
	}
	if got := len(src.opsMatching("comment-create:")); got != 1 {
		t.Errorf("created %d comments, want exactly one per PR", got)
	}
	if got := src.opsMatching("comment-update:"); len(got) != 0 {
		t.Errorf("updated the comment %v times on an unchanged PR, want none", got)
	}
}

func TestSweepRewritesTheSameCommentWhenTheHeadMoves(t *testing.T) {
	src := newFakeVerdictSource()
	src.addPR(900, "aaa", true, []string{"docs/a.md"})
	sweeper := newTestSweeper(src)
	ctx := context.Background()
	sweeper.Sweep(ctx)
	firstID := src.comments[900].ID

	src.prs[0].HeadSHA = "bbb"
	facts := src.facts[900]
	facts.HeadSHA, facts.Files = "bbb", []string{"docs/a.md", "ops/x.yml"}
	src.facts[900] = facts
	sweeper.Sweep(ctx)

	if got := len(src.opsMatching("comment-create:")); got != 1 {
		t.Errorf("created %d comments, want one — the verdict is rewritten in place", got)
	}
	if got := src.opsMatching("comment-update:"); len(got) != 1 {
		t.Errorf("comment updates = %v, want exactly one", got)
	}
	if src.comments[900].ID != firstID {
		t.Errorf("comment id changed from %d to %d", firstID, src.comments[900].ID)
	}
	if got := src.prLabels[900]; got[0] != "merge:never" {
		t.Errorf("label = %v, want merge:never after the push added an ops file", got)
	}
}

func TestSweepRescoresWhenThePolicyChanges(t *testing.T) {
	src := newFakeVerdictSource()
	src.addPR(900, "aaa", true, []string{"adapters/fq-cron/main.go"})
	sweeper := newTestSweeper(src)
	ctx := context.Background()
	sweeper.Sweep(ctx)
	if got := src.prLabels[900][0]; got != "merge:unsupervised" {
		t.Fatalf("label = %s, want merge:unsupervised", got)
	}

	src.files[PolicyPath+"@main"] = []byte(strings.Replace(testPolicyYAML, "adapters: unsupervised", "adapters: never", 1))
	sweeper.Sweep(ctx)

	if got := src.prLabels[900][0]; got != "merge:never" {
		t.Errorf("label = %s, want merge:never — a policy edit must re-score PRs nobody pushed to", got)
	}
}

// A restart forgets every cache entry. Re-scoring must then write nothing
// at all, or every open PR's comment is edited to identical text and
// everyone watching it is notified.
func TestSweepAfterARestartWritesNothingWhenNothingChanged(t *testing.T) {
	src := newFakeVerdictSource()
	src.addPR(900, "aaa", true, []string{"docs/a.md"})
	newTestSweeper(src).Sweep(context.Background())
	before := len(src.ops)

	newTestSweeper(src).Sweep(context.Background())

	if got := src.opsMatching("comment-update:"); len(got) != 0 {
		t.Errorf("comment updates after a restart = %v, want none", got)
	}
	if len(src.ops) <= before {
		t.Fatal("the second sweep did no work at all; the test is not exercising a re-score")
	}
}

func TestSweepSkipsWhenAVerdictLabelIsMissing(t *testing.T) {
	src := newFakeVerdictSource()
	src.labels = []string{"merge:unsupervised", "merge:never"}
	src.addPR(900, "aaa", true, []string{"docs/a.md"})
	newTestSweeper(src).Sweep(context.Background())

	if got := src.opsMatching("label:"); len(got) != 0 {
		t.Errorf("label ops = %v, want none when a verdict label does not exist", got)
	}
	if len(src.comments) != 0 {
		t.Errorf("commented on %d PRs, want none", len(src.comments))
	}
}

func TestSweepSurvivesEveryGitHubFailure(t *testing.T) {
	boom := errors.New("boom")
	cases := map[string]func(*fakeVerdictSource){
		"listing PRs fails":     func(f *fakeVerdictSource) { f.listErr = boom },
		"listing labels fails":  func(f *fakeVerdictSource) { f.labelsErr = boom },
		"gathering facts fails": func(f *fakeVerdictSource) { f.factsErr = boom },
		"the policy is unreadable": func(f *fakeVerdictSource) {
			delete(f.files, PolicyPath+"@main")
		},
		"the policy is malformed": func(f *fakeVerdictSource) {
			f.files[PolicyPath+"@main"] = []byte("default_tier: nonsense\nmin_age_minutes: 1\nareas:\n  docs: unsupervised\n")
		},
	}
	for name, breakIt := range cases {
		t.Run(name, func(t *testing.T) {
			src := newFakeVerdictSource()
			src.addPR(900, "aaa", true, []string{"docs/a.md"})
			breakIt(src)
			newTestSweeper(src).Sweep(context.Background())
			if len(src.comments) != 0 {
				t.Errorf("wrote a verdict despite %s", name)
			}
		})
	}
}

// The sweep is the advisory last step of a poll: it must never be able to
// fail the cycle that moves labels.
func TestPollOnceSurvivesAFailingMergeVerdictSweep(t *testing.T) {
	src := newFakeVerdictSource()
	src.listErr = errors.New("github is down")
	rec := &recorder{}
	w := &Watcher{
		Source:        &fakeSource{rec: rec},
		Publisher:     &fakePublisher{rec: rec},
		Config:        testConfig(),
		Log:           discardLogger(),
		MergeVerdicts: newTestSweeper(src),
	}
	if err := w.pollOnce(context.Background()); err != nil {
		t.Fatalf("pollOnce = %v, want nil", err)
	}
}

func TestVerdictCommentRendersTheTruncationNote(t *testing.T) {
	src := newFakeVerdictSource()
	files := make([]string, maxVerdictFileRows+3)
	for i := range files {
		files[i] = fmt.Sprintf("docs/%03d.md", i)
	}
	src.addPR(900, "aaa", true, files)
	newTestSweeper(src).Sweep(context.Background())

	body := src.comments[900].Body
	if !strings.Contains(body, "… and 3 more changed file(s)") {
		t.Errorf("wide PR comment has no truncation note:\n%s", body)
	}
}
