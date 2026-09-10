package main

import (
	"errors"
	"os"
	"path/filepath"
	"strings"
	"testing"
	"time"
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

// joblessConfigs are the shapes a file takes when it holds no jobs and never
// says it means to: the zero bytes of a writer's truncate gap, and the three
// ways an operator reaches the same place by hand. Every one of them is
// refused wherever a config is read (#623, #664).
var joblessConfigs = map[string]string{
	"zero bytes":      "",
	"whitespace only": "  \n\t\n",
	"comments only":   "# every job commented out\n",
	"limits only":     "[limits]\nmax_fires_per_hour = 30\n",
}

// The rule lives in ParseConfig, so it is the same rule and the same sentence
// on every path that reads a config — the reload's, startup's, and
// `--check`'s. It used to be the reload's alone, which is how `--check` came
// to call a zero-byte file valid (#664).
func TestParseConfigRefusesAConfigDeclaringNoJobs(t *testing.T) {
	for name, text := range joblessConfigs {
		t.Run(name, func(t *testing.T) {
			cfg, err := ParseConfig([]byte(text))
			if err == nil {
				t.Fatalf("a %d-byte config declaring no jobs was accepted: %+v", len(text), cfg)
			}
			// Byte-identical to the sentence watch.go logs — the reload's log
			// line is this error, printed (watch_test.go asserts on it).
			want := jobsUndeclaredError{Bytes: len(text)}.Error()
			if err.Error() != want {
				t.Fatalf("error = %q, want %q", err, want)
			}
			var undeclared jobsUndeclaredError
			if !errors.As(err, &undeclared) || undeclared.Bytes != len(text) {
				t.Fatalf("error = %#v, want a jobsUndeclaredError carrying %d bytes", err, len(text))
			}
		})
	}
}

// `job = []` is the one way a file says "no jobs" out loud, and saying it is
// enough: the config is valid, and fq-cron runs it with nothing scheduled.
func TestParseConfigAcceptsAnExplicitlyEmptyJobList(t *testing.T) {
	cfg, err := ParseConfig([]byte("job = []\n"))
	if err != nil {
		t.Fatalf("`job = []` was refused: %v", err)
	}
	if len(cfg.Jobs) != 0 {
		t.Fatalf("jobs = %q, want none", jobNames(cfg))
	}
	// And it is a whole config, not a special case: the defaults still apply.
	if cfg.Limits.MaxFiresPerHour != DefaultMaxFiresPerHour {
		t.Fatalf("limits = %+v, want the defaults applied", cfg.Limits)
	}
}

// What startup and `--check` put in front of an operator: the reload's
// sentence, the file it is about, and the line that makes it go away. The
// process exits 1 on it — main prints the error and exits (main.go).
func TestLoadConfigRefusalNamesTheFileAndWhatToWrite(t *testing.T) {
	for name, text := range joblessConfigs {
		t.Run(name, func(t *testing.T) {
			path := filepath.Join(t.TempDir(), "fq-cron.toml")
			if err := os.WriteFile(path, []byte(text), 0o600); err != nil {
				t.Fatal(err)
			}
			_, err := LoadConfig(path)
			if err == nil {
				t.Fatalf("LoadConfig started on a %d-byte config declaring no jobs", len(text))
			}
			want := path + ": " + jobsUndeclaredError{Bytes: len(text)}.Error() +
				" — write `job = []` to run with nothing scheduled"
			if err.Error() != want {
				t.Fatalf("error = %q, want %q", err, want)
			}
			var undeclared jobsUndeclaredError
			if !errors.As(err, &undeclared) {
				t.Fatalf("error = %#v, want the reload's own refusal wrapped, not a second copy of it", err)
			}
		})
	}
}

// Every refusal LoadConfig makes names the file it is about. A validation
// error and a TOML error carry no path of their own, and the operator reading
// `docker compose logs fq-cron` does not see the FQCRON_CONFIG the message
// came from — so the message has to carry it (#664 review).
func TestLoadConfigRefusalsNameTheFile(t *testing.T) {
	for name, text := range map[string]string{
		"invalid TOML":     "not = [valid",
		"validation error": "[limits]\nmax_fires_per_hour = -1\n",
		"bad job":          "[[job]]\nname = \"Bad_Name\"\nschedule = \"@daily\"\nsubject = \"fq.x\"\n",
	} {
		t.Run(name, func(t *testing.T) {
			path := filepath.Join(t.TempDir(), "fq-cron.toml")
			if err := os.WriteFile(path, []byte(text), 0o600); err != nil {
				t.Fatal(err)
			}
			_, err := LoadConfig(path)
			if err == nil {
				t.Fatal("expected a refusal")
			}
			if !strings.HasPrefix(err.Error(), path+": ") {
				t.Fatalf("error = %q, want it to begin with the file it is about (%q)", err, path)
			}
			// The reason survives the wrapping.
			if bare, _ := ParseConfig([]byte(text)); bare != nil {
				t.Fatal("this case must be one ParseConfig refuses")
			}
		})
	}
	// The read error is the exception, and deliberately: os.ReadFile already
	// names the file, so prefixing it again would only stutter.
	missing := filepath.Join(t.TempDir(), "absent.toml")
	_, err := LoadConfig(missing)
	if err == nil || !strings.Contains(err.Error(), missing) {
		t.Fatalf("error = %v, want the missing file named once", err)
	}
	if strings.HasPrefix(err.Error(), missing+": ") {
		t.Fatalf("error = %q, want no second copy of the path", err)
	}
}

// #634's "silently and indefinitely", one step earlier. A LoadConfig landing
// in a writer's truncate gap used to start fq-cron with nothing scheduled,
// because startup did not apply the reload rule (#632) — the watcher's first
// check was the only thing that could put the jobs back. Startup applies that
// rule now, so there is no job-less startup left to recover from: the process
// does not come up, and the supervisor's restart reads the file the writer has
// since finished (#664). The watcher's own guarantee, that an edit made during
// the broker wait is seen, is watch_test.go's "a write inside the window lands
// on the first check".
func TestAStartupThatReadsATornFileDoesNotStart(t *testing.T) {
	path := filepath.Join(t.TempDir(), "fq-cron.toml")
	if err := os.WriteFile(path, nil, 0o600); err != nil { // the writer's truncate
		t.Fatal(err)
	}
	if _, err := LoadConfig(path); err == nil {
		t.Fatal("a torn read started a scheduler with nothing scheduled")
	}
	// The restart that follows reads the completed write and starts.
	if err := os.WriteFile(path, []byte(validConfig), 0o600); err != nil {
		t.Fatal(err)
	}
	loaded, err := LoadConfig(path)
	if err != nil {
		t.Fatalf("the restart refused the finished file: %v", err)
	}
	if len(loaded.Config.Jobs) != 1 {
		t.Fatalf("the restart loaded %d jobs, want the finished file's one", len(loaded.Config.Jobs))
	}
}

// What a configuration will fire is not how many jobs it declares: the planner
// skips `enabled = false` (plan.go), and this is the count everything that
// reports on a configuration uses, so the two cannot disagree.
func TestScheduledJobsCountsWhatThePlannerWillFire(t *testing.T) {
	job := func(name, enabled string) string {
		return "[[job]]\nname = \"" + name + "\"\nschedule = \"@daily\"\nsubject = \"fq.x\"\n" + enabled
	}
	for name, tc := range map[string]struct {
		text string
		want int
	}{
		"enabled by default": {job("a", ""), 1},
		"explicitly on":      {job("a", "enabled = true\n"), 1},
		"explicitly off":     {job("a", "enabled = false\n"), 0},
		"one of two off":     {job("a", "enabled = false\n") + job("b", ""), 1},
		"every one off":      {job("a", "enabled = false\n") + job("b", "enabled = false\n"), 0},
		"none declared":      {"job = []\n", 0},
	} {
		t.Run(name, func(t *testing.T) {
			cfg := mustParse(t, tc.text)
			if got := cfg.scheduledJobs(); got != tc.want {
				t.Fatalf("scheduledJobs() = %d, want %d", got, tc.want)
			}
			// The count must be the planner's own answer, not a second
			// opinion about it: an empty plan is what "schedules nothing"
			// means to the process.
			fires, _ := plan(time.Now(), JobSet{Jobs: cfg.Jobs, MaxFiresPerHour: cfg.Limits.MaxFiresPerHour}, map[string]FireState{})
			if (len(fires) == 0) != (tc.want == 0) {
				t.Fatalf("scheduledJobs() = %d but the planner produced %d fires", tc.want, len(fires))
			}
		})
	}
}

func TestLoadConfigAcceptsAnExplicitlyEmptyJobList(t *testing.T) {
	path := filepath.Join(t.TempDir(), "fq-cron.toml")
	if err := os.WriteFile(path, []byte("job = []\n"), 0o600); err != nil {
		t.Fatal(err)
	}
	loaded, err := LoadConfig(path)
	if err != nil {
		t.Fatalf("`job = []` was refused at startup: %v", err)
	}
	if len(loaded.Config.Jobs) != 0 {
		t.Fatalf("jobs = %q, want none", jobNames(loaded.Config))
	}
	// The bytes still come back with it, so the watcher is seeded from what is
	// running rather than from a read of its own (#634).
	if string(loaded.Raw) != "job = []\n" {
		t.Fatalf("raw = %q, want the bytes it was parsed from", loaded.Raw)
	}
}

// A file with a real mistake in it is told about the mistake: the whole-file
// rule is applied last, so it cannot mask the field that is actually wrong.
func TestAFileWithNoJobsAndABadLimitIsToldAboutTheLimit(t *testing.T) {
	_, err := ParseConfig([]byte("[limits]\nmax_fires_per_hour = -1\n"))
	if err == nil || !strings.Contains(err.Error(), "max_fires_per_hour") {
		t.Fatalf("error = %v, want the limit named", err)
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
