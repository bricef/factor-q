//! Unit tests for [`super`]. Extracted from the parent module so the
//! file that ships is the file you read (#390); `super::*` keeps the
//! same access it had inline.

use super::*;

#[test]
fn server_name_validation_enforces_charset_length_and_reservation() {
    for ok in ["everything", "a", "srv-2", &"x".repeat(48)] {
        assert!(validate_server_name(ok).is_ok(), "'{ok}' should be valid");
    }
    for (bad, why) in [
        ("", "empty"),
        ("Server", "uppercase"),
        ("my_server", "underscore breaks __ splitting"),
        ("srv.1", "dot violates provider tool-name rules"),
        (&"x".repeat(49), "over the 48-char bound"),
        ("builtin", "reserved runtime namespace"),
    ] {
        assert!(validate_server_name(bad).is_err(), "'{bad}' ({why})");
    }
    // The reservation gets its own message so the failure is
    // self-explaining, not a charset complaint.
    let err = validate_server_name("builtin").unwrap_err();
    assert!(format!("{err}").contains("reserved"), "{err}");
}

#[test]
fn namespaced_tool_names_are_bounded_to_provider_limits() {
    assert_eq!(
        namespaced_tool_name("everything", "echo").unwrap(),
        "everything__echo"
    );
    // 48 (max server) + 2 + 14 = 64: exactly at the bound is fine.
    let server = "x".repeat(48);
    assert!(namespaced_tool_name(&server, &"t".repeat(14)).is_ok());
    // One more character crosses the provider bound and must fail
    // loudly at discovery, not at the first LLM call.
    let err = namespaced_tool_name(&server, &"t".repeat(15)).unwrap_err();
    assert!(format!("{err}").contains("64"), "{err}");
    // A remote tool name containing `__` is legal — only the FIRST
    // `__` is the namespace split (server ids cannot contain `_`).
    assert_eq!(
        namespaced_tool_name("srv", "get__thing").unwrap(),
        "srv__get__thing"
    );
}
