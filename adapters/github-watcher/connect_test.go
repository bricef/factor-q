package main

import (
	"bytes"
	"context"
	"fmt"
	"log/slog"
	"net"
	"os"
	"os/exec"
	"path/filepath"
	"slices"
	"strings"
	"sync"
	"testing"
	"time"

	"github.com/nats-io/nats.go"
)

// --- private-broker helpers ---

// natsServerBinary returns the pinned nats-server the gate provisions
// (`just install-nats`), or skips: a bare `go test` need not have it.
func natsServerBinary(t *testing.T) string {
	t.Helper()
	server := os.Getenv("FQ_TEST_NATS_SERVER")
	if server == "" {
		server = "../../.tools/nats-server"
	}
	if _, err := os.Stat(server); err != nil {
		t.Skipf("private nats-server unavailable (%s): %v", server, err)
	}
	return server
}

// freePort reserves an ephemeral port and releases it, so a broker can be
// started — and, for the reconnect test, restarted — on a known port.
func freePort(t *testing.T) int {
	t.Helper()
	listener, err := net.Listen("tcp", "127.0.0.1:0")
	if err != nil {
		t.Fatal(err)
	}
	defer listener.Close()
	return listener.Addr().(*net.TCPAddr).Port
}

// startBroker starts a JetStream broker on port and returns a stop func.
// The stop func is idempotent and also runs at test cleanup, so a test can
// stop the broker mid-way to simulate an outage.
func startBroker(t *testing.T, binary string, port int) (stop func()) {
	t.Helper()
	cmd := exec.Command(binary, "-js", "-p", fmt.Sprint(port), "-sd", filepath.Join(t.TempDir(), "nats"))
	if err := cmd.Start(); err != nil {
		t.Fatal(err)
	}
	var once sync.Once
	stop = func() {
		once.Do(func() {
			_ = cmd.Process.Kill()
			_ = cmd.Wait()
		})
	}
	t.Cleanup(stop)
	return stop
}

// waitStatus waits for the connection to reach want, failing the test with
// the status it was stuck in.
func waitStatus(t *testing.T, nc *nats.Conn, want nats.Status, within time.Duration) {
	t.Helper()
	deadline := time.Now().Add(within)
	for time.Now().Before(deadline) {
		if nc.Status() == want {
			return
		}
		time.Sleep(20 * time.Millisecond)
	}
	t.Fatalf("connection status = %s after %s, want %s", nc.Status(), within, want)
}

// syncBuffer is a log sink a test goroutine may read while the nats.go
// callbacks write to it.
type syncBuffer struct {
	mu  sync.Mutex
	buf bytes.Buffer
}

func (b *syncBuffer) Write(p []byte) (int, error) {
	b.mu.Lock()
	defer b.mu.Unlock()
	return b.buf.Write(p)
}

func (b *syncBuffer) String() string {
	b.mu.Lock()
	defer b.mu.Unlock()
	return b.buf.String()
}

// waitForLog waits for want to appear in the captured log.
func waitForLog(t *testing.T, logs *syncBuffer, want string, within time.Duration) {
	t.Helper()
	deadline := time.Now().Add(within)
	for time.Now().Before(deadline) {
		if strings.Contains(logs.String(), want) {
			return
		}
		time.Sleep(20 * time.Millisecond)
	}
	t.Fatalf("log did not contain %q within %s; log was:\n%s", want, within, logs.String())
}

// fastReconnect shortens the reconnect interval so a test can outlast the
// sixty attempts nats.go's default policy would have given up after,
// without waiting the two minutes that would take in production.
func fastReconnect() []nats.Option {
	return []nats.Option{nats.ReconnectWait(20 * time.Millisecond), nats.ReconnectJitter(0, 0)}
}

// --- the policy ---

// A broker that is not up yet must be a wait, not a startup failure: the
// deploy launches every process at once and only the daemon waits for the
// broker's health endpoint.
func TestPublisherWaitsForABrokerThatIsNotUpYet(t *testing.T) {
	binary := natsServerBinary(t)
	port := freePort(t)
	logs := &syncBuffer{}
	pub, err := NewNatsTriggerPublisher(fmt.Sprintf("nats://127.0.0.1:%d", port), slog.New(slog.NewTextHandler(logs, nil)), fastReconnect()...)
	if err != nil {
		t.Fatalf("connect against a down broker must not fail: %v", err)
	}
	defer pub.Close()
	if pub.Connected() {
		t.Fatal("publisher reports connected with no broker running")
	}

	startBroker(t, binary, port)
	waitStatus(t, pub.Conn(), nats.CONNECTED, 15*time.Second)
	if !pub.Connected() {
		t.Fatal("publisher still reports disconnected after the broker appeared")
	}
}

// Connected is checked once per cycle, so a disconnect *mid*-cycle still
// reaches the publish. It must fail there and then: with nats.go's 8 MB
// pending buffer the trigger would sit in it, the publish would block
// until its ack timed out, the watcher would revert the issue believing
// it failed — and the buffered trigger would flush on reconnect, so the
// next cycle claims and publishes it a second time.
func TestPublishWhileDisconnectedFailsInsteadOfBuffering(t *testing.T) {
	port := freePort(t) // nothing is listening on it
	pub, err := NewNatsTriggerPublisher(fmt.Sprintf("nats://127.0.0.1:%d", port), discardLogger(), fastReconnect()...)
	if err != nil {
		t.Fatal(err)
	}
	defer pub.Close()

	started := time.Now()
	err = pub.Publish(context.Background(), "m0-issue-fix", TriggerPayload{Task: "issue #1"})
	if err == nil {
		t.Fatal("a publish on a disconnected connection must fail, not be buffered for later delivery")
	}
	// The JetStream ack timeout is five seconds; failing fast is the
	// point, so anything near it means the message was buffered.
	if took := time.Since(started); took > 2*time.Second {
		t.Errorf("publish took %s to fail; it was buffered rather than refused", took)
	}
}

// nats.go's default policy gives up after sixty reconnect attempts and
// closes the connection for good — two minutes of broker downtime and the
// watcher polls GitHub for ever with a dead connection. MaxReconnects(-1)
// means the outage can outlast any number of attempts.
func TestReconnectOutlastsSixtyAttempts(t *testing.T) {
	binary := natsServerBinary(t)
	port := freePort(t)
	stop := startBroker(t, binary, port)
	logs := &syncBuffer{}
	pub, err := NewNatsTriggerPublisher(fmt.Sprintf("nats://127.0.0.1:%d", port), slog.New(slog.NewTextHandler(logs, nil)), fastReconnect()...)
	if err != nil {
		t.Fatal(err)
	}
	defer pub.Close()
	waitStatus(t, pub.Conn(), nats.CONNECTED, 15*time.Second)

	stop()
	waitForLog(t, logs, "nats disconnected", 5*time.Second)
	// 20ms reconnect interval, no jitter: three seconds is more than the
	// sixty attempts the default policy allows, after which a default
	// connection is CLOSED and never comes back.
	time.Sleep(3 * time.Second)
	if pub.Conn().IsClosed() {
		t.Fatal("connection closed itself during the outage; MaxReconnects(-1) is not in force")
	}
	if pub.Connected() {
		t.Fatal("publisher reports connected with the broker stopped")
	}

	startBroker(t, binary, port)
	waitStatus(t, pub.Conn(), nats.CONNECTED, 30*time.Second)
	waitForLog(t, logs, "nats reconnected", 5*time.Second)
}

// A cycle that finds the broker down must not touch a label: claiming an
// issue it cannot trigger relabels ready → in-progress, fails to publish,
// reverts, and repeats every cycle.
func TestPollOnceSkipsWholeCycleWhileDisconnected(t *testing.T) {
	rec := &recorder{}
	logs := &syncBuffer{}
	connected := false
	w := &Watcher{
		Source:    &fakeSource{rec: rec, issues: []Issue{{1, []string{"ready"}}}},
		Publisher: &fakePublisher{rec: rec},
		Reviewer:  &labelSource{inReview: []Issue{{2, []string{"in-review"}}}, merged: map[int]bool{2: true}},
		Config:    testConfig(),
		Log:       slog.New(slog.NewTextHandler(logs, nil)),
		Connected: func() bool { return connected },
	}
	w.Config.InReviewLabel, w.Config.DoneLabel = "in-review", "done"

	if err := w.pollOnce(context.Background()); err != nil {
		t.Fatalf("pollOnce: %v", err)
	}
	if len(rec.ops) != 0 {
		t.Fatalf("ops = %v while disconnected, want none (no claim, no trigger, no sweep)", rec.ops)
	}
	if !strings.Contains(logs.String(), "broker disconnected") {
		t.Errorf("a skipped cycle must say so; log was: %s", logs.String())
	}

	connected = true
	if err := w.pollOnce(context.Background()); err != nil {
		t.Fatalf("pollOnce: %v", err)
	}
	want := []string{
		"relabel #1 ready->in-progress",
		`publish m0-issue-fix "issue #1"`,
		"relabel #2 in-review->done",
	}
	if !slices.Equal(rec.ops, want) {
		t.Errorf("ops after reconnect =\n  %v\nwant\n  %v", rec.ops, want)
	}
}

func TestRedactURL(t *testing.T) {
	for _, tc := range []struct{ in, want string }{
		{"nats://127.0.0.1:4222", "nats://127.0.0.1:4222"},
		{"nats://s3cr3t@127.0.0.1:4222", "nats://[redacted]@127.0.0.1:4222"},
		{"nats://user:s3cr3t@127.0.0.1:4222", "nats://[redacted]@127.0.0.1:4222"},
		{"127.0.0.1:4222", "127.0.0.1:4222"},
	} {
		if got := redactURL(tc.in); got != tc.want {
			t.Errorf("redactURL(%q) = %q, want %q", tc.in, got, tc.want)
		}
	}
}
