package main

import (
	"bytes"
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"net/http"
	"os"
	"strings"

	"github.com/google/go-github/v76/github"
)

// GhCliIssueSource implements the watcher GitHub interfaces using the GitHub
// API directly. Its name is retained to avoid changing the internal interfaces.
type GhCliIssueSource struct {
	Repo            string // "owner/name"
	Client          *github.Client
	Token           string
	GraphQLEndpoint string
}

// NewGhCliIssueSource creates an authenticated API source. GH_TOKEN is
// preferred for compatibility with GitHub tooling; GITHUB_TOKEN is a fallback.
func NewGhCliIssueSource(repo string) (*GhCliIssueSource, error) {
	token := os.Getenv("GH_TOKEN")
	if token == "" {
		token = os.Getenv("GITHUB_TOKEN")
	}
	if token == "" {
		return nil, fmt.Errorf("GitHub token is required: set GH_TOKEN or GITHUB_TOKEN")
	}
	return &GhCliIssueSource{Repo: repo, Client: github.NewClient(nil).WithAuthToken(token), Token: token, GraphQLEndpoint: "https://api.github.com/graphql"}, nil
}

func (g *GhCliIssueSource) ListReady(ctx context.Context, readyLabel string) ([]Issue, error) {
	return g.ListByLabel(ctx, readyLabel)
}

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

// Relabel matches gh's idempotency: a missing removed label is harmless.
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

const mergedPRQuery = `query($owner:String!,$repo:String!,$number:Int!){
  repository(owner:$owner,name:$repo){ issue(number:$number){ closedByPullRequestsReferences(first:20,includeClosedPrs:true){ nodes{ merged } } } }
}`

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

func (g *GhCliIssueSource) HasMergedPR(ctx context.Context, number int) (bool, error) {
	out, err := g.graphQL(ctx, mergedPRQuery, number)
	if err != nil {
		return false, err
	}
	return parseMergedPRResponse(out)
}
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

const openPRQuery = `query($owner:String!,$repo:String!,$number:Int!){
  repository(owner:$owner,name:$repo){ issue(number:$number){ closedByPullRequestsReferences(first:20){ nodes{ number state } } } }
}`

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

func (g *GhCliIssueSource) OpenPRsClosingIssue(ctx context.Context, number int) ([]int, error) {
	out, err := g.graphQL(ctx, openPRQuery, number)
	if err != nil {
		return nil, err
	}
	return parseOpenPRResponse(out)
}
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
	resp, err := http.DefaultClient.Do(req)
	if err != nil {
		return nil, fmt.Errorf("GitHub GraphQL: %w", err)
	}
	defer resp.Body.Close()
	if resp.StatusCode < 200 || resp.StatusCode >= 300 {
		return nil, fmt.Errorf("GitHub GraphQL: %s", resp.Status)
	}
	var raw json.RawMessage
	if err := json.NewDecoder(resp.Body).Decode(&raw); err != nil {
		return nil, err
	}
	return raw, nil
}

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
func splitRepo(repo string) (owner, name string, err error) {
	parts := strings.SplitN(repo, "/", 2)
	if len(parts) != 2 || parts[0] == "" || parts[1] == "" {
		return "", "", fmt.Errorf("repo must be owner/name, got %q", repo)
	}
	return parts[0], parts[1], nil
}

// parseGhIssueList remains available for the legacy parser unit test; GitHub's
// REST issue representation has the same number/label fields.
type ghLabel struct {
	Name string `json:"name"`
}
type ghIssue struct {
	Number int       `json:"number"`
	Labels []ghLabel `json:"labels"`
}

func parseGhIssueList(data []byte) ([]Issue, error) {
	var raw []ghIssue
	if err := json.Unmarshal(data, &raw); err != nil {
		return nil, fmt.Errorf("parse issue list response: %w", err)
	}
	issues := make([]Issue, 0, len(raw))
	for _, item := range raw {
		labels := make([]string, 0, len(item.Labels))
		for _, label := range item.Labels {
			labels = append(labels, label.Name)
		}
		issues = append(issues, Issue{Number: item.Number, Labels: labels})
	}
	return issues, nil
}
