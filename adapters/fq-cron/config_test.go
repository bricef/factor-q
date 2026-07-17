package main

import (
	"strings"
	"testing"
)

const validConfig = `
[[job]]
name = "nightly"
schedule = "@daily"
subject = "fq.trigger.agent"
[job.payload]
task = "run {{job}}"
`

func TestParseConfigDefaults(t *testing.T) {
	cfg, err := ParseConfig([]byte(validConfig))
	if err != nil {
		t.Fatal(err)
	}
	j := cfg.Jobs[0]
	if cfg.Limits.MaxFiresPerHour != 120 || j.TZ != "UTC" || j.CatchUp != "skip" || !*j.Durable || !*j.Enabled {
		t.Fatalf("defaults not applied: %+v", cfg)
	}
}

func TestWorkedExample(t *testing.T) {
	input := `[limits]
max_fires_per_hour = 120
[defaults]
tz = "UTC"
catch_up = "skip"
durable = true
[[job]]
name = "nightly-maintenance"
schedule = "0 2 * * *"
subject = "fq.trigger.m0-maintenance"
catch_up = "once"
[job.payload]
task = "Run at {{scheduled_time}}."
refs = []
constraints = ["Open a PR"]
[[job]]
name = "ops-heartbeat"
schedule = "@every 5m"
subject = "ops.fq-cron.heartbeat"
durable = false
payload_json = '{"source":"fq-cron","slot":"{{scheduled_time}}"}'`
	if _, err := ParseConfig([]byte(input)); err != nil {
		t.Fatal(err)
	}
}

func TestValidationRejects(t *testing.T) {
	cases := map[string]string{
		"bad name":         "name = \"Bad_name\"",
		"long name":        "name = \"" + strings.Repeat("a", 65) + "\"",
		"bad cron":         "schedule = \"not cron\"",
		"seconds":          "schedule = \"@every 30s\"",
		"second precision": "schedule = \"@every 90s\"",
		"wildcard subject": "subject = \"fq.*\"",
		"space subject":    "subject = \"fq bad\"",
		"timezone":         "tz = \"Mars/Olympus\"",
		"catchup":          "catch_up = \"all\"",
		"invalid json":     "payload_json = \"nope\"",
	}
	base := `[[job]]
name = "good"
schedule = "@every 1m"
subject = "fq.good"
`
	for name, replacement := range cases {
		t.Run(name, func(t *testing.T) {
			field := strings.SplitN(replacement, " ", 2)[0]
			input := base
			for _, line := range strings.Split(base, "\n") {
				if strings.HasPrefix(line, field+" ") {
					input = strings.Replace(input, line, replacement, 1)
				}
			}
			if !strings.Contains(input, replacement) {
				input += replacement + "\n"
			}
			if _, err := ParseConfig([]byte(input)); err == nil {
				t.Fatalf("expected rejection of %s", input)
			}
		})
	}
}

func TestDuplicateAndPayloadConflict(t *testing.T) {
	duplicate := validConfig + validConfig
	if _, err := ParseConfig([]byte(duplicate)); err == nil || !strings.Contains(err.Error(), "duplicate") {
		t.Fatalf("expected duplicate error, got %v", err)
	}
	conflict := strings.Replace(validConfig, "[job.payload]", `payload_json = "null"
[job.payload]`, 1)
	if _, err := ParseConfig([]byte(conflict)); err == nil {
		t.Fatal("expected payload conflict")
	}
}

func TestAcceptedSchedules(t *testing.T) {
	for _, spec := range []string{"* * * * *", "@every 1m", "@daily"} {
		input := strings.Replace(validConfig, "@daily", spec, 1)
		if _, err := ParseConfig([]byte(input)); err != nil {
			t.Errorf("%s: %v", spec, err)
		}
	}
}
