//! Unit tests for [`super`]: the pause, the wave, the AIMD cap, the
//! permits, and the hermetic burst that is #278's acceptance criterion
//! (c). Everything but the burst runs on tokio's paused clock, so a
//! thirty-second pause costs nothing to wait out.

use super::*;
use crate::events::{AssistantPart, RequestParams, StopReason, TokenUsage};
use std::sync::atomic::{AtomicU32, Ordering};

const SECOND: Duration = Duration::from_secs(1);

fn bounds(ceiling: usize) -> ThrottleBounds {
    ThrottleBounds {
        ceiling,
        max_pause: Duration::from_secs(120),
    }
}

fn throttle(ceiling: usize) -> Arc<ModelThrottle> {
    Arc::new(ModelThrottle::new(
        ThrottleConfig::default(),
        bounds(ceiling),
    ))
}

fn state(t: &ModelThrottle, model: &str) -> ModelState {
    t.models
        .lock()
        .unwrap()
        .get(model)
        .cloned()
        .unwrap_or_else(|| ModelState::fresh(t.bounds.ceiling))
}

/// One call that came back 429: take a permit (waiting out any pause,
/// which the paused clock makes free) and settle it rate-limited.
async fn rate_limited(t: &Arc<ModelThrottle>, model: &str, retry_after: Option<Duration>) {
    t.acquire(model)
        .await
        .settle(CallVerdict::RateLimited { retry_after });
}

async fn succeeded(t: &Arc<ModelThrottle>, model: &str) {
    t.acquire(model).await.settle(CallVerdict::Succeeded);
}

#[tokio::test(start_paused = true)]
async fn a_retry_after_sets_the_pause_the_provider_asked_for() {
    let t = throttle(4);
    assert_eq!(
        t.pause_remaining("m"),
        None,
        "nothing is paused to begin with"
    );
    rate_limited(&t, "m", Some(5 * SECOND)).await;
    assert_eq!(t.pause_remaining("m"), Some(5 * SECOND));
    tokio::time::advance(5 * SECOND).await;
    assert_eq!(t.pause_remaining("m"), None, "a pause ends by expiring");
}

#[tokio::test(start_paused = true)]
async fn a_headerless_429_pauses_the_default_and_doubles_per_wave_up_to_the_cap() {
    let t = throttle(4);
    // Each call here waits out the previous pause first (paused clock),
    // so every 429 opens a new wave.
    rate_limited(&t, "m", None).await;
    assert_eq!(t.pause_remaining("m"), Some(30 * SECOND), "wave 1");
    rate_limited(&t, "m", None).await;
    assert_eq!(t.pause_remaining("m"), Some(60 * SECOND), "wave 2 doubles");
    rate_limited(&t, "m", None).await;
    assert_eq!(t.pause_remaining("m"), Some(120 * SECOND), "wave 3 doubles");
    rate_limited(&t, "m", None).await;
    assert_eq!(
        t.pause_remaining("m"),
        Some(120 * SECOND),
        "wave 4 is held at max_retry_after_ms"
    );
    assert_eq!(state(&t, "m").waves, 4);
}

#[tokio::test(start_paused = true)]
async fn a_success_clears_the_escalation() {
    let t = throttle(4);
    rate_limited(&t, "m", None).await;
    rate_limited(&t, "m", None).await;
    assert_eq!(state(&t, "m").waves, 2);
    succeeded(&t, "m").await;
    assert_eq!(state(&t, "m").waves, 0, "a success clears the wave count");
    rate_limited(&t, "m", None).await;
    assert_eq!(
        t.pause_remaining("m"),
        Some(30 * SECOND),
        "the next 429 starts the escalation over"
    );
}

#[tokio::test(start_paused = true)]
async fn a_pause_is_only_ever_extended() {
    let t = throttle(4);
    let first = t.acquire("m").await;
    let second = t.acquire("m").await;
    first.settle(CallVerdict::RateLimited {
        retry_after: Some(10 * SECOND),
    });
    second.settle(CallVerdict::RateLimited {
        retry_after: Some(2 * SECOND),
    });
    assert_eq!(
        t.pause_remaining("m"),
        Some(10 * SECOND),
        "a shorter ask inside a longer pause does not cut it short"
    );
}

/// The 429s that come back for calls already out when the first one
/// landed are one wave: one halving, one escalation step, however many
/// there are.
#[tokio::test(start_paused = true)]
async fn the_429s_inside_a_pause_are_one_wave() {
    let t = throttle(8);
    let permits = [
        t.acquire("m").await,
        t.acquire("m").await,
        t.acquire("m").await,
        t.acquire("m").await,
    ];
    for permit in permits {
        permit.settle(CallVerdict::RateLimited {
            retry_after: Some(2 * SECOND),
        });
    }
    let s = state(&t, "m");
    assert_eq!(s.waves, 1, "four 429s at once are one wave");
    assert_eq!(s.cap, 4, "halved once, not four times");
    assert_eq!(s.rate_limited_in_window, 4, "but every one is counted");
}

#[tokio::test(start_paused = true)]
async fn the_cap_halves_per_wave_floors_at_one_and_climbs_one_per_window() {
    let t = throttle(8);
    for expected in [4, 2, 1, 1] {
        rate_limited(&t, "m", None).await;
        assert_eq!(state(&t, "m").cap, expected);
    }
    // Ten clean calls earn one permit; a 429 in the middle resets the
    // window without moving the cap.
    for _ in 0..9 {
        succeeded(&t, "m").await;
    }
    assert_eq!(state(&t, "m").cap, 1, "nine is not a window");
    rate_limited(&t, "m", None).await;
    for _ in 0..9 {
        succeeded(&t, "m").await;
    }
    assert_eq!(state(&t, "m").cap, 1, "the 429 reset the window");
    succeeded(&t, "m").await;
    assert_eq!(state(&t, "m").cap, 2, "ten in a row: one more permit");
    assert_eq!(
        state(&t, "m").rate_limited_in_window,
        0,
        "a completed window forgets its 429s"
    );
    for _ in 0..(6 * 10) {
        succeeded(&t, "m").await;
    }
    assert_eq!(state(&t, "m").cap, 8, "never above the ceiling");
    for _ in 0..10 {
        succeeded(&t, "m").await;
    }
    assert_eq!(state(&t, "m").cap, 8);
}

#[tokio::test(start_paused = true)]
async fn permits_block_at_the_cap_and_a_settled_or_dropped_permit_admits_the_next() {
    let t = throttle(2);
    let first = t.acquire("m").await;
    let second = t.acquire("m").await;
    assert_eq!(state(&t, "m").in_flight, 2);

    let blocked = tokio::time::timeout(SECOND, t.acquire("m")).await;
    assert!(blocked.is_err(), "a third call must wait at cap 2");

    drop(first);
    let third = tokio::time::timeout(SECOND, t.acquire("m"))
        .await
        .expect("a dropped permit admits the next call");
    assert_eq!(state(&t, "m").in_flight, 2);

    second.settle(CallVerdict::Succeeded);
    let fourth = tokio::time::timeout(SECOND, t.acquire("m"))
        .await
        .expect("a settled permit admits the next call");
    drop((third, fourth));
    assert_eq!(state(&t, "m").in_flight, 0);
}

#[tokio::test(start_paused = true)]
async fn a_paused_model_grants_no_permit_until_the_pause_ends() {
    let t = throttle(4);
    rate_limited(&t, "m", Some(3 * SECOND)).await;
    let started = Instant::now();
    let waiting = tokio::time::timeout(2 * SECOND, t.acquire("m")).await;
    assert!(waiting.is_err(), "still paused two seconds in");
    let permit = tokio::time::timeout(2 * SECOND, t.acquire("m"))
        .await
        .expect("granted once the pause ends");
    assert!(started.elapsed() >= 3 * SECOND, "not a moment early");
    drop(permit);

    // Another model is not paused by this one's 429.
    let other = tokio::time::timeout(Duration::from_millis(10), t.acquire("other")).await;
    assert!(other.is_ok(), "the pause is per model");
}

#[tokio::test(start_paused = true)]
async fn a_disabled_throttle_is_inert() {
    let t = Arc::new(ModelThrottle::new(
        ThrottleConfig {
            enabled: false,
            ..ThrottleConfig::default()
        },
        bounds(1),
    ));
    let a = t.acquire("m").await;
    let b = tokio::time::timeout(Duration::from_millis(10), t.acquire("m"))
        .await
        .expect("no cap when disabled");
    a.settle(CallVerdict::RateLimited {
        retry_after: Some(60 * SECOND),
    });
    assert_eq!(t.pause_remaining("m"), None, "no pause when disabled");
    let c = tokio::time::timeout(Duration::from_millis(10), t.acquire("m"))
        .await
        .expect("no wait after a 429 when disabled");
    drop((b, c));
    assert!(t.snapshot(0).is_empty(), "nothing to report when disabled");
    assert_eq!(
        t.deferral_delay("m", None),
        30 * SECOND,
        "a deferral still waits the default pause"
    );
}

#[tokio::test(start_paused = true)]
async fn the_deferral_delay_is_the_largest_of_ask_escalation_and_pause() {
    let t = throttle(4);
    assert_eq!(
        t.deferral_delay("m", None),
        30 * SECOND,
        "an unknown model defers the default pause"
    );
    assert_eq!(
        t.deferral_delay("m", Some(300 * SECOND)),
        300 * SECOND,
        "a provider asking for more than the cap is honoured in full"
    );
    rate_limited(&t, "m", Some(2 * SECOND)).await;
    rate_limited(&t, "m", Some(2 * SECOND)).await;
    assert_eq!(state(&t, "m").waves, 2);
    assert_eq!(
        t.deferral_delay("m", Some(2 * SECOND)),
        60 * SECOND,
        "a short Retry-After on the second wave defers the escalated default"
    );
    rate_limited(&t, "m", Some(90 * SECOND)).await;
    assert_eq!(
        t.deferral_delay("m", Some(2 * SECOND)),
        120 * SECOND,
        "wave three: the escalation outgrows the pause in force"
    );
    tokio::time::advance(30 * SECOND).await;
    assert_eq!(
        t.deferral_delay("m", None),
        120 * SECOND,
        "the escalation is the floor even as the pause runs down"
    );
}

#[tokio::test(start_paused = true)]
async fn the_snapshot_lists_only_throttled_models() {
    let t = throttle(4);
    succeeded(&t, "clean").await;
    assert!(
        t.snapshot(1_000).is_empty(),
        "a model that only succeeds is not listed"
    );

    rate_limited(&t, "b-paused", Some(5 * SECOND)).await;
    let listed = t.snapshot(1_000);
    assert_eq!(listed.len(), 1);
    let m = &listed[0];
    assert_eq!(m.model, "b-paused");
    assert_eq!(
        m.paused_until_ms,
        Some(6_000),
        "the pause end on the caller's clock"
    );
    assert_eq!((m.cap, m.ceiling, m.in_flight), (2, 4, 0));
    assert_eq!((m.rate_limited_in_window, m.waves), (1, 1));

    // Past the pause and after a success the cap is still under the
    // ceiling, which is reason enough to keep listing it.
    tokio::time::advance(5 * SECOND).await;
    succeeded(&t, "b-paused").await;
    let held = t.acquire("a-under").await;
    let listed = t.snapshot(1_000);
    assert_eq!(
        listed.len(),
        1,
        "an in-flight call on a clean model is not throttling"
    );
    assert_eq!(listed[0].paused_until_ms, None);
    assert_eq!(listed[0].cap, 2);
    drop(held);

    rate_limited(&t, "a-under", None).await;
    let names: Vec<String> = t.snapshot(0).into_iter().map(|m| m.model).collect();
    assert_eq!(names, ["a-under", "b-paused"], "sorted by name");
}

/// Fails with the scripted errors, in order, then succeeds.
struct ScriptedClient {
    errors: Mutex<std::collections::VecDeque<LlmError>>,
    calls: AtomicU32,
}

#[async_trait]
impl LlmClient for ScriptedClient {
    async fn chat(&self, _request: ChatRequest) -> Result<ChatResponse, LlmError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        match self.errors.lock().unwrap().pop_front() {
            Some(err) => Err(err),
            None => Ok(canned()),
        }
    }
}

fn canned() -> ChatResponse {
    ChatResponse {
        parts: vec![AssistantPart::Text {
            text: "done".to_string(),
        }],
        stop_reason: StopReason::EndTurn,
        usage: TokenUsage::default(),
        reported_cost_usd: None,
    }
}

fn request(model: &str) -> ChatRequest {
    ChatRequest {
        model: model.to_string(),
        messages: vec![],
        tools: vec![],
        params: RequestParams {
            effort: None,
            temperature: None,
            max_tokens: None,
        },
    }
}

#[tokio::test(start_paused = true)]
async fn the_client_settles_every_outcome() {
    let t = throttle(4);
    let client = ThrottledLlmClient::new(
        ScriptedClient {
            errors: Mutex::new(
                vec![
                    LlmError::RateLimited {
                        model: "m".to_string(),
                        retry_after: Some(4 * SECOND),
                    },
                    LlmError::RequestFailed("503".to_string()),
                ]
                .into(),
            ),
            calls: AtomicU32::new(0),
        },
        t.clone(),
    );
    let err = client.chat(request("m")).await.expect_err("scripted 429");
    assert!(matches!(err, LlmError::RateLimited { .. }));
    let s = state(&t, "m");
    assert_eq!(
        (s.in_flight, s.cap),
        (0, 2),
        "the permit came back and the cap halved"
    );
    assert_eq!(t.pause_remaining("m"), Some(4 * SECOND));

    // The next call waits out the pause; the 503 is a plain failure.
    let started = Instant::now();
    client.chat(request("m")).await.expect_err("scripted 503");
    assert!(
        started.elapsed() >= 4 * SECOND,
        "the call waited for the pause"
    );
    let s = state(&t, "m");
    assert_eq!(
        (s.in_flight, s.cap, s.waves),
        (0, 2, 1),
        "a 503 moves nothing"
    );

    client.chat(request("m")).await.expect("scripted success");
    let s = state(&t, "m");
    assert_eq!((s.waves, s.successes_in_window), (0, 1));
    assert_eq!(client.throttle().config().success_window, 10);
}

/// #278 acceptance criterion (c), hermetic: eight concurrent calls
/// against a provider that answers its first five with
/// `429 Retry-After: 2` and everything after with 200. Nothing fails,
/// and once the provider has said no the fleet never exceeds the halved
/// cap. Disable the throttle and the five retries land together.
#[tokio::test]
async fn a_burst_against_a_rate_limited_provider_is_throttled_not_failed() {
    use crate::llm::{RetryConfig, RetryingLlmClient};
    use crate::test_support::fault::MockFault;
    use crate::test_support::mock_openai::{MockChoice, MockOpenAiServer};

    let mock = MockOpenAiServer::start().await;
    // Every answer is held long enough that concurrent arrivals overlap
    // — and so that the whole first burst is at the provider before its
    // first 429 goes out.
    mock.hold_each_response(Duration::from_millis(200));
    for _ in 0..5 {
        mock.push_fault(MockFault::status(429).with_retry_after("2"));
    }
    for _ in 0..8 {
        mock.push(MockChoice::text("ok"));
    }

    let ceiling = 8;
    let throttle = Arc::new(ModelThrottle::new(
        ThrottleConfig::default(),
        bounds(ceiling),
    ));
    let llm = Arc::new(RetryingLlmClient::new(
        ThrottledLlmClient::new(mock.client("burst-model"), throttle.clone()),
        // No jitter: the five retries fire together, which is the
        // lockstep the throttle has to absorb.
        RetryConfig {
            base_delay_ms: 0,
            max_delay_ms: 0,
            ..RetryConfig::default()
        },
    ));

    let mut calls = tokio::task::JoinSet::new();
    for _ in 0..ceiling {
        let llm = llm.clone();
        calls.spawn(async move { llm.chat(request("burst-model")).await });
    }
    let mut completed = 0;
    while let Some(joined) = calls.join_next().await {
        joined
            .expect("call task")
            .expect("no call fails under a 429 burst");
        completed += 1;
    }
    assert_eq!(completed, ceiling, "all eight complete");

    let arrivals = mock.arrivals();
    assert_eq!(
        arrivals.len(),
        13,
        "eight first attempts and five retries reached the provider"
    );
    let cap_after_one_wave = ceiling / 2;
    let after_first_429: Vec<usize> = arrivals
        .iter()
        .filter(|a| a.rate_limits_served_before > 0)
        .map(|a| a.in_flight)
        .collect();
    assert_eq!(
        after_first_429.len(),
        5,
        "the five retries came after the first 429"
    );
    let peak = after_first_429.iter().copied().max().unwrap_or(0);
    assert!(
        peak <= cap_after_one_wave,
        "peak concurrency at the provider after the first 429 was {peak}, \
         over the halved cap of {cap_after_one_wave}: {after_first_429:?}"
    );

    let listed = throttle.snapshot(0);
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].cap as usize, cap_after_one_wave);
    assert_eq!(listed[0].rate_limited_in_window, 5);
    assert_eq!(listed[0].in_flight, 0);
    mock.shutdown().await;
}
