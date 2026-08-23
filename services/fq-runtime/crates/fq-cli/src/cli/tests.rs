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
