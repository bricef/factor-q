use super::*;
use clap::Parser;

/// The default (no flag, no env) is `text`, preserving the
/// existing human-readable output.
#[test]
fn log_format_defaults_to_text() {
    let cli = Cli::parse_from(["fq", "status"]);
    assert_eq!(cli.global.log_format, LogFormat::Text);
}

/// `--log-format json` parses to the JSON renderer.
#[test]
fn log_format_json_flag_parses() {
    let cli = Cli::parse_from(["fq", "--log-format", "json", "status"]);
    assert_eq!(cli.global.log_format, LogFormat::Json);
}

/// `--log-format text` parses to the text renderer.
#[test]
fn log_format_text_flag_parses() {
    let cli = Cli::parse_from(["fq", "--log-format", "text", "status"]);
    assert_eq!(cli.global.log_format, LogFormat::Text);
}

/// The flag is global — it can follow the subcommand too.
#[test]
fn log_format_flag_is_global() {
    let cli = Cli::parse_from(["fq", "status", "--log-format", "json"]);
    assert_eq!(cli.global.log_format, LogFormat::Json);
}

/// An unknown value is rejected rather than silently defaulting.
#[test]
fn log_format_rejects_unknown_value() {
    let result = Cli::try_parse_from(["fq", "--log-format", "yaml", "status"]);
    let err = match result {
        Ok(_) => panic!("unknown log-format value should be rejected"),
        Err(err) => err,
    };
    let msg = err.to_string();
    assert!(
        msg.contains("yaml") || msg.contains("possible values"),
        "got: {msg}"
    );
}

/// The JSON formatter layer builds and renders a structured event
/// as parseable JSON with the fields intact. Uses a
/// `tracing_subscriber::fmt` layer with a captured writer rather
/// than the process-global subscriber (which can only be set once),
/// but exercises the same `.json()` renderer `init_tracing` wires up.
#[test]
fn json_layer_emits_parseable_json_with_fields() {
    use std::sync::{Arc, Mutex};
    use tracing::subscriber;
    use tracing_subscriber::fmt::MakeWriter;

    #[derive(Clone, Default)]
    struct SharedBuf(Arc<Mutex<Vec<u8>>>);
    impl std::io::Write for SharedBuf {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    impl<'a> MakeWriter<'a> for SharedBuf {
        type Writer = SharedBuf;
        fn make_writer(&'a self) -> Self::Writer {
            self.clone()
        }
    }

    let buf = SharedBuf::default();
    let subscriber = tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::new("info"))
        .json()
        .with_writer(buf.clone())
        .finish();

    subscriber::with_default(subscriber, || {
        tracing::warn!(
            invocation_id = "inv-42",
            worker_id = "w-1",
            "structured event"
        );
    });

    let raw = String::from_utf8(buf.0.lock().unwrap().clone()).unwrap();
    let line = raw.lines().next().expect("expected at least one log line");
    let parsed: serde_json::Value =
        serde_json::from_str(line).expect("each log line must be a JSON object");
    assert_eq!(parsed["level"], "WARN");
    assert_eq!(parsed["fields"]["message"], "structured event");
    assert_eq!(parsed["fields"]["invocation_id"], "inv-42");
    assert_eq!(parsed["fields"]["worker_id"], "w-1");
}

// ------------------------------------------------------------------
// The caps the daemon enforces, as this client documents them.
//
// Both listings are capped daemon-side (`dead_letter.list`,
// `event.list`), and neither cap is re-declared here as a clap range:
// a client-side range check would be a second copy of the number in
// the place least able to notice the daemon disagreeing — an older
// `fq` would refuse pages a newer daemon serves, and quote its own
// stale cap doing it. So `--limit` travels as typed and the daemon
// rules on it, and the client's only copy of the number is the help
// text an operator reads. That copy is hand-written, so it is the one
// that can drift; these pin it to the constant both sides share.
//
// The other half of each contract — the cap as the filter schema's
// `maximum` and in its declared description — is asserted daemon-side,
// in `fq-daemon`'s `dead_letter_atom` and `event_atom` tests.
// ------------------------------------------------------------------

/// The help clap would print for one flag of one `fq` subcommand
/// path, e.g. `["dead-letters", "list"]` and `"limit"`.
fn arg_help(path: &[&str], arg: &str) -> String {
    use clap::CommandFactory;

    let mut command = Cli::command();
    for name in path {
        let next = command
            .find_subcommand(name)
            .unwrap_or_else(|| panic!("`fq {}` exists", path.join(" ")))
            .clone();
        command = next;
    }
    command
        .get_arguments()
        .find(|candidate| candidate.get_id() == arg)
        .and_then(|candidate| candidate.get_help().map(ToString::to_string))
        .unwrap_or_else(|| panic!("`fq {} --{arg}` is documented", path.join(" ")))
}

#[test]
fn dead_letter_limit_help_names_the_cap_the_daemon_enforces() {
    let help = arg_help(&["dead-letters", "list"], "limit");
    let cap = fq_ops::surface::DEAD_LETTER_LIST_MAX_LIMIT;
    assert!(
        help.contains(&cap.to_string()),
        "`fq dead-letters list --limit`'s help must name the {cap}-row cap; got {help:?}"
    );
}

#[test]
fn event_query_limit_help_names_the_cap_the_daemon_enforces() {
    let help = arg_help(&["events", "query"], "limit");
    let cap = fq_ops::surface::EVENT_LIST_MAX_LIMIT;
    assert!(
        help.contains(&cap.to_string()),
        "`fq events query --limit`'s help must name the {cap}-row cap; got {help:?}"
    );
}
