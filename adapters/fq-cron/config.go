package main

import (
	"encoding/json"
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

func LoadConfig(path string) (*Config, error) {
	data, err := os.ReadFile(path)
	if err != nil {
		return nil, fmt.Errorf("read config: %w", err)
	}
	return ParseConfig(data)
}

func ParseConfig(data []byte) (*Config, error) {
	var cfg Config
	if _, err := toml.Decode(string(data), &cfg); err != nil {
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
