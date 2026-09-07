//! The per-model provider throttle (#278): a pause set by a 429, an
//! AIMD cap on calls in flight, and the permit every call takes before
//! it reaches the provider.
//!
//! One [`ModelThrottle`] lives in the daemon and is shared three ways —
//! by the LLM client stack through [`ThrottledLlmClient`], by the
//! trigger dispatcher (which asks it before starting an invocation) and
//! by the reducer runner (which asks it how long to defer an invocation
//! the retry layer has given up on). The design and its invariants are
//! `docs/design/committed/provider-throttle-and-deferral.md`; the
//! classification of a 429 into [`LlmError::RateLimited`] that this
//! module reacts to is #606's.
//!
//! The key is the model string a request targets. Routing maps a model
//! to exactly one provider, so the key names the `(provider, model)`
//! pair the issue asks for without a second field.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use tokio::sync::Notify;
use tokio::time::Instant;

use super::{ChatRequest, ChatResponse, LlmClient, LlmError};
use crate::health::ThrottledModel;

/// `[worker.throttle]` — the knobs an operator may turn. Tuning
/// parameters are configuration, not code (design principle 8).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct ThrottleConfig {
    /// Off makes the throttle inert: every permit is granted at once, no
    /// pause is ever set, no trigger is held. Deferral of a 429-exhausted
    /// invocation is not part of the throttle and stays on.
    pub enabled: bool,
    /// The pause a 429 sets when the provider sent no `Retry-After`, in
    /// milliseconds. Doubles per consecutive 429 wave, capped at
    /// `[worker.llm_retry] max_retry_after_ms`, and resets on a success.
    pub default_pause_ms: u64,
    /// How many consecutive successes, with no 429 between them, earn
    /// the model one more permit.
    pub success_window: u32,
}

impl Default for ThrottleConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            default_pause_ms: 30_000,
            success_window: 10,
        }
    }
}

/// The two bounds the throttle must agree with, taken from `[worker]`
/// rather than declared twice: the permit ceiling is
/// `max_concurrent_invocations`, and the longest default pause is
/// `llm_retry.max_retry_after_ms`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ThrottleBounds {
    /// The most permits one model can hold — the cap's upper bound.
    pub ceiling: usize,
    /// The longest pause the escalating default reaches. A provider's
    /// own `Retry-After` is not capped by this: its number is honoured.
    pub max_pause: Duration,
}

/// What a call settled its permit with. Only the rate limit moves the
/// cap down and only a success moves it up; every other failure just
/// gives the permit back.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CallVerdict {
    Succeeded,
    RateLimited { retry_after: Option<Duration> },
    Failed,
}

/// One model's state, a value the throttle keeps per key.
#[derive(Debug, Clone)]
struct ModelState {
    /// The pause in force, if any. Only ever extended; ends by expiring.
    paused_until: Option<Instant>,
    /// Consecutive 429 waves without a success — what the default pause
    /// escalates on.
    waves: u32,
    /// Permits this model may hold at once: `1..=ceiling`.
    cap: usize,
    /// Permits held right now. Never above `cap`.
    in_flight: usize,
    /// Consecutive successes toward the next cap increase.
    successes_in_window: u32,
    /// 429s since the success window last completed — what the operator
    /// surface reports.
    rate_limited_in_window: u32,
}

impl ModelState {
    fn fresh(ceiling: usize) -> Self {
        Self {
            paused_until: None,
            waves: 0,
            cap: ceiling.max(1),
            in_flight: 0,
            successes_in_window: 0,
            rate_limited_in_window: 0,
        }
    }

    fn pause_remaining(&self, now: Instant) -> Option<Duration> {
        self.paused_until
            .filter(|until| *until > now)
            .map(|until| until - now)
    }

    /// Whether anything about this model is worth an operator's line.
    fn is_throttled(&self, now: Instant, ceiling: usize) -> bool {
        self.pause_remaining(now).is_some()
            || self.cap < ceiling.max(1)
            || self.rate_limited_in_window > 0
    }
}

/// The throttle: one per daemon, one [`ModelState`] per model.
pub struct ModelThrottle {
    config: ThrottleConfig,
    bounds: ThrottleBounds,
    models: Mutex<HashMap<String, ModelState>>,
    /// Woken on every change that could admit a waiting call — a permit
    /// released, a cap raised, a pause set (so waiters re-arm their
    /// sleep against the new end).
    changed: Notify,
}

impl std::fmt::Debug for ModelThrottle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ModelThrottle")
            .field("config", &self.config)
            .field("bounds", &self.bounds)
            .finish_non_exhaustive()
    }
}

impl ModelThrottle {
    pub fn new(config: ThrottleConfig, bounds: ThrottleBounds) -> Self {
        Self {
            config,
            bounds,
            models: Mutex::new(HashMap::new()),
            changed: Notify::new(),
        }
    }

    /// A throttle that gates nothing — for a runner with no daemon
    /// around it (tests, the sim, `fq trigger`'s direct path). It still
    /// answers [`Self::deferral_delay`] from the default configuration,
    /// so a deferral decided without a daemon waits the default pause.
    pub fn inert() -> Self {
        Self::new(
            ThrottleConfig {
                enabled: false,
                ..ThrottleConfig::default()
            },
            ThrottleBounds {
                ceiling: usize::MAX,
                max_pause: Duration::from_millis(super::RetryConfig::default().max_retry_after_ms),
            },
        )
    }

    pub fn config(&self) -> &ThrottleConfig {
        &self.config
    }

    pub fn bounds(&self) -> ThrottleBounds {
        self.bounds
    }

    /// Take a permit for one call to `model`, waiting while the model is
    /// paused or has its cap's worth of calls in flight. The permit is
    /// given back when settled or dropped.
    pub async fn acquire(self: &Arc<Self>, model: &str) -> Permit {
        if !self.config.enabled {
            return Permit::inert();
        }
        let mut waited = false;
        loop {
            // Register interest before looking, so a change that lands
            // between the look and the wait is not lost.
            let notified = self.changed.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();

            let wait_until = {
                let mut models = self.models.lock().expect("throttle lock poisoned");
                let state = models
                    .entry(model.to_string())
                    .or_insert_with(|| ModelState::fresh(self.bounds.ceiling));
                let now = Instant::now();
                let pause = state.pause_remaining(now);
                if pause.is_none() && state.in_flight < state.cap {
                    state.in_flight += 1;
                    if waited {
                        tracing::debug!(
                            model,
                            in_flight = state.in_flight,
                            cap = state.cap,
                            "throttle permit granted after a wait"
                        );
                    }
                    return Permit::held(Arc::clone(self), model);
                }
                if !waited {
                    tracing::info!(
                        model,
                        paused_for_ms = pause.map(|p| p.as_millis() as u64),
                        in_flight = state.in_flight,
                        cap = state.cap,
                        "waiting for a throttle permit"
                    );
                }
                pause.map(|remaining| now + remaining)
            };
            waited = true;
            match wait_until {
                Some(until) => tokio::select! {
                    _ = &mut notified => {}
                    _ = tokio::time::sleep_until(until) => {}
                },
                None => notified.await,
            }
        }
    }

    /// How much longer `model` is paused, if it is. The dispatcher's
    /// admission question: `Some` means start nothing for this model.
    pub fn pause_remaining(&self, model: &str) -> Option<Duration> {
        if !self.config.enabled {
            return None;
        }
        let models = self.models.lock().expect("throttle lock poisoned");
        models
            .get(model)
            .and_then(|state| state.pause_remaining(Instant::now()))
    }

    /// How long to defer an invocation on `model` that the retry layer
    /// gave up on: the largest of what the provider asked for, the
    /// model's escalating default for its current wave, and whatever
    /// pause is still in force. The floor is the escalation so an
    /// invocation that keeps meeting a short `Retry-After` backs off
    /// anyway rather than returning every two seconds.
    pub fn deferral_delay(&self, model: &str, retry_after: Option<Duration>) -> Duration {
        let models = self.models.lock().expect("throttle lock poisoned");
        let (waves, remaining) = models
            .get(model)
            .map(|state| (state.waves, state.pause_remaining(Instant::now())))
            .unwrap_or((0, None));
        retry_after
            .unwrap_or(Duration::ZERO)
            .max(self.escalated_default(waves.max(1)))
            .max(remaining.unwrap_or(Duration::ZERO))
    }

    /// Every model worth an operator's line, sorted by name. `now_ms` is
    /// the daemon's wall clock, against which a pause's end is reported.
    pub fn snapshot(&self, now_ms: i64) -> Vec<ThrottledModel> {
        if !self.config.enabled {
            return Vec::new();
        }
        let now = Instant::now();
        let models = self.models.lock().expect("throttle lock poisoned");
        let mut listed: Vec<ThrottledModel> = models
            .iter()
            .filter(|(_, state)| state.is_throttled(now, self.bounds.ceiling))
            .map(|(model, state)| ThrottledModel {
                model: model.clone(),
                paused_until_ms: state
                    .pause_remaining(now)
                    .map(|remaining| now_ms.saturating_add(remaining.as_millis() as i64)),
                cap: state.cap.min(u32::MAX as usize) as u32,
                ceiling: self.bounds.ceiling.min(u32::MAX as usize) as u32,
                in_flight: state.in_flight.min(u32::MAX as usize) as u32,
                rate_limited_in_window: state.rate_limited_in_window,
                waves: state.waves,
            })
            .collect();
        listed.sort_by(|a, b| a.model.cmp(&b.model));
        listed
    }

    /// The default pause for the `wave`th consecutive wave:
    /// `default_pause_ms × 2^(wave-1)`, capped at `max_pause`.
    fn escalated_default(&self, wave: u32) -> Duration {
        let doubled = self
            .config
            .default_pause_ms
            .saturating_mul(1u64 << wave.saturating_sub(1).min(20));
        Duration::from_millis(doubled).min(self.bounds.max_pause)
    }

    /// A call is over: give its permit back and let the verdict move the
    /// model's state.
    fn settle(&self, model: &str, verdict: CallVerdict) {
        let mut models = self.models.lock().expect("throttle lock poisoned");
        let Some(state) = models.get_mut(model) else {
            return;
        };
        state.in_flight = state.in_flight.saturating_sub(1);
        let now = Instant::now();
        match verdict {
            CallVerdict::Succeeded => {
                state.waves = 0;
                state.successes_in_window += 1;
                if state.successes_in_window >= self.config.success_window.max(1) {
                    state.successes_in_window = 0;
                    state.rate_limited_in_window = 0;
                    let raised = (state.cap + 1).min(self.bounds.ceiling.max(1));
                    if raised != state.cap {
                        tracing::info!(
                            model,
                            cap = raised,
                            "throttle cap raised after a clean window"
                        );
                    }
                    state.cap = raised;
                }
            }
            CallVerdict::RateLimited { retry_after } => {
                // The 429s that arrive while a pause is in force answer
                // calls that were already out when the first one landed:
                // one wave, one halving.
                let new_wave = state.pause_remaining(now).is_none();
                if new_wave {
                    state.waves += 1;
                    state.cap = (state.cap / 2).max(1);
                }
                let pause = retry_after.unwrap_or_else(|| self.escalated_default(state.waves));
                let until = now + pause;
                state.paused_until = Some(state.paused_until.map_or(until, |u| u.max(until)));
                state.rate_limited_in_window += 1;
                state.successes_in_window = 0;
                if new_wave {
                    tracing::warn!(
                        model,
                        wave = state.waves,
                        cap = state.cap,
                        pause_ms = pause.as_millis() as u64,
                        from_provider = retry_after.is_some(),
                        "model rate-limited; pausing it and halving its in-flight cap"
                    );
                } else {
                    tracing::debug!(model, "another 429 in the same wave");
                }
            }
            CallVerdict::Failed => {}
        }
        self.changed.notify_waiters();
    }
}

/// A call's right to be at the provider, given back when settled or
/// dropped. Dropping without a verdict — a cancelled future — releases
/// the slot and moves nothing else.
pub struct Permit {
    held: Option<(Arc<ModelThrottle>, String)>,
}

impl Permit {
    fn held(throttle: Arc<ModelThrottle>, model: &str) -> Self {
        Self {
            held: Some((throttle, model.to_string())),
        }
    }

    /// The permit a disabled throttle hands out: nothing to give back.
    fn inert() -> Self {
        Self { held: None }
    }

    /// The call is over; say how it went.
    pub fn settle(mut self, verdict: CallVerdict) {
        if let Some((throttle, model)) = self.held.take() {
            throttle.settle(&model, verdict);
        }
    }
}

impl Drop for Permit {
    fn drop(&mut self) {
        if let Some((throttle, model)) = self.held.take() {
            throttle.settle(&model, CallVerdict::Failed);
        }
    }
}

/// An [`LlmClient`] decorator that takes a [`Permit`] for every call and
/// settles it with the provider's answer. Sits *inside*
/// [`RetryingLlmClient`](super::RetryingLlmClient) so it sees every raw
/// outcome, and so the retry layer's per-attempt sleep overlaps the
/// model's pause instead of adding to it.
pub struct ThrottledLlmClient<C> {
    inner: C,
    throttle: Arc<ModelThrottle>,
}

impl<C> ThrottledLlmClient<C> {
    pub fn new(inner: C, throttle: Arc<ModelThrottle>) -> Self {
        Self { inner, throttle }
    }

    pub fn throttle(&self) -> &Arc<ModelThrottle> {
        &self.throttle
    }
}

#[async_trait]
impl<C: LlmClient> LlmClient for ThrottledLlmClient<C> {
    async fn chat(&self, request: ChatRequest) -> Result<ChatResponse, LlmError> {
        let permit = self.throttle.acquire(&request.model).await;
        let outcome = self.inner.chat(request).await;
        let verdict = match &outcome {
            Ok(_) => CallVerdict::Succeeded,
            Err(LlmError::RateLimited { retry_after, .. }) => CallVerdict::RateLimited {
                retry_after: *retry_after,
            },
            Err(_) => CallVerdict::Failed,
        };
        permit.settle(verdict);
        outcome
    }
}

#[cfg(test)]
mod tests;
