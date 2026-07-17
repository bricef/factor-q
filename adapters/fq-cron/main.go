package main

import (
	"flag"
	"fmt"
	"io"
	"os"
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
	cfg, err := configFromArgs(args)
	if err != nil {
		return err
	}
	if _, err := LoadConfig(cfg.ConfigPath); err != nil {
		return err
	}
	if cfg.Check {
		fmt.Printf("configuration %s is valid\n", cfg.ConfigPath)
		return nil
	}
	return fmt.Errorf("scheduling is not implemented; use --check")
}

func main() {
	if err := run(os.Args[1:]); err != nil {
		fmt.Fprintln(os.Stderr, "fq-cron:", err)
		os.Exit(1)
	}
}
