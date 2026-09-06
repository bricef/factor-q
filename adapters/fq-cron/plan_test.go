package main

import (
	"encoding/json"
	"testing"
	"time"
)

func TestPlanCronEdgesAndCatchUp(t *testing.T) {
	utc := time.UTC
	tests := []struct {
		name  string
		now   time.Time
		job   Job
		state map[string]FireState
		want  time.Time
	}{
		{
			name: "month boundary",
			now:  time.Date(2026, time.January, 31, 23, 59, 0, 0, utc),
			job:  testJob("0 0 1 * *", "UTC", "skip"),
			want: time.Date(2026, time.February, 1, 0, 0, 0, 0, utc),
		},
		{
			name: "new once job does not catch up",
			now:  time.Date(2026, time.July, 17, 12, 30, 0, 0, utc),
			job:  testJob("0 * * * *", "UTC", "once"),
			want: time.Date(2026, time.July, 17, 13, 0, 0, 0, utc),
		},
		{
			name: "present once state collapses missed slots",
			now:  time.Date(2026, time.July, 17, 12, 30, 0, 0, utc),
			job:  testJob("0 * * * *", "UTC", "once"),
			state: map[string]FireState{"job": {
				LastScheduled: time.Date(2026, time.July, 17, 8, 0, 0, 0, utc),
			}},
			want: time.Date(2026, time.July, 17, 12, 0, 0, 0, utc),
		},
		{
			name: "present skip state chooses future slot",
			now:  time.Date(2026, time.July, 17, 12, 30, 0, 0, utc),
			job:  testJob("0 * * * *", "UTC", "skip"),
			state: map[string]FireState{"job": {
				LastScheduled: time.Date(2026, time.July, 17, 8, 0, 0, 0, utc),
			}},
			want: time.Date(2026, time.July, 17, 13, 0, 0, 0, utc),
		},
	}
	for _, tt := range tests {
		t.Run(tt.name, func(t *testing.T) {
			fires, _ := plan(tt.now, JobSet{Jobs: []Job{tt.job}, MaxFiresPerHour: 10}, tt.state)
			if len(fires) != 1 || !fires[0].ScheduledAt.Equal(tt.want) {
				t.Fatalf("plan() = %#v, want one fire at %s", fires, tt.want)
			}
		})
	}
}

func TestPlanDST(t *testing.T) {
	location, err := time.LoadLocation("America/New_York")
	if err != nil {
		t.Fatal(err)
	}
	tests := []struct {
		name string
		now  time.Time
		spec string
		want time.Time
	}{
		{
			name: "spring-forward skips nonexistent local time",
			now:  time.Date(2026, time.March, 8, 1, 59, 0, 0, location),
			spec: "30 2 * * *",
			want: time.Date(2026, time.March, 9, 2, 30, 0, 0, location),
		},
		{
			name: "fall-back chooses next wall-clock occurrence",
			now:  time.Date(2026, time.November, 1, 0, 59, 0, 0, location),
			spec: "30 1 * * *",
			want: time.Date(2026, time.November, 1, 1, 30, 0, 0, location),
		},
	}
	for _, tt := range tests {
		t.Run(tt.name, func(t *testing.T) {
			fires, _ := plan(tt.now, JobSet{Jobs: []Job{testJob(tt.spec, location.String(), "skip")}, MaxFiresPerHour: 10}, nil)
			if len(fires) != 1 || !fires[0].ScheduledAt.Equal(tt.want) {
				t.Fatalf("plan() = %#v, want one fire at %s", fires, tt.want)
			}
		})
	}
}

func TestPlanSupersedesOlderSlots(t *testing.T) {
	now := time.Date(2026, time.July, 17, 12, 30, 0, 0, time.UTC)
	fires, _ := plan(now, JobSet{Jobs: []Job{testJob("*/5 * * * *", "UTC", "once")}, MaxFiresPerHour: 10}, map[string]FireState{
		"job": {LastScheduled: now.Add(-30 * time.Minute)},
	})
	if len(fires) != 1 || !fires[0].ScheduledAt.Equal(time.Date(2026, time.July, 17, 12, 30, 0, 0, time.UTC)) {
		t.Fatalf("plan() = %#v; want only newest slot", fires)
	}
}

func TestPlanValveIncludesCatchUps(t *testing.T) {
	now := time.Date(2026, time.July, 17, 12, 30, 0, 0, time.UTC)
	jobs := []Job{testJob("0 * * * *", "UTC", "once")}
	jobs[0].Name = "catch-up"
	jobs = append(jobs, testJob("0 * * * *", "UTC", "skip"))
	state := map[string]FireState{
		"catch-up": {LastScheduled: now.Add(-3 * time.Hour)},
	}
	fires, _ := plan(now, JobSet{Jobs: jobs, MaxFiresPerHour: 1}, state)
	if len(fires) != 1 || fires[0].Job != "catch-up" {
		t.Fatalf("plan() = %#v; want catch-up to consume the only valve slot", fires)
	}

	state["recent"] = FireState{PublishedAt: now.Add(-10 * time.Minute)}
	if fires, _ := plan(now, JobSet{Jobs: jobs, MaxFiresPerHour: 1}, state); len(fires) != 0 {
		t.Fatalf("plan() = %#v; want recent fire to close valve", fires)
	}
}

// A closed valve must say when it reopens, or the scheduler has nothing
// to wake up for: the oldest fire in the window leaves it exactly one
// window after it was published.
func TestPlanReportsWhenTheValveReopens(t *testing.T) {
	now := time.Date(2026, time.July, 17, 12, 30, 0, 0, time.UTC)
	oldest := now.Add(-50 * time.Minute)
	state := map[string]FireState{
		"job":   {PublishedAt: oldest},
		"other": {PublishedAt: now.Add(-10 * time.Minute)},
	}
	jobs := []Job{testJob("0 * * * *", "UTC", "skip")}
	jobs = append(jobs, testJob("0 * * * *", "UTC", "skip"))
	jobs[1].Name = "other"

	fires, reopens := plan(now, JobSet{Jobs: jobs, MaxFiresPerHour: 2}, state)
	if len(fires) != 0 {
		t.Fatalf("plan() = %#v; want the valve closed", fires)
	}
	want := oldest.Add(time.Hour)
	if !reopens.Equal(want) {
		t.Fatalf("valve reopens at %s, want %s (the oldest in-window fire leaving it)", reopens, want)
	}
	// And at that instant the valve is open again, with no reload.
	if fires, reopens := plan(want, JobSet{Jobs: jobs, MaxFiresPerHour: 2}, state); len(fires) == 0 || !reopens.IsZero() {
		t.Fatalf("plan() at the reopen instant = %#v, %s; want a fire and no further wait", fires, reopens)
	}
}

// An open valve reports no re-plan instant: the caller has fires to wait
// for, and a timer would only wake it for nothing.
func TestPlanReportsNoValveWaitWhenOpen(t *testing.T) {
	now := time.Date(2026, time.July, 17, 12, 30, 0, 0, time.UTC)
	fires, reopens := plan(now, JobSet{Jobs: []Job{testJob("0 * * * *", "UTC", "skip")}, MaxFiresPerHour: 10}, nil)
	if len(fires) != 1 || !reopens.IsZero() {
		t.Fatalf("plan() = %#v, %s; want one fire and no valve wait", fires, reopens)
	}
}

// The ceiling counts fires, not jobs. One job on a one-minute schedule
// must be refused its fourth fire inside the window against a ceiling of
// three — counting one entry per job, `used` never rose above one here
// and this schedule fired for ever (issue #612).
func TestValveCountsFiresFromOneRunawayJob(t *testing.T) {
	const limit = 3
	jobs := JobSet{Jobs: []Job{testJob("* * * * *", "UTC", "skip")}, MaxFiresPerHour: limit}
	state := map[string]FireState{}
	now := time.Date(2026, time.July, 17, 12, 0, 30, 0, time.UTC)

	// Drive the loop's own sequence: plan, wait for the slot, record.
	var fired []time.Time
	for attempt := 1; attempt <= limit; attempt++ {
		fires, reopens := plan(now, jobs, state)
		if len(fires) != 1 || !reopens.IsZero() {
			t.Fatalf("fire %d: plan() = %#v, %s; want one fire under an open valve", attempt, fires, reopens)
		}
		now = fires[0].ScheduledAt
		state["job"] = recordFire(state["job"], fires[0].ScheduledAt, now, limit)
		fired = append(fired, now)
	}

	fires, reopens := plan(now, jobs, state)
	if len(fires) != 0 {
		t.Fatalf("plan() = %#v; want the valve shut on fire %d of one job against a ceiling of %d", fires, len(fired)+1, limit)
	}
	want := fired[0].Add(valveWindow)
	if !reopens.Equal(want) {
		t.Fatalf("valve reopens at %s, want %s (the first counted fire leaving the window)", reopens, want)
	}
	if early, _ := plan(want.Add(-time.Nanosecond), jobs, state); len(early) != 0 {
		t.Fatalf("plan() a nanosecond early = %#v; want the valve still shut", early)
	}
	if open, wait := plan(want, jobs, state); len(open) != 1 || !wait.IsZero() {
		t.Fatalf("plan() at the reopen instant = %#v, %s; want one fire and no further wait", open, wait)
	}
}

// The ceiling is over the file, so two jobs' fires count together.
func TestValveCountsFiresAcrossJobs(t *testing.T) {
	now := time.Date(2026, time.July, 17, 12, 30, 0, 0, time.UTC)
	jobs := []Job{testJob("0 * * * *", "UTC", "skip"), testJob("0 * * * *", "UTC", "skip")}
	jobs[1].Name = "other"
	oldest := now.Add(-40 * time.Minute)
	// PublishedAt is each ledger's last entry, the invariant recordFire
	// maintains — so counting jobs would see two of them here and let a
	// third fire through, which is exactly the difference under test.
	state := map[string]FireState{
		"job": {
			PublishedAt: now.Add(-20 * time.Minute),
			RecentFires: []time.Time{oldest, now.Add(-20 * time.Minute)},
		},
		"other": {
			PublishedAt: now.Add(-10 * time.Minute),
			RecentFires: []time.Time{now.Add(-10 * time.Minute)},
		},
	}

	fires, reopens := plan(now, JobSet{Jobs: jobs, MaxFiresPerHour: 3}, state)
	if len(fires) != 0 {
		t.Fatalf("plan() = %#v; want two jobs' three fires to close a ceiling of three", fires)
	}
	if want := oldest.Add(valveWindow); !reopens.Equal(want) {
		t.Fatalf("valve reopens at %s, want %s", reopens, want)
	}
	// Two fires between them leave a slot, and it goes to one job only.
	state["job"] = FireState{
		PublishedAt: now.Add(-20 * time.Minute),
		RecentFires: []time.Time{now.Add(-20 * time.Minute)},
	}
	if fires, _ := plan(now, JobSet{Jobs: jobs, MaxFiresPerHour: 3}, state); len(fires) != 1 {
		t.Fatalf("plan() = %#v; want the one remaining slot", fires)
	}
}

// A row written before the ledger existed carries only published_at. It
// must decode, count as the single fire it records, and fold into the
// ledger the first time the job fires again — no migration, no crash.
func TestValveCountsRowsWrittenBeforeTheLedger(t *testing.T) {
	var old FireState
	row := `{"last_scheduled":"2026-07-17T12:00:00Z","published_at":"2026-07-17T12:00:01Z"}`
	if err := json.Unmarshal([]byte(row), &old); err != nil {
		t.Fatalf("decode a pre-ledger row: %v", err)
	}
	if old.RecentFires != nil {
		t.Fatalf("RecentFires = %v, want nothing recorded", old.RecentFires)
	}

	now := time.Date(2026, time.July, 17, 12, 30, 0, 0, time.UTC)
	jobs := []Job{testJob("0 * * * *", "UTC", "skip")}
	if fires, _ := plan(now, JobSet{Jobs: jobs, MaxFiresPerHour: 1}, map[string]FireState{"job": old}); len(fires) != 0 {
		t.Fatalf("plan() = %#v; want the pre-ledger fire counted against a ceiling of one", fires)
	}

	migrated := recordFire(old, now, now, 3)
	if len(migrated.RecentFires) != 2 || !migrated.RecentFires[0].Equal(old.PublishedAt) || !migrated.RecentFires[1].Equal(now) {
		t.Fatalf("ledger after the first new fire = %v, want [%s %s]", migrated.RecentFires, old.PublishedAt, now)
	}
	value, err := json.Marshal(migrated)
	if err != nil {
		t.Fatal(err)
	}
	want := `{"last_scheduled":"2026-07-17T12:30:00Z","published_at":"2026-07-17T12:30:00Z",` +
		`"recent_fires":["2026-07-17T12:00:01Z","2026-07-17T12:30:00Z"]}`
	if string(value) != want {
		t.Fatalf("JSON = %s, want %s", value, want)
	}
}

// The ledger stays bounded: fires that have left the window are dropped,
// and beyond the newest `limit` of them no decision can still turn on the
// rest.
func TestRecordFireBoundsTheLedger(t *testing.T) {
	now := time.Date(2026, time.July, 17, 12, 30, 0, 0, time.UTC)
	previous := FireState{RecentFires: []time.Time{
		now.Add(-90 * time.Minute), // outside the window
		now.Add(-40 * time.Minute),
		now.Add(-20 * time.Minute),
		now.Add(-10 * time.Minute),
	}}

	recorded := recordFire(previous, now, now, 2)
	want := []time.Time{now.Add(-10 * time.Minute), now}
	if len(recorded.RecentFires) != len(want) {
		t.Fatalf("ledger = %v, want the newest %d fires %v", recorded.RecentFires, len(want), want)
	}
	for i, at := range want {
		if !recorded.RecentFires[i].Equal(at) {
			t.Fatalf("ledger = %v, want %v", recorded.RecentFires, want)
		}
	}
	if len(previous.RecentFires) != 4 {
		t.Fatalf("recordFire mutated its input: %v", previous.RecentFires)
	}
}

// A superseded fire moves the slot on without publishing; it must not
// erase the history the valve counts, or a job whose publishes keep being
// superseded would launder its way past the ceiling.
func TestSupersededFireKeepsTheLedger(t *testing.T) {
	now := time.Date(2026, time.July, 17, 12, 30, 0, 0, time.UTC)
	previous := FireState{
		LastScheduled: now.Add(-time.Hour),
		PublishedAt:   now.Add(-10 * time.Minute),
		RecentFires:   []time.Time{now.Add(-20 * time.Minute), now.Add(-10 * time.Minute)},
	}
	superseded := supersedeFire(previous, now)
	if !superseded.LastScheduled.Equal(now) {
		t.Fatalf("LastScheduled = %s, want the missed slot %s", superseded.LastScheduled, now)
	}
	if len(superseded.RecentFires) != 2 || !superseded.PublishedAt.Equal(previous.PublishedAt) {
		t.Fatalf("superseded state = %#v; want the publication history intact", superseded)
	}
}

func testJob(schedule, tz, catchUp string) Job {
	return Job{Name: "job", Schedule: schedule, Subject: "fq.test", TZ: tz, CatchUp: catchUp, Enabled: boolPtr(true)}
}
