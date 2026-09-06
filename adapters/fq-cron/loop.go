package main

import (
	"context"
	"errors"
	"fmt"
	"log"
	"time"
)

const (
	initialRetryBackoff = 250 * time.Millisecond
	maximumRetryBackoff = 30 * time.Second
)

var errFireSuperseded = errors.New("fire superseded by next scheduled slot")

// runScheduler is the adapter's thin orchestration loop. A fire is recorded
// only after its publish has been acknowledged.
func runScheduler(ctx context.Context, config *Config, reloads <-chan ReloadEvent, publisher Publisher, store StateStore, logger *log.Logger) error {
	if logger == nil {
		logger = log.Default()
	}
	unhealthy := make(map[string]bool)
	for {
		var state map[string]FireState
		if err := withBrokerRetry(ctx, logger, "load state", func() error {
			var err error
			state, err = loadState(ctx, config, store)
			return err
		}); err != nil {
			return nil // withBrokerRetry only fails on a cancelled context
		}
		fires, valveReopensAt := plan(time.Now(), JobSet{Jobs: config.Jobs, MaxFiresPerHour: config.Limits.MaxFiresPerHour}, state)
		if len(fires) == 0 {
			// Nothing to fire. If the valve is what is holding the plan
			// back, wake when the window slides; otherwise only a reload
			// can change the answer.
			valve, stopValve := timerUntil(valveReopensAt)
			if valve != nil {
				logger.Printf("valve=closed reopens=%s: fires suppressed until the window slides", valveReopensAt.Format(time.RFC3339))
			}
			select {
			case <-ctx.Done():
				stopValve()
				return nil
			case <-valve:
				logger.Printf("valve=open replanning")
				continue
			case event, ok := <-reloads:
				stopValve()
				if !ok {
					reloads = nil
					continue
				}
				if err := withBrokerRetry(ctx, logger, "remove state for dropped jobs", func() error {
					return removeState(ctx, event.Diff.Removed, store)
				}); err != nil {
					return nil
				}
				config = event.Config
				unhealthy = make(map[string]bool)
			}
			continue
		}

		wait := time.Until(fires[0].ScheduledAt)
		if wait > 0 {
			timer := time.NewTimer(wait)
			select {
			case <-ctx.Done():
				timer.Stop()
				return nil
			case event, ok := <-reloads:
				timer.Stop()
				if ok {
					if err := withBrokerRetry(ctx, logger, "remove state for dropped jobs", func() error {
						return removeState(ctx, event.Diff.Removed, store)
					}); err != nil {
						return nil
					}
					config = event.Config
					unhealthy = make(map[string]bool)
				} else {
					reloads = nil
				}
				continue
			case <-timer.C:
			}
		}

		jobs := jobsByName(config)
		for _, fire := range fires {
			if fire.ScheduledAt.After(time.Now()) || unhealthy[fire.Job] {
				continue
			}
			job, ok := jobs[fire.Job]
			if !ok {
				continue
			}
			if err := publishWithBackoff(ctx, publisher, fire, job, logger); err != nil {
				if errors.Is(err, errFireSuperseded) {
					logger.Printf("job=%s scheduled=%s missed: superseded by next slot", fire.Job, fire.ScheduledAt.Format(time.RFC3339))
					record := FireState{LastScheduled: fire.ScheduledAt}
					if err := withBrokerRetry(ctx, logger, fmt.Sprintf("record superseded fire %q", fire.Job), func() error {
						return store.Put(ctx, fire.Job, record)
					}); err != nil {
						return nil
					}
					continue
				}
				if IsPermanentPublishError(err) {
					unhealthy[fire.Job] = true
					logger.Printf("job=%s scheduled=%s unhealthy: %v", fire.Job, fire.ScheduledAt.Format(time.RFC3339), err)
					continue
				}
				if ctx.Err() != nil {
					return nil
				}
				return err
			}
			record := FireState{LastScheduled: fire.ScheduledAt, PublishedAt: time.Now()}
			if err := withBrokerRetry(ctx, logger, fmt.Sprintf("record acknowledged fire %q", fire.Job), func() error {
				return store.Put(ctx, fire.Job, record)
			}); err != nil {
				return nil
			}
			logger.Printf("job=%s scheduled=%s published", fire.Job, fire.ScheduledAt.Format(time.RFC3339))
		}
	}
}

// timerUntil returns a channel that fires at at, and a stop func. A zero
// `at` yields a nil channel — a select case that never fires — so the
// caller can offer the case unconditionally.
func timerUntil(at time.Time) (<-chan time.Time, func()) {
	if at.IsZero() {
		return nil, func() {}
	}
	timer := time.NewTimer(time.Until(at))
	return timer.C, func() { timer.Stop() }
}

// withBrokerRetry runs op with capped exponential backoff for as long as
// it keeps failing, returning only once it succeeds or ctx ends — so the
// only error it ever returns is ctx.Err().
//
// Every state-store call goes through it because the state store *is* the
// broker (JetStream KV). Returning those errors up the loop is exactly how
// a broker outage used to end the process: the KV read is the first thing
// each iteration does, so sixty failed reconnects later the scheduler
// exited — and, under compose's restart policy, came straight back to die
// again, crash-looping for the length of the outage. A store that stays
// unreachable now keeps one process alive and loud instead, and the fires
// resume by themselves when the broker returns.
func withBrokerRetry(ctx context.Context, logger *log.Logger, what string, op func() error) error {
	backoff := initialRetryBackoff
	for attempt := 1; ; attempt++ {
		err := op()
		if err == nil {
			if attempt > 1 {
				logger.Printf("%s: recovered after %d attempts", what, attempt)
			}
			return nil
		}
		if ctx.Err() != nil {
			return ctx.Err()
		}
		logger.Printf("%s: attempt=%d failed, retrying in %s: %v", what, attempt, backoff, err)
		timer := time.NewTimer(backoff)
		select {
		case <-ctx.Done():
			timer.Stop()
			return ctx.Err()
		case <-timer.C:
		}
		backoff = nextBackoff(backoff)
	}
}

// nextBackoff doubles backoff, capped at maximumRetryBackoff.
func nextBackoff(backoff time.Duration) time.Duration {
	backoff *= 2
	if backoff > maximumRetryBackoff {
		return maximumRetryBackoff
	}
	return backoff
}

func loadState(ctx context.Context, config *Config, store StateStore) (map[string]FireState, error) {
	state := make(map[string]FireState, len(config.Jobs))
	for _, job := range config.Jobs {
		value, exists, err := store.Get(ctx, job.Name)
		if err != nil {
			return nil, fmt.Errorf("load state for %q: %w", job.Name, err)
		}
		if exists {
			state[job.Name] = value
		}
	}
	return state, nil
}

func removeState(ctx context.Context, names []string, store StateStore) error {
	for _, name := range names {
		if err := store.Delete(ctx, name); err != nil {
			return fmt.Errorf("remove state for %q: %w", name, err)
		}
	}
	return nil
}

func publishWithBackoff(ctx context.Context, publisher Publisher, fire Fire, job Job, logger *log.Logger) error {
	backoff := initialRetryBackoff
	attempt := 1
	location, _ := time.LoadLocation(job.TZ)
	schedule, _ := cronParser.Parse(job.Schedule)
	nextSlot := schedule.Next(fire.ScheduledAt.In(location))
	for {
		err := publisher.Publish(ctx, fire.Job, fire.Subject, fire.Payload, fire.ScheduledAt, job.Durable == nil || *job.Durable)
		if err == nil || IsPermanentPublishError(err) {
			return err
		}
		logger.Printf("job=%s scheduled=%s attempt=%d publish failed: %v; retrying in %s", fire.Job, fire.ScheduledAt.Format(time.RFC3339), attempt, err, backoff)
		wait := backoff
		if untilNext := time.Until(nextSlot); untilNext <= 0 {
			return errFireSuperseded
		} else if untilNext < wait {
			wait = untilNext
		}
		timer := time.NewTimer(wait)
		select {
		case <-ctx.Done():
			timer.Stop()
			return ctx.Err()
		case <-timer.C:
			if !time.Now().Before(nextSlot) {
				return errFireSuperseded
			}
		}
		attempt++
		backoff = nextBackoff(backoff)
	}
}
