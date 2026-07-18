package main

import (
	"context"
	"flag"
	"fmt"
	"io"
	"log"
	"os"
	"os/signal"
	"syscall"

	"github.com/nats-io/nats.go"
	"github.com/nats-io/nats.go/jetstream"
)

type cliConfig struct {
	ConfigPath, NATSURL, KVBucket string
	Check                         bool
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
	fs.BoolVar(&c.Check, "check", false, "validate config and exit")
	if err := fs.Parse(args); err != nil {
		return c, err
	}
	if c.ConfigPath == "" {
		return c, fmt.Errorf("--config (or FQCRON_CONFIG) is required")
	}
	return c, nil
}

func run(args []string) error {
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
	nc, err := nats.Connect(cli.NATSURL)
	if err != nil {
		return fmt.Errorf("connect to NATS: %w", err)
	}
	defer nc.Close()
	publisher, err := NewNATSPublisher(nc)
	if err != nil {
		return err
	}
	js, err := jetstream.New(nc)
	if err != nil {
		return fmt.Errorf("create JetStream context: %w", err)
	}
	store, err := NewKVStateStore(ctx, js, cli.KVBucket)
	if err != nil {
		return err
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
