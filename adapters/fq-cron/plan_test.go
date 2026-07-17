package main

import (
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
			fires := plan(tt.now, JobSet{Jobs: []Job{tt.job}, MaxFiresPerHour: 10}, tt.state)
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
			fires := plan(tt.now, JobSet{Jobs: []Job{testJob(tt.spec, location.String(), "skip")}, MaxFiresPerHour: 10}, nil)
			if len(fires) != 1 || !fires[0].ScheduledAt.Equal(tt.want) {
				t.Fatalf("plan() = %#v, want one fire at %s", fires, tt.want)
			}
		})
	}
}

func TestPlanSupersedesOlderSlots(t *testing.T) {
	now := time.Date(2026, time.July, 17, 12, 30, 0, 0, time.UTC)
	fires := plan(now, JobSet{Jobs: []Job{testJob("*/5 * * * *", "UTC", "once")}, MaxFiresPerHour: 10}, map[string]FireState{
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
	fires := plan(now, JobSet{Jobs: jobs, MaxFiresPerHour: 1}, state)
	if len(fires) != 1 || fires[0].Job != "catch-up" {
		t.Fatalf("plan() = %#v; want catch-up to consume the only valve slot", fires)
	}

	state["recent"] = FireState{PublishedAt: now.Add(-10 * time.Minute)}
	if fires := plan(now, JobSet{Jobs: jobs, MaxFiresPerHour: 1}, state); len(fires) != 0 {
		t.Fatalf("plan() = %#v; want recent fire to close valve", fires)
	}
}

func testJob(schedule, tz, catchUp string) Job {
	return Job{Name: "job", Schedule: schedule, Subject: "fq.test", TZ: tz, CatchUp: catchUp, Enabled: boolPtr(true)}
}
