package main

import (
	"context"
	"encoding/json"
	"errors"
	"net/http"
	"net/http/httptest"
	"slices"
	"strings"
	"testing"

	"github.com/google/go-github/v76/github"
)

func TestGitHubAPISource(t *testing.T) {
	var removed, added, edited bool
	server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		if r.Header.Get("Authorization") != "Bearer token" {
			t.Errorf("authorization = %q", r.Header.Get("Authorization"))
		}
		switch {
		case r.URL.Path == "/repos/o/r/issues":
			json.NewEncoder(w).Encode([]map[string]any{{"number": 7, "labels": []map[string]string{{"name": "ready"}}}})
		case r.URL.Path == "/repos/o/r/issues/7/labels/ready" && r.Method == http.MethodDelete:
			removed = true
			w.Write([]byte("[]"))
		case r.URL.Path == "/repos/o/r/issues/7/labels" && r.Method == http.MethodPost:
			added = true
			w.Write([]byte("[]"))
		case r.URL.Path == "/repos/o/r/pulls/9" && r.Method == http.MethodGet:
			w.Write([]byte(`{"body":"before"}`))
		case r.URL.Path == "/repos/o/r/pulls/9" && r.Method == http.MethodPatch:
			edited = true
			w.Write([]byte(`{"body":"after"}`))
		case r.URL.Path == "/graphql":
			var request struct {
				Query string `json:"query"`
			}
			json.NewDecoder(r.Body).Decode(&request)
			if strings.Contains(request.Query, "merged") {
				w.Write([]byte(`{"data":{"repository":{"issue":{"closedByPullRequestsReferences":{"nodes":[{"merged":true}]}}}}}`))
			} else {
				w.Write([]byte(`{"data":{"repository":{"issue":{"closedByPullRequestsReferences":{"nodes":[{"number":12,"state":"OPEN"},{"number":13,"state":"CLOSED"}]}}}}}`))
			}
		default:
			t.Errorf("unexpected request %s %s", r.Method, r.URL.Path)
		}
	}))
	defer server.Close()
	client := github.NewClient(nil).WithAuthToken("token")
	client.BaseURL, _ = client.BaseURL.Parse(server.URL + "/")
	source := &GhCliIssueSource{Repo: "o/r", Client: client, Token: "token", GraphQLEndpoint: server.URL + "/graphql"}
	ctx := context.Background()
	issues, err := source.ListByLabel(ctx, "ready")
	if err != nil || len(issues) != 1 || issues[0].Number != 7 {
		t.Fatalf("ListByLabel = %#v, %v", issues, err)
	}
	if err := source.Relabel(ctx, 7, "ready", "in-progress"); err != nil || !removed || !added {
		t.Fatalf("Relabel = %v, removed=%t added=%t", err, removed, added)
	}
	if merged, err := source.HasMergedPR(ctx, 7); err != nil || !merged {
		t.Fatalf("HasMergedPR = %t, %v", merged, err)
	}
	if prs, err := source.OpenPRsClosingIssue(ctx, 7); err != nil || len(prs) != 1 || prs[0] != 12 {
		t.Fatalf("OpenPRsClosingIssue = %v, %v", prs, err)
	}
	if body, err := source.PRBody(ctx, 9); err != nil || body != "before" {
		t.Fatalf("PRBody = %q, %v", body, err)
	}
	if err := source.SetPRBody(ctx, 9, "after"); err != nil || !edited {
		t.Fatalf("SetPRBody = %v, edited=%t", err, edited)
	}
}

func TestNewGhCliIssueSourceRequiresToken(t *testing.T) {
	t.Setenv("GH_TOKEN", "")
	t.Setenv("GITHUB_TOKEN", "")
	if _, err := NewGhCliIssueSource("o/r"); err == nil {
		t.Fatal("NewGhCliIssueSource succeeded without token")
	}
	t.Setenv("GITHUB_TOKEN", "fallback")
	if source, err := NewGhCliIssueSource("o/r"); err != nil || source.Token != "fallback" {
		t.Fatalf("fallback = %#v, %v", source, err)
	}
}

// GraphQL failures arrive as HTTP 200 with an `errors` array — they
// must surface as errors, not silently read as "no PRs" (the
// label-stranding failure mode). gh exited non-zero here; so do we.
func TestGraphQLQueryErrorsAreLoud(t *testing.T) {
	server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		w.Write([]byte(`{"data":null,"errors":[{"message":"boom: insufficient scopes"}]}`))
	}))
	defer server.Close()
	source := &GhCliIssueSource{Repo: "o/r", Token: "token", GraphQLEndpoint: server.URL}

	if _, err := source.HasMergedPR(context.Background(), 7); err == nil || !strings.Contains(err.Error(), "boom") {
		t.Fatalf("HasMergedPR must surface GraphQL errors, got: %v", err)
	}
	if _, err := source.OpenPRsClosingIssue(context.Background(), 7); err == nil || !strings.Contains(err.Error(), "boom") {
		t.Fatalf("OpenPRsClosingIssue must surface GraphQL errors, got: %v", err)
	}
}

// Relabel's remove-leniency is 404-only: the removal out of `ready` is
// the double-trigger dedup, so a permission failure must fail loudly —
// a lenient 403 would let a claim "succeed" while the issue stays
// `ready` and re-triggers next poll.
func TestRelabelFailsLoudlyOnForbiddenRemoval(t *testing.T) {
	server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		switch {
		case r.Method == http.MethodPost: // the add succeeds
			w.Write([]byte("[]"))
		case r.URL.Path == "/repos/o/r/issues/7/labels/in-progress": // and rolls back
			w.Write([]byte("[]"))
		default: // the removal out of `ready` is what fails
			http.Error(w, `{"message":"forbidden"}`, http.StatusForbidden)
		}
	}))
	defer server.Close()
	client := github.NewClient(nil)
	client.BaseURL, _ = client.BaseURL.Parse(server.URL + "/")
	source := &GhCliIssueSource{Repo: "o/r", Client: client, Token: "token", GraphQLEndpoint: server.URL + "/graphql"}

	err := source.Relabel(context.Background(), 7, "ready", "in-progress")
	if err == nil {
		t.Fatal("a 403 on label removal must fail the claim, not be swallowed")
	}
	if errors.Is(err, ErrClaimLost) {
		t.Fatalf("a 403 is not a lost claim, it is a broken watcher: %v", err)
	}
}

// The claim is add-then-remove. Removing first leaves a window in which
// the issue carries no status label at all, and an interruption there
// strands it where no list will ever show it again.
func TestRelabelAddsBeforeItRemoves(t *testing.T) {
	var calls []string
	server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		switch {
		case r.URL.Path == "/repos/o/r/issues/7/labels" && r.Method == http.MethodPost:
			calls = append(calls, "add in-progress")
			w.Write([]byte("[]"))
		case r.URL.Path == "/repos/o/r/issues/7/labels/ready" && r.Method == http.MethodDelete:
			calls = append(calls, "remove ready")
			w.Write([]byte("[]"))
		default:
			t.Errorf("unexpected request %s %s", r.Method, r.URL.Path)
		}
	}))
	defer server.Close()
	client := github.NewClient(nil)
	client.BaseURL, _ = client.BaseURL.Parse(server.URL + "/")
	source := &GhCliIssueSource{Repo: "o/r", Client: client, Token: "token"}

	if err := source.Relabel(context.Background(), 7, "ready", "in-progress"); err != nil {
		t.Fatal(err)
	}
	if want := []string{"add in-progress", "remove ready"}; !slices.Equal(calls, want) {
		t.Fatalf("call order = %v, want %v", calls, want)
	}
}

// A 5xx on the removal is a failed transition, not a lost race. The add
// must be rolled back: an issue left carrying both labels is skipped by
// the planner on every later cycle, so "will retry next poll" would be a
// lie and the issue would sit there until a human noticed. Removing first
// — the old order — at least left it `ready` and retrying.
func TestRelabelRollsBackTheAddWhenTheRemovalFails(t *testing.T) {
	var calls []string
	server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		switch {
		case r.URL.Path == "/repos/o/r/issues/7/labels" && r.Method == http.MethodPost:
			calls = append(calls, "add in-progress")
			w.Write([]byte("[]"))
		case r.URL.Path == "/repos/o/r/issues/7/labels/ready" && r.Method == http.MethodDelete:
			calls = append(calls, "remove ready")
			http.Error(w, `{"message":"bad gateway"}`, http.StatusBadGateway)
		case r.URL.Path == "/repos/o/r/issues/7/labels/in-progress" && r.Method == http.MethodDelete:
			calls = append(calls, "roll back in-progress")
			w.Write([]byte("[]"))
		default:
			t.Errorf("unexpected request %s %s", r.Method, r.URL.Path)
		}
	}))
	defer server.Close()
	client := github.NewClient(nil)
	client.BaseURL, _ = client.BaseURL.Parse(server.URL + "/")
	source := &GhCliIssueSource{Repo: "o/r", Client: client, Token: "token"}

	err := source.Relabel(context.Background(), 7, "ready", "in-progress")
	if err == nil {
		t.Fatal("a 502 on the removal must fail the transition")
	}
	if errors.Is(err, ErrClaimLost) || errors.Is(err, ErrBothLabels) {
		t.Fatalf("a rolled-back transition is a plain retryable failure: %v", err)
	}
	want := []string{"add in-progress", "remove ready", "roll back in-progress"}
	if !slices.Equal(calls, want) {
		t.Fatalf("call order = %v, want %v", calls, want)
	}
}

// When the rollback fails too, the issue really does carry both labels
// and no poll will pick it up again. That is ErrBothLabels, and the
// caller must say so instead of promising a retry.
func TestRelabelReportsBothLabelsWhenTheRollbackFails(t *testing.T) {
	server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		if r.Method == http.MethodPost {
			w.Write([]byte("[]"))
			return
		}
		http.Error(w, `{"message":"bad gateway"}`, http.StatusBadGateway)
	}))
	defer server.Close()
	client := github.NewClient(nil)
	client.BaseURL, _ = client.BaseURL.Parse(server.URL + "/")
	source := &GhCliIssueSource{Repo: "o/r", Client: client, Token: "token"}

	err := source.Relabel(context.Background(), 7, "ready", "in-progress")
	if !errors.Is(err, ErrBothLabels) {
		t.Fatalf("Relabel = %v, want ErrBothLabels", err)
	}
	if !strings.Contains(err.Error(), "ready") || !strings.Contains(err.Error(), "in-progress") {
		t.Errorf("error should name both labels so an operator knows what to remove: %v", err)
	}
}

// A 404 on the removal means someone else already made this transition —
// the other watcher the deploy runs, or a person on the issue. The old
// code treated it as idempotency and carried on, which is how one issue
// got two triggers.
func TestRelabelReportsALostClaimOnMissingLabel(t *testing.T) {
	server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		if r.Method == http.MethodPost {
			w.Write([]byte("[]"))
			return
		}
		http.NotFound(w, r)
	}))
	defer server.Close()
	client := github.NewClient(nil)
	client.BaseURL, _ = client.BaseURL.Parse(server.URL + "/")
	source := &GhCliIssueSource{Repo: "o/r", Client: client, Token: "token"}

	err := source.Relabel(context.Background(), 7, "ready", "in-progress")
	if !errors.Is(err, ErrClaimLost) {
		t.Fatalf("Relabel = %v, want ErrClaimLost", err)
	}
	if !strings.Contains(err.Error(), "ready") || !strings.Contains(err.Error(), "#7") {
		t.Errorf("error should name the label and the issue: %v", err)
	}
}
