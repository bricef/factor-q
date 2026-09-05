//! Unit tests for [`super`]. Extracted from the parent module so the
//! file that ships is the file you read (#390); `super::*` keeps the
//! same access it had inline.

use super::*;

/// The shared-server dedup key must be the transport target. Before
/// this was fixed the key was `(command, args)`, and `command` is `""`
/// for every `url:` server — so all remote servers collided on one
/// bucket and only the first ever started.
#[test]
fn shared_server_key_is_the_transport_target_not_the_name() {
    let remote = |name: &str, url: &str| McpServerConfig {
        name: name.to_string(),
        command: String::new(),
        args: vec![],
        env: vec![],
        url: Some(url.to_string()),
    };
    let stdio = |name: &str, command: &str, args: &[&str]| McpServerConfig {
        name: name.to_string(),
        command: command.to_string(),
        args: args.iter().map(|a| a.to_string()).collect(),
        env: vec![],
        url: None,
    };
    let key = |config: &McpServerConfig| SharedServerKey::from_config(config).expect("keyable");

    // Two distinct endpoints are two servers, even sharing a name.
    assert_ne!(
        key(&remote("docs", "https://a.example/mcp")),
        key(&remote("docs", "https://b.example/mcp")),
    );
    // One endpoint is one server, however the agents name it — that is
    // the sharing the dedup exists to provide.
    assert_eq!(
        key(&remote("docs", "https://a.example/mcp")),
        key(&remote("reference", "https://a.example/mcp")),
    );

    // Same, for stdio: the spawned process is the identity.
    assert_ne!(
        key(&stdio("a", "npx", &["server-a"])),
        key(&stdio("a", "npx", &["server-b"])),
    );
    assert_eq!(
        key(&stdio("a", "npx", &["server-a"])),
        key(&stdio("b", "npx", &["server-a"])),
    );

    // A stdio and a remote server never collide, whatever the strings.
    assert_ne!(
        key(&stdio("x", "https://a.example/mcp", &[])),
        key(&remote("x", "https://a.example/mcp")),
    );

    // `url` wins when both are set, matching `start_inner`'s transport
    // selection — the key can never disagree with what gets started.
    let both = McpServerConfig {
        url: Some("https://a.example/mcp".to_string()),
        ..stdio("x", "npx", &["server-a"])
    };
    assert_eq!(key(&both), key(&remote("x", "https://a.example/mcp")));

    // Declaring neither is unstartable, so it is an error rather than a
    // bucket every such config silently joins.
    let err = SharedServerKey::from_config(&McpServerConfig {
        name: "nothing".to_string(),
        command: String::new(),
        args: vec![],
        env: vec![],
        url: None,
    })
    .expect_err("a config with no transport has no identity");
    assert!(matches!(err, McpError::UndeclaredTransport { .. }), "{err}");
}
