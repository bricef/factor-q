package main

// health.go — the liveness probe the container image's HEALTHCHECK runs
// (ADR-0035 clause 8). The process serves `GET /healthz` on a loopback
// address; `fq-cron --probe`, run by the HEALTHCHECK from the same
// environment, asks it and exits 0 on 200. The image is distroless — no
// wget, no curl, no shell — so the binary is its own probe.
//
// Healthy means the NATS connection is up. The scheduler loop is not
// age-checked: between fires it legitimately sleeps for as long as the
// schedule says, and a loop that exits on error ends the process, which
// the supervisor sees directly.
//
// Mirrors adapters/github-watcher/health.go (the two are separate Go
// modules by design; see the watcher README, "Why Go and why standalone");
// the loop-age check is kept so the two files stay one file.

import (
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"log"
	"net"
	"net/http"
	"sync/atomic"
	"time"

	"github.com/nats-io/nats.go"
)

const (
	// healthBindEnv names the loopback address the endpoint serves on and
	// the probe asks; empty disables both.
	healthBindEnv     = "FQCRON_HEALTH_BIND"
	defaultHealthBind = "127.0.0.1:9474"
	healthPath        = "/healthz"
	probeTimeout      = 3 * time.Second
)

// Health is what the endpoint reports on. Zero-valued it reports
// healthy: a check is only made for the inputs that were wired.
type Health struct {
	// connStatus reports the broker connection's state; nil = not checked.
	connStatus func() nats.Status
	// maxAge bounds how long ago the last Tick may have been; 0 = not
	// checked (a process whose loop sleeps for hours legitimately).
	maxAge time.Duration
	// lastTick is the Unix time in nanoseconds of the last loop iteration;
	// 0 = none yet.
	lastTick atomic.Int64
	now      func() time.Time
}

// NewHealth wires the broker connection and the loop-age bound. nc may be
// nil (then the connection is not checked).
func NewHealth(nc *nats.Conn, maxAge time.Duration) *Health {
	h := &Health{maxAge: maxAge, now: time.Now}
	if nc != nil {
		h.connStatus = nc.Status
	}
	return h
}

// Tick records that the loop completed an iteration.
func (h *Health) Tick() { h.lastTick.Store(h.now().UnixNano()) }

// report is the verdict and the reasons, one entry per input checked.
// Every entry is present whether or not it passes, so the body says what
// was looked at.
func (h *Health) report() (bool, map[string]string) {
	ok := true
	out := map[string]string{}
	if h.connStatus != nil {
		s := h.connStatus()
		out["nats"] = s.String()
		if s != nats.CONNECTED {
			ok = false
		}
	}
	if h.maxAge > 0 {
		last := h.lastTick.Load()
		if last == 0 {
			out["last_cycle"] = "none yet"
			ok = false
		} else {
			age := h.now().Sub(time.Unix(0, last))
			out["last_cycle"] = age.Truncate(time.Second).String() + " ago"
			if age > h.maxAge {
				ok = false
			}
		}
	}
	if ok {
		out["status"] = "ok"
	} else {
		out["status"] = "unhealthy"
	}
	return ok, out
}

// ServeHTTP answers GET or HEAD on the health path with 200 and a small
// JSON body, or 503 with the same body naming what failed. Anything else
// is 404 or 405 — this is a probe, not an API.
func (h *Health) ServeHTTP(w http.ResponseWriter, r *http.Request) {
	if r.URL.Path != healthPath {
		http.NotFound(w, r)
		return
	}
	if r.Method != http.MethodGet && r.Method != http.MethodHead {
		w.Header().Set("Allow", "GET, HEAD")
		http.Error(w, "method not allowed", http.StatusMethodNotAllowed)
		return
	}
	ok, body := h.report()
	w.Header().Set("Content-Type", "application/json")
	w.Header().Set("Cache-Control", "no-store")
	if !ok {
		w.WriteHeader(http.StatusServiceUnavailable)
	}
	_ = json.NewEncoder(w).Encode(body)
}

// listenHealth binds the endpoint. It refuses a non-loopback address: the
// probe is for the supervisor on the same host or in the same container,
// and the endpoint says nothing worth publishing but would be one more
// port to defend. Bound synchronously so a port already taken fails
// startup instead of leaving a running process that shows unhealthy for
// ever.
func listenHealth(bind string) (net.Listener, error) {
	host, _, err := net.SplitHostPort(bind)
	if err != nil {
		return nil, fmt.Errorf("%s=%q: %w", healthBindEnv, bind, err)
	}
	if ip := net.ParseIP(host); host != "localhost" && (ip == nil || !ip.IsLoopback()) {
		return nil, fmt.Errorf("%s=%q is not a loopback address — the health endpoint is for the container's own probe", healthBindEnv, bind)
	}
	ln, err := net.Listen("tcp", bind)
	if err != nil {
		return nil, fmt.Errorf("health endpoint: %w", err)
	}
	return ln, nil
}

// serveHealth serves the endpoint on ln until ctx is done, then closes
// it. Returns nil on a clean stop.
func serveHealth(ctx context.Context, ln net.Listener, h *Health, logger *log.Logger) error {
	srv := &http.Server{Handler: h, ReadHeaderTimeout: probeTimeout}
	go func() {
		<-ctx.Done()
		shutdownCtx, cancel := context.WithTimeout(context.Background(), time.Second)
		defer cancel()
		_ = srv.Shutdown(shutdownCtx)
	}()
	logger.Printf("health endpoint serving on http://%s%s", ln.Addr(), healthPath)
	if err := srv.Serve(ln); err != nil && !errors.Is(err, http.ErrServerClosed) {
		return err
	}
	return nil
}

// probe asks the running process at bind and returns nil on 200. The
// error carries the body, so a failing HEALTHCHECK's output (`docker
// inspect`) says why.
func probe(bind string) error {
	client := &http.Client{Timeout: probeTimeout}
	resp, err := client.Get("http://" + bind + healthPath)
	if err != nil {
		return fmt.Errorf("probe %s: %w", bind, err)
	}
	defer resp.Body.Close()
	body, _ := io.ReadAll(io.LimitReader(resp.Body, 4096))
	if resp.StatusCode != http.StatusOK {
		return fmt.Errorf("probe %s: %s: %s", bind, resp.Status, string(body))
	}
	fmt.Print(string(body))
	return nil
}
