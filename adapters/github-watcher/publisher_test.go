package main

import (
	"context"
	"fmt"
	"log/slog"
	"strings"
	"testing"
	"time"

	"github.com/google/uuid"
	"github.com/nats-io/nats.go"
	"github.com/nats-io/nats.go/jetstream"
)

func TestPublisherStampsUniqueTraceHeaders(t *testing.T) {
	binary := natsServerBinary(t)
	port := freePort(t)
	startBroker(t, binary, port)

	pub, err := NewNatsTriggerPublisher(fmt.Sprintf("nats://127.0.0.1:%d", port), discardLogger(), nats.Timeout(5*time.Second))
	if err != nil {
		t.Fatal(err)
	}
	defer pub.Close()
	waitStatus(t, pub.Conn(), nats.CONNECTED, 5*time.Second)

	ctx, cancel := context.WithTimeout(context.Background(), 10*time.Second)
	defer cancel()
	js, err := jetstream.New(pub.Conn())
	if err != nil {
		t.Fatal(err)
	}
	stream, err := js.CreateStream(ctx, jetstream.StreamConfig{Name: "fq-triggers", Subjects: []string{"fq.trigger.>"}})
	if err != nil {
		t.Fatal(err)
	}

	subject := triggerSubject("m0-issue-fix")
	sub, err := pub.Conn().SubscribeSync(subject)
	if err != nil {
		t.Fatal(err)
	}
	if err := pub.Conn().Flush(); err != nil {
		t.Fatal(err)
	}

	payload := TriggerPayload{Task: "issue #812"}
	payload.GitHub.Issue = 812
	var ids []string
	for range 2 {
		returnedID, err := pub.Publish(ctx, "m0-issue-fix", payload)
		if err != nil {
			t.Fatal(err)
		}
		msg, err := sub.NextMsg(5 * time.Second)
		if err != nil {
			t.Fatal(err)
		}
		wireID := msg.Header.Get(triggerIDHeader)
		parsedID, err := uuid.Parse(wireID)
		if err != nil {
			t.Fatalf("%s = %q, want UUID: %v", triggerIDHeader, wireID, err)
		}
		if parsedID.Version() != 7 {
			t.Errorf("%s = %q, want UUIDv7", triggerIDHeader, wireID)
		}
		if wireID != returnedID {
			t.Errorf("wire trigger id %q != returned id %q", wireID, returnedID)
		}
		wantMessageID := "github-watcher/issue-812@" + wireID
		if got := msg.Header.Get(messageIDHeader); got != wantMessageID {
			t.Errorf("%s = %q, want %q", messageIDHeader, got, wantMessageID)
		}
		ids = append(ids, wireID)
	}
	if ids[0] == ids[1] {
		t.Fatalf("two publishes reused trigger id %q", ids[0])
	}

	consumer, err := stream.CreateConsumer(ctx, jetstream.ConsumerConfig{
		Name: "trace-redelivery", FilterSubject: subject,
		AckPolicy: jetstream.AckExplicitPolicy, MaxAckPending: 1,
	})
	if err != nil {
		t.Fatal(err)
	}
	first := fetchOne(t, consumer)
	firstID := first.Headers().Get(triggerIDHeader)
	if err := first.Nak(); err != nil {
		t.Fatal(err)
	}
	redelivered := fetchOne(t, consumer)
	if got := redelivered.Headers().Get(triggerIDHeader); got != firstID {
		t.Errorf("redelivery trigger id = %q, first delivery = %q", got, firstID)
	}
	if err := redelivered.Ack(); err != nil {
		t.Fatal(err)
	}
}

func fetchOne(t *testing.T, consumer jetstream.Consumer) jetstream.Msg {
	t.Helper()
	batch, err := consumer.Fetch(1, jetstream.FetchMaxWait(5*time.Second))
	if err != nil {
		t.Fatal(err)
	}
	for msg := range batch.Messages() {
		return msg
	}
	if err := batch.Error(); err != nil {
		t.Fatal(err)
	}
	t.Fatal("consumer returned no message")
	return nil
}

func TestSuccessfulTriggerLogCarriesPublishedID(t *testing.T) {
	rec := &recorder{}
	logs := &syncBuffer{}
	w := &Watcher{
		Source:    &fakeSource{rec: rec, issues: []Issue{{Number: 812, Labels: []string{"ready"}}}},
		Publisher: &fakePublisher{rec: rec},
		Config:    testConfig(),
		Log:       slog.New(slog.NewTextHandler(logs, nil)),
	}
	if err := w.pollOnce(context.Background()); err != nil {
		t.Fatal(err)
	}
	got := logs.String()
	if !strings.Contains(got, "triggered agent for issue") || !strings.Contains(got, "trigger_id=test-trigger-id") {
		t.Fatalf("success log does not carry publisher's trigger id: %s", got)
	}
}
