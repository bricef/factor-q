//! Unit tests for [`super`] — the `[nats]` section's two guarantees
//! (#540): a credential in the URL is refused, and the token is read
//! from the variable `token_env` names.

use super::*;
use crate::config::Config;

/// A credential in `[nats] url` is refused at parse, with a message
/// that names the mechanism to use instead and — the point of the
/// check — never repeats the credential.
#[test]
fn nats_url_with_userinfo_is_rejected_naming_token_env() {
    for url in [
        "nats://s3cr3t-token@127.0.0.1:4222",
        "nats://fq:s3cr3t-pass@localhost:4222",
        "nats://127.0.0.1:4222,nats://s3cr3t-token@127.0.0.1:4223",
        "s3cr3t-token@127.0.0.1:4222",
    ] {
        let toml = format!("[nats]\nurl = \"{url}\"\n");
        let err = Config::from_toml_str(&toml).expect_err(url);
        assert!(
            matches!(err, ConfigError::NatsUrlCarriesCredential),
            "{url}: {err:?}"
        );
        let msg = err.to_string();
        assert!(msg.contains("token_env"), "{url}: {msg}");
        assert!(
            !msg.contains("s3cr3t"),
            "the error echoed the credential: {msg}"
        );
    }
}

#[test]
fn nats_url_without_userinfo_is_accepted_and_token_env_parses() {
    let config = Config::from_toml_str(
        "[nats]\nurl = \"nats://broker.internal:4222\"\ntoken_env = \"FQ_NATS_TOKEN\"\n",
    )
    .unwrap();
    assert_eq!(config.nats.url, "nats://broker.internal:4222");
    assert_eq!(config.nats.token_env.as_deref(), Some("FQ_NATS_TOKEN"));
    // The path-only `/` and a `@` inside the path are not userinfo.
    assert!(!url_has_userinfo("nats://host:4222/some@path"));
    assert!(!url_has_userinfo("nats://host:4222,nats://other:4222"));
    assert!(url_has_userinfo("nats://host:4222,nats://tok@other:4222"));
}

#[test]
fn resolve_nats_token_reads_the_named_variable() {
    let env_var = "FQ_TEST_NATS_TOKEN_RESOLVE";
    // Safety: tests share a process, but this name is unique to this test.
    unsafe { std::env::set_var(env_var, "tok-value") };
    let config = Config::from_toml_str(&format!("[nats]\ntoken_env = \"{env_var}\"\n")).unwrap();
    assert_eq!(
        config.nats.resolve_token().unwrap().as_deref(),
        Some("tok-value")
    );
    unsafe { std::env::remove_var(env_var) };
}

#[test]
fn resolve_nats_token_fails_loudly_when_the_variable_is_unset_or_empty() {
    let env_var = "FQ_TEST_NATS_TOKEN_MISSING";
    let config = Config::from_toml_str(&format!("[nats]\ntoken_env = \"{env_var}\"\n")).unwrap();
    let err = config.nats.resolve_token().unwrap_err();
    assert!(
        matches!(&err, ConfigError::NatsTokenNotSet { env_var: name } if name == env_var),
        "{err:?}"
    );
    let msg = err.to_string();
    assert!(msg.contains(env_var), "{msg}");
    assert!(
        msg.contains("[nats] token_env"),
        "the message must say which setting named the variable: {msg}"
    );

    // Safety: unique name, see above.
    unsafe { std::env::set_var(env_var, "") };
    assert!(matches!(
        config.nats.resolve_token(),
        Err(ConfigError::NatsTokenNotSet { .. })
    ));
    unsafe { std::env::remove_var(env_var) };
}

#[test]
fn resolve_nats_token_is_none_when_no_variable_is_configured() {
    let config = Config::from_toml_str("[nats]\nurl = \"nats://127.0.0.1:4222\"\n").unwrap();
    assert_eq!(config.nats.resolve_token().unwrap(), None);
    assert_eq!(Config::default().nats.resolve_token().unwrap(), None);
}
