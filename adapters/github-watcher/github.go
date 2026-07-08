package main

import (
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"os/exec"
	"strconv"
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
	if err := cmd.Run(); err != nil {
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
