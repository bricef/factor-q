package main

import (
	"context"
	"fmt"
	"net"
	"os"
	"os/exec"
	"path/filepath"
	"testing"
	"time"

	"github.com/nats-io/nats.go"
	"github.com/nats-io/nats.go/jetstream"
)

func TestNATSIntegration(t *testing.T) {
	server := os.Getenv("FQ_TEST_NATS_SERVER")
	if server == "" {
		server = "../../.tools/nats-server"
	}
	if _, err := os.Stat(server); err != nil {
		t.Skipf("private nats-server unavailable (%s): %v", server, err)
	}
	listener, err := net.Listen("tcp", "127.0.0.1:0")
	if err != nil {
		t.Fatal(err)
	}
	port := listener.Addr().(*net.TCPAddr).Port
	listener.Close()
	url := fmt.Sprintf("nats://127.0.0.1:%d", port)
	cmd := exec.Command(server, "-js", "-p", fmt.Sprint(port), "-sd", filepath.Join(t.TempDir(), "nats"))
	if err := cmd.Start(); err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { _ = cmd.Process.Kill(); _ = cmd.Wait() })

	var nc *nats.Conn
	deadline := time.Now().Add(5 * time.Second)
	for time.Now().Before(deadline) {
		nc, err = nats.Connect(url, nats.Timeout(100*time.Millisecond))
		if err == nil {
			break
		}
		time.Sleep(25 * time.Millisecond)
	}
	if err != nil {
		t.Fatalf("connect to private broker: %v", err)
	}
	defer nc.Close()
	ctx, cancel := context.WithTimeout(context.Background(), 5*time.Second)
	defer cancel()
	js, err := jetstream.New(nc)
	if err != nil {
		t.Fatal(err)
	}
	stream, err := js.CreateStream(ctx, jetstream.StreamConfig{Name: "CRON_TEST", Subjects: []string{"cron.durable"}, Duplicates: time.Minute})
	if err != nil {
		t.Fatal(err)
	}

	publisher, err := NewNATSPublisher(nc)
	if err != nil {
		t.Fatal(err)
	}
	slot := time.Date(2026, 7, 18, 0, 0, 0, 0, time.UTC)
	if err := publisher.Publish(ctx, "job", "cron.durable", []byte("one"), slot, true); err != nil {
		t.Fatal(err)
	}
	if err := publisher.Publish(ctx, "job", "cron.durable", []byte("one"), slot, true); err != nil {
		t.Fatal(err)
	}
	info, err := stream.Info(ctx)
	if err != nil {
		t.Fatal(err)
	}
	if info.State.Msgs != 1 {
		t.Fatalf("deduplication retained %d messages, want 1", info.State.Msgs)
	}
	message, err := stream.GetMsg(ctx, 1)
	if err != nil || string(message.Data) != "one" {
		t.Fatalf("durable round-trip: message=%v err=%v", message, err)
	}

	store, err := NewKVStateStore(ctx, js, "CRON_TEST_STATE")
	if err != nil {
		t.Fatal(err)
	}
	want := FireState{LastScheduled: slot, PublishedAt: slot.Add(time.Second)}
	if err := store.Put(ctx, "job", want); err != nil {
		t.Fatal(err)
	}
	got, exists, err := store.Get(ctx, "job")
	if err != nil || !exists || !got.LastScheduled.Equal(want.LastScheduled) || !got.PublishedAt.Equal(want.PublishedAt) {
		t.Fatalf("KV round-trip: got=%+v exists=%v err=%v", got, exists, err)
	}

	sub, err := nc.SubscribeSync("cron.core")
	if err != nil {
		t.Fatal(err)
	}
	if err := nc.Flush(); err != nil {
		t.Fatal(err)
	}
	if err := publisher.Publish(ctx, "core-job", "cron.core", []byte("live"), slot, false); err != nil {
		t.Fatal(err)
	}
	msg, err := sub.NextMsg(time.Second)
	if err != nil || string(msg.Data) != "live" {
		t.Fatalf("core publish: message=%v err=%v", msg, err)
	}
}
