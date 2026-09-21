package main

import (
	"context"
	"encoding/json"
	"fmt"
	"strings"

	"github.com/google/go-github/v76/github"
)

// The MergeVerdictSource half of the GitHub client (issue #879). It lives
// beside the sweep rather than in github.go for the reason the merge
// policy gives: everything the verdict is computed from is itself in the
// `merge-verdicts` area, so the file glob that makes the sweep `never`
// covers the code that feeds it too.

// ListOpenPullRequests returns every open PR, cheaply — number, head SHA,
// base ref and body. The body is what says whether a PR is the fleet's
// (a watcher footer or agent-written provenance line), so the sweep can filter before spending a
// request on anyone else's PR.
func (g *GhCliIssueSource) ListOpenPullRequests(ctx context.Context) ([]PullRequest, error) {
	owner, repo, err := splitRepo(g.Repo)
	if err != nil {
		return nil, err
	}
	opts := &github.PullRequestListOptions{State: "open", ListOptions: github.ListOptions{PerPage: 100}}
	var out []PullRequest
	for {
		page, resp, err := g.Client.PullRequests.List(ctx, owner, repo, opts)
		if err != nil {
			return nil, fmt.Errorf("list open PRs: %w", err)
		}
		for _, pr := range page {
			out = append(out, PullRequest{
				Number:  pr.GetNumber(),
				HeadSHA: pr.GetHead().GetSHA(),
				BaseRef: pr.GetBase().GetRef(),
				Body:    pr.GetBody(),
			})
		}
		if resp.NextPage == 0 {
			return out, nil
		}
		opts.Page = resp.NextPage
	}
}

// PRFactsFor gathers the full facts for one PR: the REST detail (which is
// the only place `mergeable_state` exists), the changed-file list, and one
// GraphQL call for the two things REST answers badly — GitHub's own
// review decision, and the issues this PR closes with the labels they
// carry.
func (g *GhCliIssueSource) PRFactsFor(ctx context.Context, pr PullRequest) (PRFacts, error) {
	owner, repo, err := splitRepo(g.Repo)
	if err != nil {
		return PRFacts{}, err
	}
	detail, _, err := g.Client.PullRequests.Get(ctx, owner, repo, pr.Number)
	if err != nil {
		return PRFacts{}, fmt.Errorf("get PR #%d: %w", pr.Number, err)
	}
	files, err := g.prFiles(ctx, owner, repo, pr.Number)
	if err != nil {
		return PRFacts{}, err
	}
	decision, closing, err := g.prReviewAndClosingIssues(ctx, pr.Number)
	if err != nil {
		return PRFacts{}, err
	}
	labels := make([]string, 0, len(detail.Labels))
	for _, l := range detail.Labels {
		labels = append(labels, l.GetName())
	}
	return PRFacts{
		Number:         pr.Number,
		Title:          detail.GetTitle(),
		HeadBranch:     detail.GetHead().GetRef(),
		HeadSHA:        detail.GetHead().GetSHA(),
		BaseRef:        detail.GetBase().GetRef(),
		Body:           detail.GetBody(),
		Files:          files,
		CommitCount:    detail.GetCommits(),
		Labels:         labels,
		MergeableState: detail.GetMergeableState(),
		ChangesRequest: decision == "CHANGES_REQUESTED",
		CreatedAt:      detail.GetCreatedAt().Time,
		ClosingIssues:  closing,
		Additions:      detail.GetAdditions(),
		Deletions:      detail.GetDeletions(),
	}, nil
}

func (g *GhCliIssueSource) prFiles(ctx context.Context, owner, repo string, number int) ([]string, error) {
	opts := &github.ListOptions{PerPage: 100}
	var files []string
	for {
		page, resp, err := g.Client.PullRequests.ListFiles(ctx, owner, repo, number, opts)
		if err != nil {
			return nil, fmt.Errorf("list files of PR #%d: %w", number, err)
		}
		for _, f := range page {
			files = append(files, f.GetFilename())
		}
		if resp.NextPage == 0 {
			return files, nil
		}
		opts.Page = resp.NextPage
	}
}

// prFactsQuery asks for the review decision GitHub itself computes (so
// the watcher does not re-derive "which review is still standing" from a
// list) and the issues this PR closes, with their labels.
const prFactsQuery = `query($owner:String!,$repo:String!,$number:Int!){
  repository(owner:$owner,name:$repo){ pullRequest(number:$number){ reviewDecision closingIssuesReferences(first:20){ nodes{ number title body labels(first:50){ nodes{ name } } } } } }
}`

type ghPRFactsResponse struct {
	Data struct {
		Repository struct {
			PullRequest struct {
				ReviewDecision          string `json:"reviewDecision"`
				ClosingIssuesReferences struct {
					Nodes []struct {
						Number int    `json:"number"`
						Title  string `json:"title"`
						Body   string `json:"body"`
						Labels struct {
							Nodes []struct {
								Name string `json:"name"`
							} `json:"nodes"`
						} `json:"labels"`
					} `json:"nodes"`
				} `json:"closingIssuesReferences"`
			} `json:"pullRequest"`
		} `json:"repository"`
	} `json:"data"`
}

func (g *GhCliIssueSource) prReviewAndClosingIssues(ctx context.Context, number int) (string, []ClosingIssue, error) {
	raw, err := g.graphQL(ctx, prFactsQuery, number)
	if err != nil {
		return "", nil, err
	}
	return parsePRFactsResponse(raw)
}

// parsePRFactsResponse is split out so it is unit-testable without a server.
func parsePRFactsResponse(raw []byte) (string, []ClosingIssue, error) {
	var resp ghPRFactsResponse
	if err := json.Unmarshal(raw, &resp); err != nil {
		return "", nil, fmt.Errorf("parse GitHub GraphQL response: %w", err)
	}
	pr := resp.Data.Repository.PullRequest
	closing := make([]ClosingIssue, 0, len(pr.ClosingIssuesReferences.Nodes))
	for _, n := range pr.ClosingIssuesReferences.Nodes {
		labels := make([]string, 0, len(n.Labels.Nodes))
		for _, l := range n.Labels.Nodes {
			labels = append(labels, l.Name)
		}
		closing = append(closing, ClosingIssue{Number: n.Number, Labels: labels, Title: n.Title, Body: n.Body})
	}
	return pr.ReviewDecision, closing, nil
}

// RepoFileAtRef reads a tracked file at a git ref. The sweep calls it with
// the PR's BASE ref: the policy that judges a PR must be the policy on the
// branch it is merging into, never the one in the PR.
func (g *GhCliIssueSource) RepoFileAtRef(ctx context.Context, path, ref string) ([]byte, error) {
	owner, repo, err := splitRepo(g.Repo)
	if err != nil {
		return nil, err
	}
	file, _, _, err := g.Client.Repositories.GetContents(ctx, owner, repo, path, &github.RepositoryContentGetOptions{Ref: ref})
	if err != nil {
		return nil, fmt.Errorf("get %s@%s: %w", path, ref, err)
	}
	if file == nil {
		return nil, fmt.Errorf("get %s@%s: not a file", path, ref)
	}
	content, err := file.GetContent()
	if err != nil {
		return nil, fmt.Errorf("decode %s@%s: %w", path, ref, err)
	}
	return []byte(content), nil
}

// RepoLabels returns the labels that exist in the repository.
func (g *GhCliIssueSource) RepoLabels(ctx context.Context) ([]string, error) {
	owner, repo, err := splitRepo(g.Repo)
	if err != nil {
		return nil, err
	}
	opts := &github.ListOptions{PerPage: 100}
	var names []string
	for {
		page, resp, err := g.Client.Issues.ListLabels(ctx, owner, repo, opts)
		if err != nil {
			return nil, fmt.Errorf("list labels: %w", err)
		}
		for _, l := range page {
			names = append(names, l.GetName())
		}
		if resp.NextPage == 0 {
			return names, nil
		}
		opts.Page = resp.NextPage
	}
}

// SetPRLabels applies the verdict label and clears the other two. The add
// runs first, so a PR is never momentarily unlabelled, and a removal that
// 404s is the label simply not being there — the common case, and not an
// error.
func (g *GhCliIssueSource) SetPRLabels(ctx context.Context, pr int, add string, remove []string) error {
	owner, repo, err := splitRepo(g.Repo)
	if err != nil {
		return err
	}
	if _, _, err := g.Client.Issues.AddLabelsToIssue(ctx, owner, repo, pr, []string{add}); err != nil {
		return fmt.Errorf("add %q to #%d: %w", add, pr, err)
	}
	for _, label := range remove {
		if _, err := g.Client.Issues.RemoveLabelForIssue(ctx, owner, repo, pr, label); err != nil && !isNotFound(err) {
			return fmt.Errorf("remove %q from #%d: %w", label, pr, err)
		}
	}
	return nil
}

// RemovePRLabel removes one label, tolerating its absence — the common
// case when a rubric flag was never set.
func (g *GhCliIssueSource) RemovePRLabel(ctx context.Context, pr int, label string) error {
	owner, repo, err := splitRepo(g.Repo)
	if err != nil {
		return err
	}
	if _, err := g.Client.Issues.RemoveLabelForIssue(ctx, owner, repo, pr, label); err != nil && !isNotFound(err) {
		return fmt.Errorf("remove %q from #%d: %w", label, pr, err)
	}
	return nil
}

// FindPRComment returns the PR's first comment containing marker — the
// sweep's own, which it then edits in place.
func (g *GhCliIssueSource) FindPRComment(ctx context.Context, pr int, marker string) (PRComment, bool, error) {
	owner, repo, err := splitRepo(g.Repo)
	if err != nil {
		return PRComment{}, false, err
	}
	opts := &github.IssueListCommentsOptions{ListOptions: github.ListOptions{PerPage: 100}}
	for {
		page, resp, err := g.Client.Issues.ListComments(ctx, owner, repo, pr, opts)
		if err != nil {
			return PRComment{}, false, fmt.Errorf("list comments on #%d: %w", pr, err)
		}
		for _, c := range page {
			if strings.Contains(c.GetBody(), marker) {
				return PRComment{ID: c.GetID(), Body: c.GetBody()}, true, nil
			}
		}
		if resp.NextPage == 0 {
			return PRComment{}, false, nil
		}
		opts.Page = resp.NextPage
	}
}

// CreatePRComment posts the verdict comment for the first time.
func (g *GhCliIssueSource) CreatePRComment(ctx context.Context, pr int, body string) error {
	owner, repo, err := splitRepo(g.Repo)
	if err != nil {
		return err
	}
	if _, _, err := g.Client.Issues.CreateComment(ctx, owner, repo, pr, &github.IssueComment{Body: github.Ptr(body)}); err != nil {
		return fmt.Errorf("comment on #%d: %w", pr, err)
	}
	return nil
}

// UpdatePRComment rewrites the verdict comment in place.
func (g *GhCliIssueSource) UpdatePRComment(ctx context.Context, id int64, body string) error {
	owner, repo, err := splitRepo(g.Repo)
	if err != nil {
		return err
	}
	if _, _, err := g.Client.Issues.EditComment(ctx, owner, repo, id, &github.IssueComment{Body: github.Ptr(body)}); err != nil {
		return fmt.Errorf("edit comment %d: %w", id, err)
	}
	return nil
}
