package main

import (
	"bytes"
	"context"
	"fmt"
	"io"
	"log"
	"net"
	"os"
	"os/exec"
	"path/filepath"
	"strings"
	"sync"
	"sync/atomic"
	"testing"
	"time"

	"github.com/nats-io/nats.go"
)

// --- private-broker helpers, shared with integration_test.go ---

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

// countingReconnect shortens the reconnect interval and records how many
// attempts nats.go has made since the disconnect, so a test can wait for
// the sixtieth rather than sleep for a duration that ought to contain it:
// under load a fixed sleep can deliver fewer than sixty attempts and pass
// without ever testing the limit.
func countingReconnect(attempts *atomic.Int64) []nats.Option {
	return []nats.Option{
		nats.ReconnectJitter(0, 0),
		nats.CustomReconnectDelay(func(attempt int) time.Duration {
			attempts.Store(int64(attempt))
			return 20 * time.Millisecond
		}),
	}
}

// waitAttempts waits for at least n reconnect attempts to have been made.
func waitAttempts(t *testing.T, attempts *atomic.Int64, n int64, within time.Duration) {
	t.Helper()
	deadline := time.Now().Add(within)
	for time.Now().Before(deadline) {
		if attempts.Load() >= n {
			return
		}
		time.Sleep(10 * time.Millisecond)
	}
	t.Fatalf("only %d reconnect attempts in %s, want at least %d", attempts.Load(), within, n)
}

// --- the policy ---

// A broker that is not up yet must be a wait, not a startup failure: the
// deploy launches every process at once and only the daemon waits for the
// broker's health endpoint.
func TestConnectWaitsForABrokerThatIsNotUpYet(t *testing.T) {
	binary := natsServerBinary(t)
	port := freePort(t)
	logs := &syncBuffer{}
	nc, err := connectNATS(fmt.Sprintf("nats://127.0.0.1:%d", port), log.New(logs, "", 0), fastReconnect()...)
	if err != nil {
		t.Fatalf("connect against a down broker must not fail: %v", err)
	}
	defer nc.Close()
	if nc.Status() == nats.CONNECTED {
		t.Fatalf("status = %s with no broker running", nc.Status())
	}

	startBroker(t, binary, port)
	ctx, cancel := context.WithTimeout(context.Background(), 15*time.Second)
	defer cancel()
	if err := waitConnected(ctx, nc, log.New(logs, "", 0)); err != nil {
		t.Fatalf("waitConnected: %v (log: %s)", err, logs.String())
	}
	if nc.Status() != nats.CONNECTED {
		t.Fatalf("status = %s after waitConnected", nc.Status())
	}

	nc.Close()
	waitForLog(t, logs, "nats=closed", 5*time.Second)
}

// A publish made while the connection is down must fail there and then.
// nats.go's 8 MB pending buffer would otherwise hold it and flush it on
// reconnect — a fire recorded as failed and delivered anyway, outside the
// retry policy that is supposed to decide what is re-sent.
func TestPublishWhileDisconnectedFailsInsteadOfBuffering(t *testing.T) {
	port := freePort(t) // nothing is listening on it
	nc, err := connectNATS(fmt.Sprintf("nats://127.0.0.1:%d", port), log.New(io.Discard, "", 0), fastReconnect()...)
	if err != nil {
		t.Fatal(err)
	}
	defer nc.Close()
	if err := nc.Publish("cron.test", []byte("fire")); err == nil {
		t.Fatal("a publish on a disconnected connection must fail, not be buffered for later delivery")
	}
}

// nats.go's default policy gives up after sixty reconnect attempts and
// closes the connection for good — two minutes of broker downtime and the
// scheduler is a process that will never fire again. MaxReconnects(-1)
// means the outage can outlast any number of attempts.
func TestReconnectOutlastsSixtyAttempts(t *testing.T) {
	binary := natsServerBinary(t)
	port := freePort(t)
	stop := startBroker(t, binary, port)
	logs := &syncBuffer{}
	var attempts atomic.Int64
	nc, err := connectNATS(fmt.Sprintf("nats://127.0.0.1:%d", port), log.New(logs, "", 0), countingReconnect(&attempts)...)
	if err != nil {
		t.Fatal(err)
	}
	defer nc.Close()
	waitStatus(t, nc, nats.CONNECTED, 15*time.Second)

	stop()
	waitForLog(t, logs, "nats=disconnected", 5*time.Second)
	// Wait for the sixty-first attempt to have actually happened, rather
	// than for a stretch of time that ought to contain it.
	waitAttempts(t, &attempts, 61, 30*time.Second)
	if nc.IsClosed() {
		t.Fatalf("connection closed itself after %d attempts; MaxReconnects(-1) is not in force", attempts.Load())
	}

	startBroker(t, binary, port)
	waitStatus(t, nc, nats.CONNECTED, 30*time.Second)
	waitForLog(t, logs, "nats=reconnected", 5*time.Second)
}

// The state store is the broker, so a store that cannot be read must not
// end the scheduler: it is the very first call of every iteration, and
// returning its error is how an outage used to kill the process.
func TestSchedulerSurvivesAnUnreadableStateStore(t *testing.T) {
	store := &flakyStore{MemoryStateStore: NewMemoryStateStore(), failures: 2}
	job := Job{Name: "job", Schedule: "@every 1m", Subject: "cron.test", TZ: "UTC", CatchUp: "skip", Enabled: boolPtr(true), Durable: boolPtr(false)}
	config := &Config{Limits: Limits{MaxFiresPerHour: 10}, Jobs: []Job{job}}
	ctx, cancel := context.WithTimeout(context.Background(), 10*time.Second)
	defer cancel()

	logs := &syncBuffer{}
	done := make(chan error, 1)
	go func() {
		done <- runScheduler(ctx, config, nil, &orderedPublisher{order: new([]string)}, store, removalPolicy{}, log.New(logs, "", 0))
	}()

	// The store recovers on its third read; the scheduler must then be
	// waiting for the job's slot rather than gone.
	waitForLog(t, logs, "load state: recovered after 3 attempts", 5*time.Second)
	cancel()
	select {
	case err := <-done:
		if err != nil {
			t.Fatalf("runScheduler = %v, want a clean stop", err)
		}
	case <-time.After(5 * time.Second):
		t.Fatal("runScheduler did not stop on cancellation")
	}
}

// flakyStore fails its first `failures` reads, as an unreachable JetStream
// KV does.
type flakyStore struct {
	*MemoryStateStore
	mu       sync.Mutex
	failures int
}

func (s *flakyStore) Get(ctx context.Context, job string) (FireState, bool, error) {
	s.mu.Lock()
	if s.failures > 0 {
		s.failures--
		s.mu.Unlock()
		return FireState{}, false, fmt.Errorf("nats: no responders available for request")
	}
	s.mu.Unlock()
	return s.MemoryStateStore.Get(ctx, job)
}
