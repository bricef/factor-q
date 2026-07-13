package main

import (
	"context"
	"encoding/json"
	"fmt"

	"github.com/nats-io/nats.go"
	"github.com/nats-io/nats.go/jetstream"
)

// NatsTriggerPublisher publishes triggers to the `fq-triggers` JetStream
// stream per the trigger wire contract
// (docs/design/committed/trigger-wire-contract.md). It owns the NATS
// connection.
type NatsTriggerPublisher struct {
	nc *nats.Conn
	js jetstream.JetStream
}

// NewNatsTriggerPublisher connects to NATS at url and opens a JetStream
// context. The caller must Close it.
func NewNatsTriggerPublisher(url string) (*NatsTriggerPublisher, error) {
	nc, err := nats.Connect(url)
	if err != nil {
		return nil, fmt.Errorf("connect to NATS at %s: %w", url, err)
	}
	js, err := jetstream.New(nc)
	if err != nil {
		nc.Close()
		return nil, fmt.Errorf("create JetStream context: %w", err)
	}
	return &NatsTriggerPublisher{nc: nc, js: js}, nil
}

// Publish publishes a trigger for agentID with the given payload, per the
// wire contract: subject `fq.trigger.<agentID>`, a JSON-value body (here a
// JSON object), awaiting the JetStream ack for durability.
func (p *NatsTriggerPublisher) Publish(ctx context.Context, agentID string, payload TriggerPayload) error {
	subject := triggerSubject(agentID)
	// The contract's body is a JSON value; the convention payload is an
	// object. json.Marshal produces exactly that.
	body, err := json.Marshal(payload)
	if err != nil {
		return fmt.Errorf("marshal payload: %w", err)
	}
	if _, err := p.js.Publish(ctx, subject, body); err != nil {
		return fmt.Errorf("publish to %s: %w", subject, err)
	}
	return nil
}

// Conn returns the underlying NATS connection so the outcome observer can
// share it (one connection for both publishing triggers and subscribing to
// outcomes).
func (p *NatsTriggerPublisher) Conn() *nats.Conn { return p.nc }

// Close closes the NATS connection.
func (p *NatsTriggerPublisher) Close() {
	if p.nc != nil {
		p.nc.Close()
	}
}

// triggerSubject returns the trigger subject for an agent, per the wire
// contract: `fq.trigger.<agentID>`.
func triggerSubject(agentID string) string {
	return "fq.trigger." + agentID
}
