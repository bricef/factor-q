package main

import (
	"context"
	"encoding/json"
	"net/http"
	"net/http/httptest"
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
			http.NotFound(w, r) // removal is deliberately lenient
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
