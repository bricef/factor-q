package main

import (
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"os/exec"
	"strconv"
	"strings"
)

// GhCliIssueSource implements IssueSource and ReviewSource by shelling out
// to the `gh` CLI, reusing the operator's existing GitHub auth. `gh` must
// be on PATH and authenticated (e.g. via GH_TOKEN).
type GhCliIssueSource struct {
	Repo string // "owner/name"
}

// ghLabel and ghIssue mirror the JSON emitted by
// `gh issue list --json number,labels`.
type ghLabel struct {
	Name string `json:"name"`
}

type ghIssue struct {
	Number int       `json:"number"`
	Labels []ghLabel `json:"labels"`
}

// ListReady returns open issues carrying readyLabel.
func (g *GhCliIssueSource) ListReady(ctx context.Context, readyLabel string) ([]Issue, error) {
	return g.ListByLabel(ctx, readyLabel)
}

// ListByLabel returns open issues carrying label.
func (g *GhCliIssueSource) ListByLabel(ctx context.Context, label string) ([]Issue, error) {
	cmd := exec.CommandContext(ctx, "gh", "issue", "list",
		"--repo", g.Repo,
		"--label", label,
		"--state", "open",
		"--json", "number,labels",
		"--limit", "100",
	)
	out, err := cmd.Output()
	if err != nil {
		return nil, fmt.Errorf("gh issue list: %w", ghError(err))
	}
	return parseGhIssueList(out)
}

// parseGhIssueList converts `gh issue list --json number,labels` output
// into Issues. Split out so it is unit-testable without invoking gh.
func parseGhIssueList(stdout []byte) ([]Issue, error) {
	var raw []ghIssue
	if err := json.Unmarshal(stdout, &raw); err != nil {
		return nil, fmt.Errorf("parse gh issue list output: %w", err)
	}
	issues := make([]Issue, 0, len(raw))
	for _, r := range raw {
		labels := make([]string, 0, len(r.Labels))
		for _, l := range r.Labels {
			labels = append(labels, l.Name)
		}
		issues = append(issues, Issue{Number: r.Number, Labels: labels})
	}
	return issues, nil
}

// Relabel removes `remove` and adds `add` on the issue via `gh issue edit`.
func (g *GhCliIssueSource) Relabel(ctx context.Context, number int, remove, add string) error {
	cmd := exec.CommandContext(ctx, "gh", "issue", "edit", strconv.Itoa(number),
		"--repo", g.Repo,
		"--remove-label", remove,
		"--add-label", add,
	)
	// cmd.Output() (not cmd.Run()) so a non-zero exit populates
	// ExitError.Stderr, which ghError surfaces — otherwise a failed
	// relabel logs a bare "exit status 1" with no reason (e.g. a
	// missing target label). stdout is discarded.
	if _, err := cmd.Output(); err != nil {
		return fmt.Errorf("gh issue edit #%d: %w", number, ghError(err))
	}
	return nil
}

// mergedPRQuery asks the GitHub GraphQL API for the merge state of the PRs
// that close an issue. `closedByPullRequestsReferences` is the authoritative
// "this PR closes this issue" link (a PR with `Closes #N`), so a merged one
// means the proposed fix landed.
const mergedPRQuery = `query($owner:String!,$repo:String!,$number:Int!){
  repository(owner:$owner,name:$repo){
    issue(number:$number){
      closedByPullRequestsReferences(first:20,includeClosedPrs:true){
        nodes{ merged }
      }
    }
  }
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

// HasMergedPR reports whether a merged PR closes the issue, via the GitHub
// GraphQL API.
func (g *GhCliIssueSource) HasMergedPR(ctx context.Context, number int) (bool, error) {
	owner, repo, err := splitRepo(g.Repo)
	if err != nil {
		return false, err
	}
	cmd := exec.CommandContext(ctx, "gh", "api", "graphql",
		"-f", "query="+mergedPRQuery,
		"-F", "owner="+owner,
		"-F", "repo="+repo,
		"-F", "number="+strconv.Itoa(number),
	)
	out, err := cmd.Output()
	if err != nil {
		return false, fmt.Errorf("gh api graphql (issue #%d): %w", number, ghError(err))
	}
	return parseMergedPRResponse(out)
}

// parseMergedPRResponse reports whether any closing PR in the GraphQL
// response has merged. Split out so it is unit-testable without invoking gh.
func parseMergedPRResponse(stdout []byte) (bool, error) {
	var resp ghMergedPRResponse
	if err := json.Unmarshal(stdout, &resp); err != nil {
		return false, fmt.Errorf("parse gh api graphql output: %w", err)
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
  repository(owner:$owner,name:$repo){
    issue(number:$number){
      closedByPullRequestsReferences(first:20){
        nodes{ number state }
      }
    }
  }
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
// issue, via the GitHub GraphQL API.
func (g *GhCliIssueSource) OpenPRsClosingIssue(ctx context.Context, number int) ([]int, error) {
	owner, repo, err := splitRepo(g.Repo)
	if err != nil {
		return nil, err
	}
	cmd := exec.CommandContext(ctx, "gh", "api", "graphql",
		"-f", "query="+openPRQuery,
		"-F", "owner="+owner,
		"-F", "repo="+repo,
		"-F", "number="+strconv.Itoa(number),
	)
	out, err := cmd.Output()
	if err != nil {
		return nil, fmt.Errorf("gh api graphql (issue #%d): %w", number, ghError(err))
	}
	return parseOpenPRResponse(out)
}

// parseOpenPRResponse extracts the open closing-PR numbers. Split out so
// it is unit-testable without invoking gh.
func parseOpenPRResponse(stdout []byte) ([]int, error) {
	var resp ghOpenPRResponse
	if err := json.Unmarshal(stdout, &resp); err != nil {
		return nil, fmt.Errorf("parse gh api graphql output: %w", err)
	}
	var prs []int
	for _, n := range resp.Data.Repository.Issue.ClosedByPullRequestsReferences.Nodes {
		if n.State == "OPEN" {
			prs = append(prs, n.Number)
		}
	}
	return prs, nil
}

// PRBody returns the PR's current body via `gh pr view`.
func (g *GhCliIssueSource) PRBody(ctx context.Context, pr int) (string, error) {
	cmd := exec.CommandContext(ctx, "gh", "pr", "view", strconv.Itoa(pr),
		"--repo", g.Repo,
		"--json", "body",
	)
	out, err := cmd.Output()
	if err != nil {
		return "", fmt.Errorf("gh pr view #%d: %w", pr, ghError(err))
	}
	var resp struct {
		Body string `json:"body"`
	}
	if err := json.Unmarshal(out, &resp); err != nil {
		return "", fmt.Errorf("parse gh pr view output: %w", err)
	}
	return resp.Body, nil
}

// SetPRBody replaces the PR's body via `gh pr edit`. The body is passed
// on stdin (`--body-file -`) so its length and content never fight the
// argument list.
func (g *GhCliIssueSource) SetPRBody(ctx context.Context, pr int, body string) error {
	cmd := exec.CommandContext(ctx, "gh", "pr", "edit", strconv.Itoa(pr),
		"--repo", g.Repo,
		"--body-file", "-",
	)
	cmd.Stdin = strings.NewReader(body)
	if _, err := cmd.Output(); err != nil {
		return fmt.Errorf("gh pr edit #%d: %w", pr, ghError(err))
	}
	return nil
}

// splitRepo splits "owner/name" into its parts.
func splitRepo(repo string) (owner, name string, err error) {
	for i := 0; i < len(repo); i++ {
		if repo[i] == '/' {
			return repo[:i], repo[i+1:], nil
		}
	}
	return "", "", fmt.Errorf("repo must be owner/name, got %q", repo)
}

// ghError enriches an exec error with gh's stderr when available.
func ghError(err error) error {
	var ee *exec.ExitError
	if errors.As(err, &ee) && len(ee.Stderr) > 0 {
		return fmt.Errorf("%w: %s", err, string(ee.Stderr))
	}
	return err
}
