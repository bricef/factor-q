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
	"time"

	"github.com/nats-io/nats.go/jetstream"
)

const reloadSettleEnv = "FQCRON_RELOAD_SETTLE"

const removalConfirmEnv = "FQCRON_REMOVAL_CONFIRM"

type cliConfig struct {
	ConfigPath, NATSURL, KVBucket string
	// HealthBind is the loopback address of GET /healthz (health.go);
	// "" = no endpoint.
	HealthBind string
	// ReloadSettle is the quiet period a changed jobs file must hold
	// before the watcher reads it as final (watch.go).
	ReloadSettle time.Duration
	// RemovalConfirm is how long a job dropped by a reload keeps its fire
	// state before the deletion is carried out (removal.go).
	RemovalConfirm time.Duration
	Check          bool
}

func envOr(key, fallback string) string {
	if v := os.Getenv(key); v != "" {
		return v
	}
	return fallback
}

// envOrDuration is envOr for a duration, with a malformed value an error
// rather than a silent fallback — a mistyped settle must not look applied.
func envOrDuration(key string, fallback time.Duration) (time.Duration, error) {
	v := os.Getenv(key)
	if v == "" {
		return fallback, nil
	}
	d, err := time.ParseDuration(v)
	if err != nil {
		return 0, fmt.Errorf("%s: %w", key, err)
	}
	return d, nil
}

func configFromArgs(args []string) (cliConfig, error) {
	fs := flag.NewFlagSet("fq-cron", flag.ContinueOnError)
	fs.SetOutput(io.Discard)
	var c cliConfig
	settle, err := envOrDuration(reloadSettleEnv, DefaultReloadSettle)
	if err != nil {
		return c, err
	}
	removalConfirm, err := envOrDuration(removalConfirmEnv, DefaultRemovalConfirm)
	if err != nil {
		return c, err
	}
	fs.StringVar(&c.ConfigPath, "config", envOr("FQCRON_CONFIG", ""), "config file (env FQCRON_CONFIG)")
	fs.StringVar(&c.NATSURL, "nats-url", envOr("FQCRON_NATS_URL", "nats://127.0.0.1:4222"), "NATS URL (env FQCRON_NATS_URL)")
	fs.StringVar(&c.KVBucket, "kv-bucket", envOr("FQCRON_KV_BUCKET", "fq-cron-state"), "KV bucket (env FQCRON_KV_BUCKET)")
	fs.StringVar(&c.HealthBind, "health-bind", envOr(healthBindEnv, defaultHealthBind), "loopback address for GET /healthz, the probe the container's HEALTHCHECK runs; empty disables (env "+healthBindEnv+")")
	fs.DurationVar(&c.ReloadSettle, "reload-settle", settle, "quiet period a changed config file must hold before it is reloaded (env "+reloadSettleEnv+")")
	fs.DurationVar(&c.RemovalConfirm, "removal-confirm", removalConfirm, "how long a job dropped by a reload keeps its fire state before it is deleted (env "+removalConfirmEnv+")")
	fs.BoolVar(&c.Check, "check", false, "validate config and exit")
	if err := fs.Parse(args); err != nil {
		return c, err
	}
	if c.ConfigPath == "" {
		return c, fmt.Errorf("--config (or FQCRON_CONFIG) is required")
	}
	// Zero is rejected rather than treated as "no settle": the watcher reads
	// a non-positive settle as "use the default", so accepting 0 here would
	// start a scheduler running at 250 ms while its operator believed the
	// confirming read was off. A setting must never look applied when it is
	// not.
	if c.ReloadSettle <= 0 {
		return c, fmt.Errorf("--reload-settle (or %s) must be greater than zero", reloadSettleEnv)
	}
	// Same reason, same shape: the loop reads a non-positive window as "use
	// the default", so a zero here would run a 60-second confirmation window
	// while its operator believed removals were immediate.
	if c.RemovalConfirm <= 0 {
		return c, fmt.Errorf("--removal-confirm (or %s) must be greater than zero", removalConfirmEnv)
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

// emptyScheduleNotice is what a deliberate empty schedule says at startup.
// `job = []` is now the only way to reach it — anything else that parses to no
// jobs is refused (config.go, jobsUndeclaredError) — and from outside, a
// scheduler running it is indistinguishable from one that is failing to fire.
// So it says so, once, rather than starting in silence (#664).
const emptyScheduleNotice = "config declares no jobs (`job = []`): nothing scheduled until one is added"

// noneEnabledNotice is the same silence reached by the other road: a file full
// of jobs, every one of them `enabled = false`. The planner skips those
// (plan.go), so such a config fires exactly as much as `job = []` — and unlike
// `job = []`, it does not look empty to the operator reading it.
func noneEnabledNotice(declared int) string {
	return fmt.Sprintf("config declares %d job(s), none enabled: nothing scheduled until one is enabled", declared)
}

// announceEmptySchedule says, once at startup, that this configuration will
// fire nothing. A configuration with something to schedule says nothing here:
// the scheduler reports its jobs as it fires them.
func announceEmptySchedule(cfg *Config, logger *log.Logger) {
	switch {
	case len(cfg.Jobs) == 0:
		logger.Print(emptyScheduleNotice)
	case cfg.scheduledJobs() == 0:
		logger.Print(noneEnabledNotice(len(cfg.Jobs)))
	}
}

// checkQualifier is what `--check` adds to "is valid" for a file that will
// schedule nothing. Valid and useful are different questions, and the operator
// running `--check` is the one who can still do something about the answer
// (#664).
func checkQualifier(cfg *Config) string {
	switch {
	case len(cfg.Jobs) == 0:
		return " (no jobs declared)"
	case cfg.scheduledJobs() == 0:
		return fmt.Sprintf(" (%d job(s) declared, none enabled)", len(cfg.Jobs))
	}
	return ""
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
	// The bytes come back with the parsed config so the watcher can be seeded
	// from them below, after the broker wait, rather than re-reading the file
	// there and missing anything written in between (#634).
	loaded, err := LoadConfig(cli.ConfigPath)
	if err != nil {
		return err
	}
	if cli.Check {
		fmt.Printf("configuration %s is valid%s\n", cli.ConfigPath, checkQualifier(loaded.Config))
		return nil
	}
	announceEmptySchedule(loaded.Config, log.Default())

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
	watcher := NewConfigWatcher(cli.ConfigPath, loaded, ConfigWatcherOptions{Settle: cli.ReloadSettle, Logger: log.Default()})
	// Recheck is the watcher's own Check: at a parked removal's deadline the
	// loop takes one last look at the file, so a write that completed since
	// the reload that dropped the job is seen before its state is deleted —
	// and judges the job's absence against the watcher's configuration, which
	// the loop's own copy may trail by one undelivered reload.
	removal := removalPolicy{Confirm: cli.RemovalConfirm, Recheck: watcher.Check}
	return runScheduler(ctx, loaded.Config, watcher.Run(ctx), publisher, store, removal, log.Default())
}

func main() {
	if err := run(os.Args[1:]); err != nil {
		fmt.Fprintln(os.Stderr, "fq-cron:", err)
		os.Exit(1)
	}
}
