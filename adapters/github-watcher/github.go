package main

import (
	"bytes"
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"net/http"
	"os"
	"strings"
	"time"

	"github.com/google/go-github/v76/github"
)

// githubHTTPClient bounds every GitHub call (REST and GraphQL). The
// poll loop's context carries no deadline, so without a client timeout
// a single hung connection would wedge polling indefinitely.
var githubHTTPClient = &http.Client{Timeout: 30 * time.Second}

// GhCliIssueSource implements the watcher GitHub interfaces using the GitHub
// API directly (issue #30 — no `gh` subprocess, no PATH dependency). Its
// name is retained to avoid churning the internal interfaces.
type GhCliIssueSource struct {
	Repo            string // "owner/name"
	Client          *github.Client
	Token           string
	GraphQLEndpoint string
}

// NewGhCliIssueSource creates an authenticated API source. GH_TOKEN is
// preferred for compatibility with GitHub tooling; GITHUB_TOKEN is a fallback.
// No token is a startup error, not a per-call one: the watcher fails fast
// and loudly rather than polling uselessly.
func NewGhCliIssueSource(repo string) (*GhCliIssueSource, error) {
	token := os.Getenv("GH_TOKEN")
	if token == "" {
		token = os.Getenv("GITHUB_TOKEN")
	}
	if token == "" {
		return nil, fmt.Errorf("GitHub token is required: set GH_TOKEN or GITHUB_TOKEN")
	}
	return &GhCliIssueSource{
		Repo:            repo,
		Client:          github.NewClient(githubHTTPClient).WithAuthToken(token),
		Token:           token,
		GraphQLEndpoint: "https://api.github.com/graphql",
	}, nil
}

// ListReady returns open issues carrying readyLabel.
func (g *GhCliIssueSource) ListReady(ctx context.Context, readyLabel string) ([]Issue, error) {
	return g.ListByLabel(ctx, readyLabel)
}

// ListByLabel returns open issues carrying label (first 100 — the same
// cap the old `gh issue list --limit 100` had).
func (g *GhCliIssueSource) ListByLabel(ctx context.Context, label string) ([]Issue, error) {
	owner, repo, err := splitRepo(g.Repo)
	if err != nil {
		return nil, err
	}
	issues, _, err := g.Client.Issues.ListByRepo(ctx, owner, repo, &github.IssueListByRepoOptions{State: "open", Labels: []string{label}, ListOptions: github.ListOptions{PerPage: 100}})
	if err != nil {
		return nil, fmt.Errorf("list issues: %w", err)
	}
	out := make([]Issue, 0, len(issues))
	for _, issue := range issues {
		labels := make([]string, 0, len(issue.Labels))
		for _, label := range issue.Labels {
			labels = append(labels, label.GetName())
		}
		out = append(out, Issue{Number: issue.GetNumber(), Labels: labels})
	}
	return out, nil
}

// Relabel removes `remove` and adds `add` on the issue. Removing out of
// `ready` before triggering is the watcher's double-trigger dedup, so
// leniency is scoped precisely: a *missing* label (404) is harmless —
// matching gh's idempotency — but any other removal failure (403, 5xx)
// must fail loudly, or a claim could "succeed" while the issue stays
// `ready` and re-triggers on the next poll.
func (g *GhCliIssueSource) Relabel(ctx context.Context, number int, remove, add string) error {
	owner, repo, err := splitRepo(g.Repo)
	if err != nil {
		return err
	}
	_, err = g.Client.Issues.RemoveLabelForIssue(ctx, owner, repo, number, remove)
	if err != nil && !isNotFound(err) {
		return fmt.Errorf("remove label from #%d: %w", number, err)
	}
	if _, _, err := g.Client.Issues.AddLabelsToIssue(ctx, owner, repo, number, []string{add}); err != nil {
		return fmt.Errorf("add label to #%d: %w", number, err)
	}
	return nil
}

func isNotFound(err error) bool {
	var response *github.ErrorResponse
	return errors.As(err, &response) && response.Response != nil && response.Response.StatusCode == http.StatusNotFound
}

// mergedPRQuery asks the GraphQL API for the merge state of the PRs
// that close an issue. `closedByPullRequestsReferences` is the
// authoritative "this PR closes this issue" link (a PR with
// `Closes #N`), so a merged one means the proposed fix landed.
const mergedPRQuery = `query($owner:String!,$repo:String!,$number:Int!){
  repository(owner:$owner,name:$repo){ issue(number:$number){ closedByPullRequestsReferences(first:20,includeClosedPrs:true){ nodes{ merged } } } }
}`

// ghMergedPRResponse mirrors the GraphQL response for mergedPRQuery.
type ghMergedPRResponse struct {
	Data struct {
		Repository struct {
			Issue struct {
				ClosedByPullRequestsReferences struct {
					Nodes []struct {
						Merged bool `json:"merged"`
					} `json:"nodes"`
				} `json:"closedByPullRequestsReferences"`
			} `json:"issue"`
		} `json:"repository"`
	} `json:"data"`
}

// HasMergedPR reports whether a merged PR closes the issue.
func (g *GhCliIssueSource) HasMergedPR(ctx context.Context, number int) (bool, error) {
	out, err := g.graphQL(ctx, mergedPRQuery, number)
	if err != nil {
		return false, err
	}
	return parseMergedPRResponse(out)
}

// parseMergedPRResponse reports whether any closing PR in the GraphQL
// response has merged. Split out so it is unit-testable without a server.
func parseMergedPRResponse(stdout []byte) (bool, error) {
	var resp ghMergedPRResponse
	if err := json.Unmarshal(stdout, &resp); err != nil {
		return false, fmt.Errorf("parse GitHub GraphQL response: %w", err)
	}
	for _, n := range resp.Data.Repository.Issue.ClosedByPullRequestsReferences.Nodes {
		if n.Merged {
			return true, nil
		}
	}
	return false, nil
}

// openPRQuery asks the GraphQL API for the *open* PRs that close an
// issue — the provenance-stamp targets (issue #162). Same authoritative
// `closedByPullRequestsReferences` link as mergedPRQuery, but excluding
// closed/merged PRs (`includeClosedPrs` defaults false; the state field
// is still selected and filtered defensively).
const openPRQuery = `query($owner:String!,$repo:String!,$number:Int!){
  repository(owner:$owner,name:$repo){ issue(number:$number){ closedByPullRequestsReferences(first:20){ nodes{ number state } } } }
}`

// ghOpenPRResponse mirrors the GraphQL response for openPRQuery.
type ghOpenPRResponse struct {
	Data struct {
		Repository struct {
			Issue struct {
				ClosedByPullRequestsReferences struct {
					Nodes []struct {
						Number int    `json:"number"`
						State  string `json:"state"`
					} `json:"nodes"`
				} `json:"closedByPullRequestsReferences"`
			} `json:"issue"`
		} `json:"repository"`
	} `json:"data"`
}

// OpenPRsClosingIssue returns the numbers of open PRs that close the
// issue — the targets of the provenance stamp (issue #162).
func (g *GhCliIssueSource) OpenPRsClosingIssue(ctx context.Context, number int) ([]int, error) {
	out, err := g.graphQL(ctx, openPRQuery, number)
	if err != nil {
		return nil, err
	}
	return parseOpenPRResponse(out)
}

// parseOpenPRResponse extracts the open closing-PR numbers. Split out so
// it is unit-testable without a server.
func parseOpenPRResponse(stdout []byte) ([]int, error) {
	var resp ghOpenPRResponse
	if err := json.Unmarshal(stdout, &resp); err != nil {
		return nil, fmt.Errorf("parse GitHub GraphQL response: %w", err)
	}
	var prs []int
	for _, n := range resp.Data.Repository.Issue.ClosedByPullRequestsReferences.Nodes {
		if n.State == "OPEN" {
			prs = append(prs, n.Number)
		}
	}
	return prs, nil
}

// graphQL posts one of the fixed queries and returns the raw response
// body for the typed parsers above. GraphQL failures arrive as HTTP 200
// with an `errors` array and null data — decoding that envelope
// "succeeds" with zero nodes, which would silently read as "no merged
// PR" / "no open PRs" (exactly the label-stranding failure mode the
// watcher exists to prevent; `gh api graphql` exited non-zero here). So
// the envelope is checked and query-level errors are returned as errors.
func (g *GhCliIssueSource) graphQL(ctx context.Context, query string, number int) ([]byte, error) {
	owner, repo, err := splitRepo(g.Repo)
	if err != nil {
		return nil, err
	}
	body, err := json.Marshal(struct {
		Query     string         `json:"query"`
		Variables map[string]any `json:"variables"`
	}{query, map[string]any{"owner": owner, "repo": repo, "number": number}})
	if err != nil {
		return nil, err
	}
	req, err := http.NewRequestWithContext(ctx, http.MethodPost, g.GraphQLEndpoint, bytes.NewReader(body))
	if err != nil {
		return nil, err
	}
	req.Header.Set("Authorization", "Bearer "+g.Token)
	req.Header.Set("Content-Type", "application/json")
	resp, err := githubHTTPClient.Do(req)
	if err != nil {
		return nil, fmt.Errorf("GitHub GraphQL: %w", err)
	}
	defer resp.Body.Close()
	if resp.StatusCode < 200 || resp.StatusCode >= 300 {
		return nil, fmt.Errorf("GitHub GraphQL: %s", resp.Status)
	}
	raw, err := io.ReadAll(resp.Body)
	if err != nil {
		return nil, fmt.Errorf("GitHub GraphQL: read response: %w", err)
	}
	var envelope struct {
		Errors []struct {
			Message string `json:"message"`
		} `json:"errors"`
	}
	if err := json.Unmarshal(raw, &envelope); err != nil {
		return nil, fmt.Errorf("parse GitHub GraphQL response: %w", err)
	}
	if len(envelope.Errors) > 0 {
		msgs := make([]string, 0, len(envelope.Errors))
		for _, e := range envelope.Errors {
			msgs = append(msgs, e.Message)
		}
		return nil, fmt.Errorf("GitHub GraphQL errors (issue #%d): %s", number, strings.Join(msgs, "; "))
	}
	return raw, nil
}

// PRBody returns the PR's current body.
func (g *GhCliIssueSource) PRBody(ctx context.Context, pr int) (string, error) {
	owner, repo, err := splitRepo(g.Repo)
	if err != nil {
		return "", err
	}
	pull, _, err := g.Client.PullRequests.Get(ctx, owner, repo, pr)
	if err != nil {
		return "", fmt.Errorf("get PR #%d: %w", pr, err)
	}
	return pull.GetBody(), nil
}

// SetPRBody replaces the PR's body (the provenance-stamp write, #162).
func (g *GhCliIssueSource) SetPRBody(ctx context.Context, pr int, body string) error {
	owner, repo, err := splitRepo(g.Repo)
	if err != nil {
		return err
	}
	_, _, err = g.Client.PullRequests.Edit(ctx, owner, repo, pr, &github.PullRequest{Body: github.Ptr(body)})
	if err != nil {
		return fmt.Errorf("edit PR #%d: %w", pr, err)
	}
	return nil
}

// splitRepo splits "owner/name" into its parts.
func splitRepo(repo string) (owner, name string, err error) {
	parts := strings.SplitN(repo, "/", 2)
	if len(parts) != 2 || parts[0] == "" || parts[1] == "" {
		return "", "", fmt.Errorf("repo must be owner/name, got %q", repo)
	}
	return parts[0], parts[1], nil
}
