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

// FireState is the persisted value for a job's fires. LastScheduled and
// PublishedAt describe the last acknowledged one; RecentFires is the
// valve's ledger — that job's recent publication instants, oldest first,
// which is what the per-hour ceiling counts.
//
// The ledger exists because one timestamp per job cannot express a rate:
// counting jobs whose last fire fell in the window, the ceiling only ever
// closed when the file itself held `limit` jobs, and a single runaway
// schedule fired for ever (issue #612). Its last entry is PublishedAt,
// which is retained as the answer to "when did this job last fire?" for
// anyone reading the bucket.
//
// A row written before the ledger existed decodes with RecentFires empty;
// countedFires then reads its PublishedAt as the single fire it records,
// so an existing bucket loads without a migration and folds into the
// ledger the next time each job fires.
type FireState struct {
	LastScheduled time.Time   `json:"last_scheduled"`
	PublishedAt   time.Time   `json:"published_at"`
	RecentFires   []time.Time `json:"recent_fires,omitempty"`
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
// this says when enough of the window's oldest fires have left it for the
// count to fall back under the ceiling and planning to be worth
// repeating. It is the zero time whenever the valve is not the
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

	limit := effectiveLimit(jobs.MaxFiresPerHour)
	inWindow := firesInWindow(state, now)
	used := len(inWindow)
	if used >= limit {
		// The window slides: sorted oldest first, the fire at index
		// used-limit is the one whose departure first brings the count
		// back under the ceiling — everything older leaves before it, and
		// once it has gone only limit-1 fires remain. With used == limit,
		// the ordinary case, that is the oldest fire in the window. At
		// that instant `used` is below the ceiling and this plan is worth
		// recomputing. (used >= limit >= 1, so the index is in range.)
		return nil, inWindow[used-limit].Add(valveWindow)
	}
	if remaining := limit - used; len(candidates) > remaining {
		candidates = candidates[:remaining]
	}
	return candidates, time.Time{}
}

// firesInWindow is every fire the valve counts, oldest first: each job's
// recorded publications that fall inside the sliding window. Fires are
// counted one by one rather than one per job, so a single schedule
// running away trips the ceiling on its own.
func firesInWindow(state map[string]FireState, now time.Time) []time.Time {
	windowStart := now.Add(-valveWindow)
	var fires []time.Time
	for _, previous := range state {
		for _, at := range countedFires(previous) {
			if at.After(windowStart) && !at.After(now) {
				fires = append(fires, at)
			}
		}
	}
	sort.Slice(fires, func(i, j int) bool { return fires[i].Before(fires[j]) })
	return fires
}

// countedFires is one job's fire history as the valve reads it, oldest
// first: its ledger, or — for a row written before the ledger existed —
// the single fire its PublishedAt records.
func countedFires(state FireState) []time.Time {
	if len(state.RecentFires) > 0 {
		return state.RecentFires
	}
	if state.PublishedAt.IsZero() {
		return nil
	}
	return []time.Time{state.PublishedAt}
}

// recordFire is the state a job carries after a publish acknowledged at
// publishedAt: the slot advances and the fire joins the ledger, which is
// then trimmed to what the ceiling can still count — fires inside the
// window, and at most the newest `limit` of them, since no decision can
// turn on an older one. Trimming per job is exact for the global count
// while the ceiling is unchanged, because the newest `limit` fires across
// the file are always inside the union of each job's newest `limit`;
// lowering the ceiling stays exact, and raising it under-counts for at
// most one window, until the ledgers refill under the new one. The trim
// is also what bounds the stored value — a job's ledger never holds more
// than `limit` instants, whatever its schedule does.
//
// It is the ledger's only writer, which is what keeps it in publication
// order.
func recordFire(previous FireState, scheduled, publishedAt time.Time, configuredLimit int) FireState {
	windowStart := publishedAt.Add(-valveWindow)
	counted := countedFires(previous)
	ledger := make([]time.Time, 0, len(counted)+1)
	for _, at := range counted {
		if at.After(windowStart) {
			ledger = append(ledger, at)
		}
	}
	ledger = append(ledger, publishedAt)
	if limit := effectiveLimit(configuredLimit); len(ledger) > limit {
		ledger = ledger[len(ledger)-limit:]
	}
	return FireState{LastScheduled: scheduled, PublishedAt: publishedAt, RecentFires: ledger}
}

// supersedeFire records a fire that was never published: the slot moves
// on, but the job's publication history is untouched. Rebuilding the
// record from scratch here would drop the ledger, and a job whose
// publishes kept being superseded would launder its way past the valve.
func supersedeFire(previous FireState, scheduled time.Time) FireState {
	previous.LastScheduled = scheduled
	return previous
}

// effectiveLimit resolves the configured ceiling. Validation guarantees a
// positive value, so the fallback only covers a JobSet built in code.
func effectiveLimit(configured int) int {
	if configured <= 0 {
		return DefaultMaxFiresPerHour
	}
	return configured
}
