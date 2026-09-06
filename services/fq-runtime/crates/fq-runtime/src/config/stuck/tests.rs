use crate::config::Config;

/// The formula, spelled out against a config whose four inputs are all
/// distinct primes-ish numbers, so a transposed factor cannot coincide
/// with the right answer.
#[test]
fn the_threshold_is_twice_one_worst_case_step() {
    let config: Config = Config::from_toml_str(
        "[worker]\nllm_timeout_secs = 70\n\n\
         [worker.llm_retry]\ntimeout_max_attempts = 3\n\n\
         [tools]\nmax_timeout_secs = 400\n",
    )
    .expect("parse");
    // 2 × (3 × 70 + 400 + 5) = 2 × 615 = 1230
    assert_eq!(config.stuck_after().as_secs(), 1230);
    assert_eq!(config.stuck_after_ms(), 1_230_000);
}

/// The shipped defaults, and the dogfood host's numbers: 600 s model
/// deadline, 2 timeout attempts, 900 s tool ceiling, 5 s backstop
/// grace. Pinned because it is the number an operator reads out of `fq
/// doctor`, and a silent change to any of the four deadlines moves it.
#[test]
fn the_default_configuration_derives_seventy_minutes() {
    let config = Config::default();
    assert_eq!(config.worker.llm_timeout_secs, 600);
    assert_eq!(config.worker.llm_retry.timeout_max_attempts, 2);
    assert_eq!(config.tools.max_timeout_secs, 900);
    assert_eq!(config.stuck_after().as_secs(), 4_210);
}

/// Retune a deadline and the safety net under it moves. This is the
/// whole reason the threshold is derived rather than declared.
#[test]
fn halving_the_model_deadline_moves_the_threshold() {
    let base = Config::default().stuck_after();
    let faster: Config =
        Config::from_toml_str("[worker]\nllm_timeout_secs = 300\n").expect("parse");
    assert!(
        faster.stuck_after() < base,
        "{:?} should be under {base:?}",
        faster.stuck_after()
    );
    // 2 × (2 × 300 + 900 + 5)
    assert_eq!(faster.stuck_after().as_secs(), 3_010);
}

/// The override replaces the derivation outright — it does not add to
/// it or clamp it — so an operator who sets it gets exactly the number
/// they wrote.
#[test]
fn the_override_replaces_the_derived_value() {
    let config: Config =
        Config::from_toml_str("[worker]\nstuck_threshold_override_secs = 90\n").expect("parse");
    assert_eq!(config.stuck_after().as_secs(), 90);
    assert_eq!(config.stuck_after_ms(), 90_000);
}

/// Absent by default, which is what "emergency escape hatch" has to
/// mean if the derived value is to be the one definition.
#[test]
fn there_is_no_override_by_default() {
    assert_eq!(Config::default().worker.stuck_threshold_override_secs, None);
}
