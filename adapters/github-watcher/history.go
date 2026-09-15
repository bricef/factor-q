package main

import (
	"context"
	"fmt"
	"time"

	"github.com/nats-io/nats.go"
	"github.com/nats-io/nats.go/jetstream"
)

const eventStream = "fq-events"

// NatsInvocationHistory queries the retained event stream with a short-lived
// ordered consumer. This is a point-in-time read used by the poll backstop,
// not a competing durable consumer; the live outcome path remains core NATS.
type NatsInvocationHistory struct {
	nc      *nats.Conn
	decoder *NatsOutcomeSource
}

func NewNatsInvocationHistory(nc *nats.Conn, decoder *NatsOutcomeSource) *NatsInvocationHistory {
	return &NatsInvocationHistory{nc: nc, decoder: decoder}
}

// Events returns retained lifecycle events for agentID in stream order. The
// event stream itself is the same durable ground truth projected by
// `fq events query`; filtering at the server keeps each poll bounded to this
// watcher's target agent.
func (h *NatsInvocationHistory) Events(ctx context.Context, agentID string) ([]OutcomeEvent, error) {
	js, err := jetstream.New(h.nc)
	if err != nil {
		return nil, fmt.Errorf("open JetStream context: %w", err)
	}
	stream, err := js.Stream(ctx, eventStream)
	if err != nil {
		return nil, fmt.Errorf("open %s stream: %w", eventStream, err)
	}
	start := time.Now().Add(-30 * 24 * time.Hour) // matches fq-events retention
	consumer, err := stream.OrderedConsumer(ctx, jetstream.OrderedConsumerConfig{
		FilterSubjects: []string{
			fmt.Sprintf("fq.agent.%s.triggered", agentID),
			fmt.Sprintf("fq.agent.%s.completed", agentID),
			fmt.Sprintf("fq.agent.%s.failed", agentID),
			fmt.Sprintf("fq.agent.%s.invocation.ambiguous", agentID),
		},
		DeliverPolicy: jetstream.DeliverByStartTimePolicy,
		OptStartTime:  &start,
	})
	if err != nil {
		return nil, fmt.Errorf("create event history query: %w", err)
	}
	info, err := consumer.Info(ctx)
	if err != nil {
		return nil, fmt.Errorf("inspect event history query: %w", err)
	}
	remaining := info.NumPending
	events := make([]OutcomeEvent, 0, remaining)
	for remaining > 0 {
		batchSize := remaining
		if batchSize > 2000 {
			batchSize = 2000
		}
		batch, err := consumer.Fetch(int(batchSize), jetstream.FetchMaxWait(5*time.Second))
		if err != nil {
			return nil, fmt.Errorf("fetch retained events: %w", err)
		}
		var received uint64
		for msg := range batch.Messages() {
			received++
			nm := &nats.Msg{Subject: msg.Subject(), Data: msg.Data(), Header: msg.Headers()}
			if ev, ok := h.decoder.decode(nm); ok {
				events = append(events, ev)
			}
		}
		if err := batch.Error(); err != nil {
			return nil, fmt.Errorf("fetch retained events: %w", err)
		}
		if received == 0 {
			return nil, fmt.Errorf("event history query returned no messages with %d pending", remaining)
		}
		if received >= remaining {
			break
		}
		remaining -= received
	}
	return events, nil
}
