package main

import (
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"os/exec"
	"strconv"
)

// GhCliIssueSource implements IssueSource by shelling out to the `gh` CLI,
// reusing the operator's existing GitHub auth. `gh` must be on PATH and
// authenticated (e.g. via GH_TOKEN).
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
	cmd := exec.CommandContext(ctx, "gh", "issue", "list",
		"--repo", g.Repo,
		"--label", readyLabel,
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

// ghError enriches an exec error with gh's stderr when available.
func ghError(err error) error {
	var ee *exec.ExitError
	if errors.As(err, &ee) && len(ee.Stderr) > 0 {
		return fmt.Errorf("%w: %s", err, string(ee.Stderr))
	}
	return err
}
