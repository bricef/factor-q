package main

import (
	"context"
	"errors"
	"fmt"
	"strings"
	"time"

	"github.com/nats-io/nats.go"
	"github.com/nats-io/nats.go/jetstream"
)

// Publisher is the side-effect seam used by the scheduler to publish a fire.
type Publisher interface {
	Publish(ctx context.Context, job, subject string, payload []byte, scheduled time.Time, durable bool) error
}

// PublishError classifies failures for the scheduler's retry policy.
type PublishError struct {
	Permanent bool
	Err       error
}

func (e *PublishError) Error() string { return e.Err.Error() }
func (e *PublishError) Unwrap() error { return e.Err }

// IsPermanentPublishError reports whether err is a configuration error which
// cannot be repaired by retrying.
func IsPermanentPublishError(err error) bool {
	var publishErr *PublishError
	return errors.As(err, &publishErr) && publishErr.Permanent
}

type corePublisher interface {
	Publish(subject string, data []byte) error
}

// NATSPublisher publishes durable fires through JetStream and non-durable
// fires through core NATS.
type NATSPublisher struct {
	core corePublisher
	js   jetstream.Publisher
}

func NewNATSPublisher(nc *nats.Conn) (*NATSPublisher, error) {
	js, err := jetstream.New(nc)
	if err != nil {
		return nil, fmt.Errorf("create JetStream context: %w", err)
	}
	return &NATSPublisher{core: nc, js: js}, nil
}

func (p *NATSPublisher) Publish(ctx context.Context, job, subject string, payload []byte, scheduled time.Time, durable bool) error {
	if !durable {
		if err := p.core.Publish(subject, payload); err != nil {
			return &PublishError{Err: fmt.Errorf("publish to %s: %w", subject, err)}
		}
		return nil
	}

	msg := nats.NewMsg(subject)
	msg.Data = payload
	msg.Header.Set(nats.MsgIdHdr, DedupMessageID(job, scheduled))
	if _, err := p.js.PublishMsg(ctx, msg); err != nil {
		wrapped := fmt.Errorf("publish to %s: %w", subject, err)
		return &PublishError{Permanent: isNoStreamError(err), Err: wrapped}
	}
	return nil
}

func DedupMessageID(job string, scheduled time.Time) string {
	return "fq-cron/" + job + "@" + scheduled.Format(time.RFC3339)
}

func isNoStreamError(err error) bool {
	return errors.Is(err, jetstream.ErrNoStreamResponse) ||
		strings.Contains(strings.ToLower(err.Error()), "no stream matches subject")
}

// PublishedFire is one call recorded by MemoryPublisher.
type PublishedFire struct {
	Job, Subject string
	Payload      []byte
	Scheduled    time.Time
	Durable      bool
}

// MemoryPublisher is an in-memory fake. Errors are returned in order.
type MemoryPublisher struct {
	Publishes []PublishedFire
	Errors    []error
}

func (p *MemoryPublisher) Publish(_ context.Context, job, subject string, payload []byte, scheduled time.Time, durable bool) error {
	p.Publishes = append(p.Publishes, PublishedFire{job, subject, append([]byte(nil), payload...), scheduled, durable})
	if len(p.Errors) == 0 {
		return nil
	}
	err := p.Errors[0]
	p.Errors = p.Errors[1:]
	return err
}

func TransientPublishFailure(err error) error { return &PublishError{Err: err} }
func PermanentPublishFailure(err error) error { return &PublishError{Permanent: true, Err: err} }
