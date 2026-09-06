package main

import (
	"sort"
	"time"
)

// Fire is one scheduled publication. ScheduledAt is the logical cron slot,
// rather than the time at which the planner happened to run.
type Fire struct {
	Job         string
	Subject     string
	Payload     []byte
	ScheduledAt time.Time
}

// FireState is the persisted value for the last acknowledged fire of a job.
type FireState struct {
	LastScheduled time.Time `json:"last_scheduled"`
	PublishedAt   time.Time `json:"published_at"`
}

// JobSet is the validated scheduler input. Keeping the global limit beside the
// jobs makes plan independent of configuration loading and external state.
type JobSet struct {
	Jobs            []Job
	MaxFiresPerHour int
}

// valveWindow is the width of the sliding window `max_fires_per_hour`
// counts over.
const valveWindow = time.Hour

// plan computes at most one fire per job. Replanning therefore supersedes an
// older, unexecuted plan instead of building a queue. It is deliberately pure:
// now and all publication history are supplied by the caller.
//
// The second return value is the instant the valve re-opens: when the fire
// count in the sliding window is at the ceiling there are no fires, and
// this says when the oldest fire in the window leaves it and planning is
// worth repeating. It is the zero time whenever the valve is not the
// reason — the caller has fires to wait for, or there is simply nothing to
// schedule. Without it the loop waited only on a config reload, so a burst
// that tripped the valve silenced the scheduler until someone edited the
// file.
func plan(now time.Time, jobs JobSet, state map[string]FireState) ([]Fire, time.Time) {
	candidates := make([]Fire, 0, len(jobs.Jobs))
	for _, job := range jobs.Jobs {
		if job.Enabled != nil && !*job.Enabled {
			continue
		}
		location, err := time.LoadLocation(job.TZ)
		if err != nil {
			continue // JobSet is normally validated before it reaches the planner.
		}
		schedule, err := cronParser.Parse(job.Schedule)
		if err != nil {
			continue
		}

		previous, exists := state[job.Name]
		var scheduled time.Time
		if !exists || previous.LastScheduled.IsZero() {
			// A new job never catches up: establish its first future slot.
			scheduled = schedule.Next(now.In(location))
		} else {
			next := schedule.Next(previous.LastScheduled.In(location))
			if !next.Before(now) {
				scheduled = next
			} else if job.CatchUp != "once" {
				for !next.After(now) {
					next = schedule.Next(next)
				}
				scheduled = next
			} else {
				// Collapse every missed slot to the most recent one.
				scheduled = next
				for candidate := schedule.Next(scheduled); !candidate.After(now); candidate = schedule.Next(scheduled) {
					scheduled = candidate
				}
			}
		}

		payload, err := RenderPayload(job, scheduled)
		if err != nil {
			continue
		}
		candidates = append(candidates, Fire{Job: job.Name, Subject: job.Subject, Payload: payload, ScheduledAt: scheduled})
	}

	// Stable ordering makes both valve decisions and shell execution
	// deterministic when several jobs share a slot.
	sort.Slice(candidates, func(i, j int) bool {
		if candidates[i].ScheduledAt.Equal(candidates[j].ScheduledAt) {
			return candidates[i].Job < candidates[j].Job
		}
		return candidates[i].ScheduledAt.Before(candidates[j].ScheduledAt)
	})

	limit := jobs.MaxFiresPerHour
	if limit <= 0 {
		limit = DefaultMaxFiresPerHour
	}
	windowStart := now.Add(-valveWindow)
	used := 0
	var oldestInWindow time.Time
	for _, previous := range state {
		if previous.PublishedAt.After(windowStart) && !previous.PublishedAt.After(now) {
			used++
			if oldestInWindow.IsZero() || previous.PublishedAt.Before(oldestInWindow) {
				oldestInWindow = previous.PublishedAt
			}
		}
	}
	if used >= limit {
		// The window slides: at oldestInWindow+valveWindow that fire is no
		// longer inside it, `used` drops below the ceiling, and this plan
		// is worth recomputing. (used >= limit implies at least one fire
		// in the window for any limit >= 1, and limit is validated
		// positive, so oldestInWindow is set.)
		return nil, oldestInWindow.Add(valveWindow)
	}
	if remaining := limit - used; len(candidates) > remaining {
		candidates = candidates[:remaining]
	}
	return candidates, time.Time{}
}
