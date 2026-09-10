package main

import (
	"encoding/json"
	"errors"
	"fmt"
	"os"
	"regexp"
	"strings"
	"time"

	"github.com/BurntSushi/toml"
	"github.com/robfig/cron/v3"
)

const DefaultMaxFiresPerHour = 120

type Config struct {
	Limits   Limits   `toml:"limits"`
	Defaults Defaults `toml:"defaults"`
	Jobs     []Job    `toml:"job"`
}

type Limits struct {
	// MaxFiresPerHour is the sliding-window ceiling on *fires* across
	// every job in the file — not on jobs, so one runaway schedule trips
	// it on its own. See DESIGN.md D6.
	MaxFiresPerHour int `toml:"max_fires_per_hour"`
}
type Defaults struct {
	TZ      string `toml:"tz"`
	CatchUp string `toml:"catch_up"`
	Durable *bool  `toml:"durable"`
}
type Job struct {
	Name        string         `toml:"name"`
	Schedule    string         `toml:"schedule"`
	Subject     string         `toml:"subject"`
	TZ          string         `toml:"tz"`
	CatchUp     string         `toml:"catch_up"`
	Durable     *bool          `toml:"durable"`
	Enabled     *bool          `toml:"enabled"`
	Payload     map[string]any `toml:"payload"`
	PayloadJSON *string        `toml:"payload_json"`
}

func boolPtr(v bool) *bool { return &v }

var namePattern = regexp.MustCompile(`^[a-z0-9][a-z0-9-]*$`)
var cronParser = cron.NewParser(cron.Minute | cron.Hour | cron.Dom | cron.Month | cron.Dow | cron.Descriptor)

// LoadedConfig is a parsed configuration together with the exact bytes it was
// parsed from. The two travel as one value so that whatever is handed the
// running configuration can also be handed the content that produced it,
// instead of reading the file a second time and getting whatever is there by
// then (https://github.com/bricef/factor-q/issues/634).
type LoadedConfig struct {
	Config *Config
	// Raw is the file's content at the moment it was read. Nil means "no
	// bytes known" — see NewConfigWatcher, which treats that as having seen
	// nothing yet.
	Raw []byte
}

// jobsUndeclaredError refuses a file that came out with no jobs in it and
// never said it meant to. `job = []` is the one way a config says "no jobs"
// out loud; everything else that parses empty — zero bytes, whitespace,
// comments alone, a `[limits]` header alone — is far more likely a read that
// landed between a writer's truncate and its write, which TOML parses as a
// perfectly valid config with every job gone
// (https://github.com/bricef/factor-q/issues/623).
//
// One rule, one sentence, one place: ParseConfig applies it, so the two paths
// that read a config cannot drift apart the way they had
// (https://github.com/bricef/factor-q/issues/664). A reload logs the sentence
// as it stands ("config reload rejected: …", watch.go); startup and `--check`
// name the file and say what to write instead (LoadConfig). Neither can skip
// it, because neither can obtain a *Config without going through here.
type jobsUndeclaredError struct{ Bytes int }

func (e jobsUndeclaredError) Error() string {
	return fmt.Sprintf("%d bytes declaring no jobs, and no explicit `job = []`", e.Bytes)
}

func LoadConfig(path string) (*LoadedConfig, error) {
	data, err := os.ReadFile(path)
	if err != nil {
		return nil, fmt.Errorf("read config: %w", err)
	}
	cfg, err := ParseConfig(data)
	if err != nil {
		// The reason a reload logs is, here, the reason the process will not
		// come up — so it carries the file it is about and the line that
		// makes it go away. An operator running `--check` has the file in
		// front of them; a scheduler that starts on it fires nothing (#664).
		var undeclared jobsUndeclaredError
		if errors.As(err, &undeclared) {
			return nil, fmt.Errorf("%s: %w — write `job = []` to run with nothing scheduled", path, err)
		}
		return nil, err
	}
	return &LoadedConfig{Config: cfg, Raw: data}, nil
}

// ParseConfig parses and validates one configuration file's bytes. A config
// that declares no jobs without saying so is refused here rather than by each
// caller: see jobsUndeclaredError.
func ParseConfig(data []byte) (*Config, error) {
	var cfg Config
	meta, err := toml.Decode(string(data), &cfg)
	if err != nil {
		return nil, fmt.Errorf("parse TOML: %w", err)
	}
	cfg.applyDefaults()
	if err := cfg.Validate(); err != nil {
		// TOML metadata does not retain a table's declaration position, so add
		// the matching name field's line when validation identifies a job.
		for line, text := range strings.Split(string(data), "\n") {
			for _, job := range cfg.Jobs {
				if strings.Contains(err.Error(), fmt.Sprintf("job %q", job.Name)) && strings.TrimSpace(text) == fmt.Sprintf("name = %q", job.Name) {
					return nil, fmt.Errorf("line %d: %w", line+1, err)
				}
			}
		}
		return nil, err
	}
	// Last, so a file with a real mistake in it is told about the mistake:
	// whether the top-level `job` key is *declared* is all that separates a
	// deliberate "no jobs any more" from a file that merely happens to hold
	// none.
	if len(cfg.Jobs) == 0 && !meta.IsDefined("job") {
		return nil, jobsUndeclaredError{Bytes: len(data)}
	}
	return &cfg, nil
}

func (c *Config) applyDefaults() {
	if c.Limits.MaxFiresPerHour == 0 {
		c.Limits.MaxFiresPerHour = DefaultMaxFiresPerHour
	}
	if c.Defaults.TZ == "" {
		c.Defaults.TZ = "UTC"
	}
	if c.Defaults.CatchUp == "" {
		c.Defaults.CatchUp = "skip"
	}
	if c.Defaults.Durable == nil {
		c.Defaults.Durable = boolPtr(true)
	}
	for i := range c.Jobs {
		j := &c.Jobs[i]
		if j.TZ == "" {
			j.TZ = c.Defaults.TZ
		}
		if j.CatchUp == "" {
			j.CatchUp = c.Defaults.CatchUp
		}
		if j.Durable == nil {
			j.Durable = boolPtr(*c.Defaults.Durable)
		}
		if j.Enabled == nil {
			j.Enabled = boolPtr(true)
		}
	}
}

func (c *Config) Validate() error {
	if c.Limits.MaxFiresPerHour <= 0 {
		return fmt.Errorf("limits.max_fires_per_hour must be greater than zero")
	}
	seen := make(map[string]bool)
	for i := range c.Jobs {
		j := &c.Jobs[i]
		prefix := fmt.Sprintf("job %q", j.Name)
		if len(j.Name) > 64 || !namePattern.MatchString(j.Name) {
			return fmt.Errorf("%s: name must match [a-z0-9][a-z0-9-]* and be at most 64 characters", prefix)
		}
		if seen[j.Name] {
			return fmt.Errorf("%s: duplicate name", prefix)
		}
		seen[j.Name] = true
		if err := validateSchedule(j.Schedule); err != nil {
			return fmt.Errorf("%s: schedule: %w", prefix, err)
		}
		if !validSubject(j.Subject) {
			return fmt.Errorf("%s: subject must be concrete, non-empty, and contain no wildcards or whitespace", prefix)
		}
		if _, err := time.LoadLocation(j.TZ); err != nil {
			return fmt.Errorf("%s: invalid timezone %q: %w", prefix, j.TZ, err)
		}
		if j.CatchUp != "skip" && j.CatchUp != "once" {
			return fmt.Errorf("%s: catch_up must be \"skip\" or \"once\"", prefix)
		}
		if j.Payload != nil && j.PayloadJSON != nil {
			return fmt.Errorf("%s: exactly one of payload and payload_json may be set", prefix)
		}
		if j.PayloadJSON != nil && !json.Valid([]byte(*j.PayloadJSON)) {
			return fmt.Errorf("%s: payload_json is not valid JSON", prefix)
		}
	}
	return nil
}

func validateSchedule(spec string) error {
	if strings.HasPrefix(spec, "@every ") {
		d, err := time.ParseDuration(strings.TrimSpace(strings.TrimPrefix(spec, "@every ")))
		if err != nil {
			return err
		}
		if d < time.Minute || d%time.Minute != 0 {
			return fmt.Errorf("interval must use whole minutes and be at least 1 minute")
		}
	}
	s, err := cronParser.Parse(spec)
	if err != nil {
		return err
	}
	start := time.Date(2000, 1, 1, 0, 0, 0, 0, time.UTC)
	first := s.Next(start)
	second := s.Next(first)
	if first.IsZero() || second.Sub(first) < time.Minute {
		return fmt.Errorf("interval must be at least 1 minute")
	}
	return nil
}

func validSubject(s string) bool {
	if s == "" || strings.ContainsAny(s, "*> \t\r\n") {
		return false
	}
	for _, token := range strings.Split(s, ".") {
		if token == "" {
			return false
		}
	}
	return true
}
