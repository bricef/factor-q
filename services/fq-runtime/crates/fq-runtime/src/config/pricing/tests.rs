use super::*;

/// The section an operator never writes: the live table, the 5x bound,
/// a seven-day window.
#[test]
fn an_absent_section_takes_the_live_table_under_the_default_bound() {
    let settings = PricingConfig::default().load_settings().unwrap();
    assert_eq!(settings.source, TableSource::LitellmMain);
    assert_eq!(settings.rules.max_drift_ratio, 5.0);
    assert_eq!(settings.max_age, DEFAULT_MAX_AGE);
    assert_eq!(
        settings,
        LoadSettings::default(),
        "the config default and the load path's default are one behaviour"
    );
}

#[test]
fn each_key_can_be_set_on_its_own() {
    let config: PricingConfig = toml::from_str("max_drift_ratio = 2.0").unwrap();
    let settings = config.load_settings().unwrap();
    assert_eq!(settings.rules.max_drift_ratio, 2.0);
    assert_eq!(settings.source, TableSource::LitellmMain);
    assert_eq!(settings.max_age, DEFAULT_MAX_AGE);

    let config: PricingConfig = toml::from_str(r#"max_age = "36h""#).unwrap();
    let settings = config.load_settings().unwrap();
    assert_eq!(settings.max_age, Duration::from_secs(36 * 3600));
    assert_eq!(settings.rules.max_drift_ratio, 5.0);
}

/// #735 acceptance box 4: `pinned:<sha>` works.
#[test]
fn a_pinned_source_names_its_commit_and_fetches_it() {
    let config: PricingConfig =
        toml::from_str(r#"source = "pinned:0f1e2d3c4b5a69788796a5b4c3d2e1f009876543""#).unwrap();
    let settings = config.load_settings().unwrap();
    let TableSource::Pinned(sha) = &settings.source else {
        panic!("expected a pin, got {:?}", settings.source);
    };
    assert_eq!(sha, "0f1e2d3c4b5a69788796a5b4c3d2e1f009876543");
    assert_eq!(
        settings.source.url(),
        "https://raw.githubusercontent.com/BerriAI/litellm/\
         0f1e2d3c4b5a69788796a5b4c3d2e1f009876543/model_prices_and_context_window.json"
    );
    assert_eq!(settings.source.commit(), Some(sha.as_str()));
    // A pin round-trips through the config spelling.
    assert_eq!(settings.source.to_string(), config.source);
}

/// An operator who asked for a pin and got the live document has the
/// opposite of what they configured, so a source that does not parse is
/// an error rather than a fallback.
#[test]
fn a_source_that_is_not_one_is_refused() {
    for setting in ["main", "pinned:", "pinned:nothex!", "pinned:abc", ""] {
        let config = PricingConfig {
            source: setting.to_string(),
            ..PricingConfig::default()
        };
        assert!(
            config.load_settings().is_err(),
            "`{setting}` should not parse as a source"
        );
    }
}

#[test]
fn a_bound_that_could_not_hold_is_refused() {
    for ratio in [0.0, 1.0, -5.0, f64::NAN, f64::INFINITY] {
        let config = PricingConfig {
            max_drift_ratio: ratio,
            ..PricingConfig::default()
        };
        assert!(
            config.load_settings().is_err(),
            "a bound of {ratio} should be refused"
        );
    }
}

#[test]
fn the_window_reads_days_hours_minutes_and_seconds() {
    for (setting, expected) in [
        ("7d", 7 * 24 * 3600),
        ("36h", 36 * 3600),
        ("90m", 90 * 60),
        ("30s", 30),
    ] {
        let config = PricingConfig {
            max_age: setting.to_string(),
            ..PricingConfig::default()
        };
        assert_eq!(
            config.load_settings().unwrap().max_age,
            Duration::from_secs(expected),
            "{setting}"
        );
    }
}

#[test]
fn a_window_that_is_not_a_duration_is_refused() {
    for setting in ["", "7", "d", "7 days", "-1d", "0d", "1y"] {
        let config = PricingConfig {
            max_age: setting.to_string(),
            ..PricingConfig::default()
        };
        assert!(
            config.load_settings().is_err(),
            "`{setting}` should not parse as a window"
        );
    }
}
