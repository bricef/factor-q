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
		state, err := loadState(ctx, config, store)
		if err != nil {
			return err
		}
		fires := plan(time.Now(), JobSet{Jobs: config.Jobs, MaxFiresPerHour: config.Limits.MaxFiresPerHour}, state)
		if len(fires) == 0 {
			select {
			case <-ctx.Done():
				return nil
			case event, ok := <-reloads:
				if !ok {
					reloads = nil
					continue
				}
				if err := removeState(ctx, event.Diff.Removed, store); err != nil {
					return err
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
					if err := removeState(ctx, event.Diff.Removed, store); err != nil {
						return err
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
					if err := store.Put(ctx, fire.Job, FireState{LastScheduled: fire.ScheduledAt}); err != nil {
						return fmt.Errorf("record superseded fire %q: %w", fire.Job, err)
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
			if err := store.Put(ctx, fire.Job, record); err != nil {
				return fmt.Errorf("record acknowledged fire %q: %w", fire.Job, err)
			}
			logger.Printf("job=%s scheduled=%s published", fire.Job, fire.ScheduledAt.Format(time.RFC3339))
		}
	}
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
		if backoff < maximumRetryBackoff {
			backoff *= 2
			if backoff > maximumRetryBackoff {
				backoff = maximumRetryBackoff
			}
		}
	}
}
