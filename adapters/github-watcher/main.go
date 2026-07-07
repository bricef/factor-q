package main

import (
	"context"
	"flag"
	"fmt"
	"log/slog"
	"os"
	"os/signal"
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

func run(args []string) error {
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

	w := &Watcher{
		Source:    &GhCliIssueSource{Repo: cfg.Repo},
		Publisher: pub,
		Config:    cfg,
		Log:       log,
	}

	ctx, stop := signal.NotifyContext(context.Background(), syscall.SIGINT, syscall.SIGTERM)
	defer stop()

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
	repo := fs.String("repo", envOr("GHW_REPO", ""), "GitHub repo owner/name (env GHW_REPO)")
	agent := fs.String("agent", envOr("GHW_AGENT", "m0-issue-fix"), "target factor-q agent id (env GHW_AGENT)")
	natsURL := fs.String("nats-url", envOr("GHW_NATS_URL", "nats://127.0.0.1:4222"), "NATS URL (env GHW_NATS_URL)")
	ready := fs.String("ready-label", envOr("GHW_READY_LABEL", "ready"), "label that triggers (env GHW_READY_LABEL)")
	inProgress := fs.String("in-progress-label", envOr("GHW_IN_PROGRESS_LABEL", "in-progress"), "label applied on trigger (env GHW_IN_PROGRESS_LABEL)")
	poll := fs.Duration("poll", envDurationOr("GHW_POLL", 60*time.Second), "poll interval, >= 60s (env GHW_POLL)")
	maxPerPoll := fs.Int("max-per-poll", envIntOr("GHW_MAX_PER_POLL", 3), "max triggers per poll, 0 = unbounded (env GHW_MAX_PER_POLL)")
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
	if !strings.Contains(*template, "%d") {
		return Config{}, "", fmt.Errorf("--task-template must contain %%d for the issue number, got %q", *template)
	}
	return Config{
		Repo:               *repo,
		TargetAgent:        *agent,
		ReadyLabel:         *ready,
		InProgressLabel:    *inProgress,
		PollInterval:       *poll,
		MaxTriggersPerPoll: *maxPerPoll,
		TaskTemplate:       *template,
	}, *natsURL, nil
}

func envOr(key, def string) string {
	if v := os.Getenv(key); v != "" {
		return v
	}
	return def
}

func envIntOr(key string, def int) int {
	if v := os.Getenv(key); v != "" {
		if n, err := strconv.Atoi(v); err == nil {
			return n
		}
	}
	return def
}

func envDurationOr(key string, def time.Duration) time.Duration {
	if v := os.Getenv(key); v != "" {
		if d, err := time.ParseDuration(v); err == nil {
			return d
		}
	}
	return def
}
