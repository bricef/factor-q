package main

import (
	"context"
	"flag"
	"fmt"
	"io"
	"log"
	"os"
	"os/signal"
	"runtime/debug"
	"syscall"

	"github.com/nats-io/nats.go/jetstream"
)

type cliConfig struct {
	ConfigPath, NATSURL, KVBucket string
	// HealthBind is the loopback address of GET /healthz (health.go);
	// "" = no endpoint.
	HealthBind string
	Check      bool
}

func envOr(key, fallback string) string {
	if v := os.Getenv(key); v != "" {
		return v
	}
	return fallback
}

func configFromArgs(args []string) (cliConfig, error) {
	fs := flag.NewFlagSet("fq-cron", flag.ContinueOnError)
	fs.SetOutput(io.Discard)
	var c cliConfig
	fs.StringVar(&c.ConfigPath, "config", envOr("FQCRON_CONFIG", ""), "config file (env FQCRON_CONFIG)")
	fs.StringVar(&c.NATSURL, "nats-url", envOr("FQCRON_NATS_URL", "nats://127.0.0.1:4222"), "NATS URL (env FQCRON_NATS_URL)")
	fs.StringVar(&c.KVBucket, "kv-bucket", envOr("FQCRON_KV_BUCKET", "fq-cron-state"), "KV bucket (env FQCRON_KV_BUCKET)")
	fs.StringVar(&c.HealthBind, "health-bind", envOr(healthBindEnv, defaultHealthBind), "loopback address for GET /healthz, the probe the container's HEALTHCHECK runs; empty disables (env "+healthBindEnv+")")
	fs.BoolVar(&c.Check, "check", false, "validate config and exit")
	if err := fs.Parse(args); err != nil {
		return c, err
	}
	if c.ConfigPath == "" {
		return c, fmt.Errorf("--config (or FQCRON_CONFIG) is required")
	}
	return c, nil
}

// buildVersion returns the git revision this binary was built from,
// read from the VCS info Go embeds by default when building inside a
// git tree (`-buildvcs`). Degrades to "unknown" when unavailable (e.g.
// a build outside version control). A "-dirty" suffix marks an
// uncommitted working tree — the same convention as `fq` and the
// watcher, so a deploy can check every binary in a bundle reports one
// commit (ops/dogfood/deploy.sh, `just docker-check`).
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
	// Answered before flag parsing, like the watcher: --version must
	// work without a --config, and the flag set would otherwise reject
	// it as undefined.
	for _, a := range args {
		if a == "-version" || a == "--version" {
			fmt.Println("fq-cron", buildVersion())
			return nil
		}
		// The container's HEALTHCHECK: ask the running process, from the
		// same environment, and exit 0 on healthy (health.go). Answered
		// here for the same reason as --version: it needs no --config.
		if a == "-probe" || a == "--probe" {
			return probe(envOr(healthBindEnv, defaultHealthBind))
		}
	}
	cli, err := configFromArgs(args)
	if err != nil {
		return err
	}
	config, err := LoadConfig(cli.ConfigPath)
	if err != nil {
		return err
	}
	if cli.Check {
		fmt.Printf("configuration %s is valid\n", cli.ConfigPath)
		return nil
	}

	ctx, stop := signal.NotifyContext(context.Background(), os.Interrupt, syscall.SIGTERM)
	defer stop()
	nc, err := connectNATS(cli.NATSURL, log.Default())
	if err != nil {
		return err
	}
	defer nc.Close()
	// Liveness for the supervisor: the broker connection (health.go).
	// Bound before the scheduler starts, so a taken port is a startup
	// error rather than a process that shows unhealthy for ever — and
	// before the broker wait below, so a scheduler waiting out an outage
	// answers the probe (503, "nats: RECONNECTING") instead of refusing
	// the connection.
	if cli.HealthBind != "" {
		ln, err := listenHealth(cli.HealthBind)
		if err != nil {
			return err
		}
		health := NewHealth(nc, 0)
		go func() {
			if err := serveHealth(ctx, ln, health, log.Default()); err != nil && ctx.Err() == nil {
				log.Printf("health endpoint stopped: %v", err)
			}
		}()
	}
	// A broker that is not up yet is a wait, not a startup failure: the
	// deploy launches every process at once and only the daemon waits for
	// the broker's health endpoint.
	if err := waitConnected(ctx, nc, log.Default()); err != nil {
		if ctx.Err() != nil {
			return nil // signalled while waiting: a clean stop
		}
		return err
	}
	publisher, err := NewNATSPublisher(nc)
	if err != nil {
		return err
	}
	js, err := jetstream.New(nc)
	if err != nil {
		return fmt.Errorf("create JetStream context: %w", err)
	}
	// waitConnected only proves the broker was there a moment ago: it can
	// drop again between that check and this call, which is the same
	// JetStream request the loop retries for ever once it is running.
	// Retry it here too, or the startup window stays the one place a
	// broker blip still ends the process.
	var store *KVStateStore
	if err := withBrokerRetry(ctx, log.Default(), "open state bucket", func() error {
		var err error
		store, err = NewKVStateStore(ctx, js, cli.KVBucket)
		return err
	}); err != nil {
		return nil // ctx cancelled: a clean stop
	}
	watcher := NewConfigWatcher(cli.ConfigPath, config, ConfigWatcherOptions{Logger: log.Default()})
	return runScheduler(ctx, config, watcher.Run(ctx), publisher, store, log.Default())
}

func main() {
	if err := run(os.Args[1:]); err != nil {
		fmt.Fprintln(os.Stderr, "fq-cron:", err)
		os.Exit(1)
	}
}
