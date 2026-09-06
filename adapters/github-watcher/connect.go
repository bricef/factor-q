package main

// connect.go — the broker connection's reconnect policy.
//
// nats.go's defaults give up: sixty reconnect attempts two seconds apart,
// then the connection is closed for good. The watcher does not exit when
// that happens — it keeps polling GitHub with a dead connection, claiming
// issues it can no longer trigger and reverting them a moment later, while
// the outcome subscriptions that would have moved them on are gone. A
// process that stays up is a process compose's restart policy cannot
// rescue and the container's HEALTHCHECK reports on but no one reads
// until the labels look wrong: a silent wedge, and the worse of the two
// adapters' failures. So the connection reconnects for ever, says so in
// the log each time the state changes, and the poll loop refuses to touch
// a label while it is down (watcher.go, pollOnce).
//
// adapters/fq-cron/connect.go mirrors this file (the two are separate Go
// modules by design; see the README, "Why Go and why standalone").

import (
	"fmt"
	"log/slog"
	"strings"

	"github.com/nats-io/nats.go"
)

// connectNATS dials the broker with the adapter's standing reconnect
// policy: retry the initial connect instead of failing startup, and
// reconnect for ever rather than closing the connection after sixty
// attempts. The returned connection may not be established yet — the poll
// loop skips a cycle while it is not (Watcher.Connected).
//
// extra options are appended last (so a test can shorten the reconnect
// interval); nothing in production passes any.
func connectNATS(url string, log *slog.Logger, extra ...nats.Option) (*nats.Conn, error) {
	options := []nats.Option{
		nats.RetryOnFailedConnect(true),
		nats.MaxReconnects(-1),
		// No pending buffer. nats.go otherwise holds up to 8 MB of
		// publishes made while reconnecting and flushes them on the way
		// back, which turns a mid-cycle disconnect into a double trigger:
		// the JetStream publish blocks until its ack times out, the
		// watcher reverts the issue to `ready` believing it failed, the
		// buffered trigger flushes on reconnect, and the next cycle
		// claims and publishes the same issue again. Failing the publish
		// immediately makes the revert honest.
		nats.ReconnectBufSize(-1),
		// The URL may carry a token in its userinfo, so no handler ever
		// logs it: the connected URL is asked for redacted, and the
		// handlers that have no connection to ask name no address at all.
		nats.DisconnectErrHandler(func(_ *nats.Conn, err error) {
			log.Warn("nats disconnected; reconnecting (no attempt limit)", "err", err)
		}),
		nats.ReconnectHandler(func(nc *nats.Conn) {
			log.Info("nats reconnected", "url", nc.ConnectedUrlRedacted())
		}),
		// nats.go calls this on our own Close() too, so a clean shutdown
		// would otherwise log an error every time. Only a connection that
		// died of something is one.
		nats.ClosedHandler(func(nc *nats.Conn) {
			if err := nc.LastError(); err != nil {
				log.Error("nats connection closed; it will not be reopened", "err", err)
				return
			}
			log.Info("nats connection closed")
		}),
	}
	nc, err := nats.Connect(url, append(options, extra...)...)
	if err != nil {
		return nil, fmt.Errorf("connect to NATS at %s: %w", redactURL(url), err)
	}
	return nc, nil
}

// redactURL hides any userinfo (a broker token) in a URL, so the one
// place that still names an address — a failed dial, which has no
// connection to ask for a redacted URL — can do so without publishing the
// credential. net/url's own Redacted masks only the password half, and a
// NATS token lives in the username.
func redactURL(raw string) string {
	scheme, rest, hasScheme := strings.Cut(raw, "://")
	if !hasScheme {
		rest = raw
	}
	if at := strings.LastIndex(rest, "@"); at >= 0 {
		rest = "[redacted]@" + rest[at+1:]
	}
	if !hasScheme {
		return rest
	}
	return scheme + "://" + rest
}
