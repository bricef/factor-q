//! Unit tests for [`super`]. Extracted from the parent module so the
//! file that ships is the file you read (#390); `super::*` keeps the
//! same access it had inline.

use super::*;

#[test]
fn advertised_capabilities_reflect_the_grant() {
    let all = FactorQClientHandler::advertised_capabilities(AdvertisedCapabilities::all());
    assert!(all.roots.is_some() && all.sampling.is_some() && all.elicitation.is_some());

    let none = FactorQClientHandler::advertised_capabilities(AdvertisedCapabilities::none());
    assert!(
        none.roots.is_none() && none.sampling.is_none() && none.elicitation.is_none(),
        "nothing is advertised without a grant"
    );

    // Partial grant: only the granted capability is advertised.
    let sampling_only = FactorQClientHandler::advertised_capabilities(AdvertisedCapabilities {
        sampling: true,
        ..AdvertisedCapabilities::none()
    });
    assert!(sampling_only.sampling.is_some());
    assert!(sampling_only.roots.is_none() && sampling_only.elicitation.is_none());
}

#[test]
fn get_info_carries_granted_capabilities() {
    // Default handler (tool-only) advertises nothing inbound.
    let tool_only = FactorQClientHandler::default().get_info();
    assert!(tool_only.capabilities.sampling.is_none());
    assert!(tool_only.capabilities.roots.is_none());
    assert!(tool_only.capabilities.elicitation.is_none());

    // A fully-granted handler advertises all three.
    let granted = FactorQClientHandler::default()
        .with_capabilities(AdvertisedCapabilities::all())
        .get_info();
    assert!(granted.capabilities.sampling.is_some());
    assert!(granted.capabilities.roots.is_some());
    assert!(granted.capabilities.elicitation.is_some());
}

#[test]
fn get_info_offers_the_handshake_era() {
    // rmcp's own default is the modern era; factor-q pins the handshake
    // era's newest version (Route A, docs/reviews/2026-11-06-…).
    assert_ne!(
        ProtocolVersion::LATEST,
        ProtocolVersion::LATEST_WITH_INITIALIZE
    );
    let offered = FactorQClientHandler::default().get_info().protocol_version;
    assert_eq!(offered, ProtocolVersion::LATEST_WITH_INITIALIZE);
    assert_eq!(offered.to_string(), "2025-11-25");
}

/// Serve one raw, non-rmcp MCP endpoint over `transport` that answers
/// `initialize` with whatever version the client offered, `2026-07-28`
/// included, then idles. The 2026-11-05 registry census found public
/// servers that do this. An rmcp-built server cannot stand in for one:
/// rmcp's server re-negotiates the handler's answer and never replies to
/// `initialize` with a version that has no handshake.
async fn accept_whatever_is_offered(transport: tokio::io::DuplexStream) {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    let (read, mut write) = tokio::io::split(transport);
    let mut lines = BufReader::new(read).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        let message: serde_json::Value = serde_json::from_str(&line).expect("JSON-RPC line");
        if message["method"] == "initialize" {
            let reply = serde_json::json!({
                "jsonrpc": "2.0",
                "id": message["id"],
                "result": {
                    "protocolVersion": message["params"]["protocolVersion"],
                    "capabilities": {},
                    "serverInfo": { "name": "accepts-whatever", "version": "0" }
                }
            });
            let mut out = serde_json::to_vec(&reply).expect("serialisable");
            out.push(b'\n');
            write
                .write_all(&out)
                .await
                .expect("write the initialize reply");
        }
    }
}

#[tokio::test]
async fn a_server_that_accepts_any_offer_still_lands_in_the_handshake_era() {
    use rmcp::ServiceExt;
    let (server_transport, client_transport) = tokio::io::duplex(4096);
    tokio::spawn(accept_whatever_is_offered(server_transport));
    let client = FactorQClientHandler::default()
        .serve(client_transport)
        .await
        .expect("client serves over the duplex");
    let negotiated = client
        .peer_info()
        .expect("the server answered initialize")
        .protocol_version
        .clone();
    assert_eq!(negotiated, ProtocolVersion::V_2025_11_25);
}
