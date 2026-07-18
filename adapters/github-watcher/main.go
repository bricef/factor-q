package main

import (
	"context"
	"flag"
	"fmt"
	"log/slog"
	"os"
	"os/signal"
	"runtime/debug"
	"strconv"
	"strings"
	"syscall"
	"time"
)

func main() {
	if err := run(os.Args[1:]); err != nil {
		fmt.Fprintln(os.Stderr, "github-watcher:", err)
		os.Exit(1)
	}
}

// buildVersion returns the git revision this binary was built from,
// read from the VCS info Go embeds by default when building inside a
// git tree (`-buildvcs`). Degrades to "unknown" when unavailable (e.g.
// a build outside version control). A "-dirty" suffix marks an
// uncommitted working tree — the same convention as `fq`.
func buildVersion() string {
	info, ok := debug.ReadBuildInfo()
	if !ok {
		return "unknown"
	}
	rev, modified := "", ""
	for _, s := range info.Settings {
		switch s.Key {
		case "vcs.revision":
			rev = s.Value
		case "vcs.modified":
			modified = s.Value
		}
	}
	if rev == "" {
		return "unknown"
	}
	if len(rev) > 12 {
		rev = rev[:12]
	}
	if modified == "true" {
		rev += "-dirty"
	}
	return rev
}

func run(args []string) error {
	for _, a := range args {
		if a == "-version" || a == "--version" {
			fmt.Println("github-watcher", buildVersion())
			return nil
		}
	}
	cfg, natsURL, err := configFromArgs(args)
	if err != nil {
		return err
	}
	log := slog.New(slog.NewTextHandler(os.Stderr, nil))

	pub, err := NewNatsTriggerPublisher(natsURL)
	if err != nil {
		return err
	}
	defer pub.Close()

	source, err := NewGhCliIssueSource(cfg.Repo)
	if err != nil {
		return err
	}
	w := &Watcher{
		Source:    source,
		Publisher: pub,
		Reviewer:  source,
		Config:    cfg,
		Log:       log,
	}

	ctx, stop := signal.NotifyContext(context.Background(), syscall.SIGINT, syscall.SIGTERM)
	defer stop()

	// Observe the outcomes of what we trigger: subscribe to the agent's
	// lifecycle events and drive the issue off `in-progress` on
	// completion/failure. Runs alongside the poll loop; a subscription
	// error ends its goroutine but does not stop polling (the review
	// sweep is the backstop for missed events).
	reactor := NewOutcomeReactor(source, cfg, log)
	// Stamp invocation provenance on the PR the agent opened (issue
	// #162) — same gh-backed source, optional and best-effort.
	reactor.Stamper = source
	outcomes := NewNatsOutcomeSource(pub.Conn(), cfg.TaskTemplate, log)
	go func() {
		if err := reactor.Run(ctx, outcomes); err != nil && ctx.Err() == nil {
			log.Error("outcome observer stopped", "err", err)
		}
	}()

	// A cancelled context is a clean stop, not an error.
	if err := w.Run(ctx); err != nil && ctx.Err() == nil {
		return err
	}
	return nil
}

// configFromArgs parses config from flags, each with an env fallback.
// Returns the Config and the NATS URL.
func configFromArgs(args []string) (Config, string, error) {
	fs := flag.NewFlagSet("github-watcher", flag.ContinueOnError)
	pollDefault, err := envDurationOr("GHW_POLL", 60*time.Second)
	if err != nil {
		return Config{}, "", err
	}
	maxPerPollDefault, err := envIntOr("GHW_MAX_PER_POLL", 3)
	if err != nil {
		return Config{}, "", err
	}
	maxRetriesDefault, err := envIntOr("GHW_MAX_RETRIES", 2)
	if err != nil {
		return Config{}, "", err
	}
	repo := fs.String("repo", envOr("GHW_REPO", ""), "GitHub repo owner/name (env GHW_REPO)")
	agent := fs.String("agent", envOr("GHW_AGENT", "m0-issue-fix"), "target factor-q agent id (env GHW_AGENT)")
	natsURL := fs.String("nats-url", envOr("GHW_NATS_URL", "nats://127.0.0.1:4222"), "NATS URL (env GHW_NATS_URL)")
	ready := fs.String("ready-label", envOr("GHW_READY_LABEL", "status:ready"), "label that triggers (env GHW_READY_LABEL)")
	inProgress := fs.String("in-progress-label", envOr("GHW_IN_PROGRESS_LABEL", "status:in-progress"), "label applied on trigger (env GHW_IN_PROGRESS_LABEL)")
	inReview := fs.String("in-review-label", envOr("GHW_IN_REVIEW_LABEL", "status:in-review"), "label applied when the agent completes / opens its PR (env GHW_IN_REVIEW_LABEL)")
	failed := fs.String("failed-label", envOr("GHW_FAILED_LABEL", "status:failed"), "label applied when retries are exhausted or the failure is terminal (env GHW_FAILED_LABEL)")
	done := fs.String("done-label", envOr("GHW_DONE_LABEL", "status:done"), "label applied when the proposed PR merges (env GHW_DONE_LABEL)")
	poll := fs.Duration("poll", pollDefault, "poll interval, >= 60s (env GHW_POLL)")
	maxPerPoll := fs.Int("max-per-poll", maxPerPollDefault, "max triggers per poll, 0 = unbounded (env GHW_MAX_PER_POLL)")
	maxRetries := fs.Int("max-retries", maxRetriesDefault, "bounded auto-retry budget for a transiently-failed issue (env GHW_MAX_RETRIES)")
	template := fs.String("task-template", envOr("GHW_TASK_TEMPLATE", "Implement the fix described in GitHub issue #%d."), "trigger payload template; %d is the issue number (env GHW_TASK_TEMPLATE)")

	if err := fs.Parse(args); err != nil {
		return Config{}, "", err
	}
	if *repo == "" {
		return Config{}, "", fmt.Errorf("--repo (or GHW_REPO) is required, e.g. bricef/factor-q")
	}
	if !strings.Contains(*repo, "/") {
		return Config{}, "", fmt.Errorf("--repo must be owner/name, got %q", *repo)
	}
	if *poll < MinPollInterval {
		return Config{}, "", fmt.Errorf("--poll must be >= %s to respect GitHub rate limits, got %s", MinPollInterval, *poll)
	}
	if *maxRetries < 0 {
		return Config{}, "", fmt.Errorf("--max-retries must be >= 0, got %d", *maxRetries)
	}
	if err := validateTaskTemplate(*template); err != nil {
		return Config{}, "", err
	}
	return Config{
		Repo:               *repo,
		TargetAgent:        *agent,
		ReadyLabel:         *ready,
		InProgressLabel:    *inProgress,
		InReviewLabel:      *inReview,
		FailedLabel:        *failed,
		DoneLabel:          *done,
		PollInterval:       *poll,
		MaxTriggersPerPoll: *maxPerPoll,
		MaxRetries:         *maxRetries,
		TaskTemplate:       *template,
	}, *natsURL, nil
}

func envOr(key, def string) string {
	if v := os.Getenv(key); v != "" {
		return v
	}
	return def
}

func envIntOr(key string, def int) (int, error) {
	v := os.Getenv(key)
	if v == "" {
		return def, nil
	}
	n, err := strconv.Atoi(v)
	if err != nil {
		return 0, fmt.Errorf("invalid %s=%q: expected integer: %w", key, v, err)
	}
	return n, nil
}

func envDurationOr(key string, def time.Duration) (time.Duration, error) {
	v := os.Getenv(key)
	if v == "" {
		return def, nil
	}
	d, err := time.ParseDuration(v)
	if err != nil {
		return 0, fmt.Errorf("invalid %s=%q: expected duration: %w", key, v, err)
	}
	return d, nil
}

func validateTaskTemplate(template string) error {
	placeholders := 0
	for i := 0; i < len(template); i++ {
		if template[i] != '%' {
			continue
		}
		if i+1 >= len(template) {
			return fmt.Errorf("--task-template has invalid format verb in %q", template)
		}
		i++
		switch template[i] {
		case '%':
		case 'd':
			placeholders++
		default:
			return fmt.Errorf("--task-template may only contain one %%d verb, got %q", template)
		}
	}
	if placeholders != 1 {
		return fmt.Errorf("--task-template must contain exactly one %%d for the issue number, got %q", template)
	}
	return nil
}
