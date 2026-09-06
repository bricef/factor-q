package main

// connect.go — the broker connection's reconnect policy.
//
// nats.go's defaults give up: sixty reconnect attempts two seconds apart,
// then the connection is closed for good. A scheduler that is launched by
// `setsid … &` with no supervisor and outlives its broker by two minutes
// is a scheduler that never fires again, and nothing in the process says
// so. So the connection reconnects for ever, says so in the log each time
// the state changes, and startup waits for the broker rather than dying on
// the first JetStream call.
//
// adapters/github-watcher/connect.go mirrors this file (the two are
// separate Go modules by design; see the watcher README, "Why Go and why
// standalone").

import (
	"context"
	"errors"
	"fmt"
	"log"
	"time"

	"github.com/nats-io/nats.go"
)

// connectPollInterval is how often waitConnected re-reads the connection
// state. The wait only happens while the broker is unreachable, so the
// cost is a handful of atomic loads per second during an outage.
const connectPollInterval = 100 * time.Millisecond

// connectNATS dials the broker with the adapter's standing reconnect
// policy: retry the initial connect instead of failing startup, and
// reconnect for ever rather than closing the connection after sixty
// attempts. The returned connection may not be established yet — call
// waitConnected before the first request-reply call.
//
// extra options are appended last (so a test can shorten the reconnect
// interval); nothing in production passes any.
func connectNATS(url string, logger *log.Logger, extra ...nats.Option) (*nats.Conn, error) {
	if logger == nil {
		logger = log.Default()
	}
	options := []nats.Option{
		nats.RetryOnFailedConnect(true),
		nats.MaxReconnects(-1),
		// The URL may carry a token in its userinfo, so no handler ever
		// logs it: the connected URL is asked for redacted, and the
		// handlers that have no connection to ask name no address at all.
		nats.DisconnectErrHandler(func(_ *nats.Conn, err error) {
			logger.Printf("nats=disconnected err=%v: reconnecting (no attempt limit)", err)
		}),
		nats.ReconnectHandler(func(nc *nats.Conn) {
			logger.Printf("nats=reconnected url=%s", nc.ConnectedUrlRedacted())
		}),
		nats.ClosedHandler(func(*nats.Conn) {
			logger.Printf("nats=closed: the connection will not be reopened")
		}),
	}
	nc, err := nats.Connect(url, append(options, extra...)...)
	if err != nil {
		return nil, fmt.Errorf("connect to NATS: %w", err)
	}
	return nc, nil
}

// waitConnected blocks until the connection is established, ctx ends, or
// the connection is closed. RetryOnFailedConnect hands back a connection
// that is still reconnecting when the broker is down; every JetStream call
// made on one — the KV bucket the scheduler opens at startup, first of all
// — would fail on a request timeout and take the process down with it,
// which is the "two minutes without a broker and the scheduler is gone"
// failure this policy exists to remove.
func waitConnected(ctx context.Context, nc *nats.Conn, logger *log.Logger) error {
	if nc.Status() == nats.CONNECTED {
		return nil
	}
	logger.Printf("nats=%s: waiting for the broker before opening JetStream state", nc.Status())
	ticker := time.NewTicker(connectPollInterval)
	defer ticker.Stop()
	for {
		select {
		case <-ctx.Done():
			return ctx.Err()
		case <-ticker.C:
			switch nc.Status() {
			case nats.CONNECTED:
				logger.Printf("nats=connected url=%s", nc.ConnectedUrlRedacted())
				return nil
			case nats.CLOSED:
				return errors.New("NATS connection closed while waiting for the broker")
			}
		}
	}
}
