//! The edge round-trip and its authentication matrix: a live server
//! with a provisioned identity, a typed command registered in one
//! call, and clients across the full spectrum of credentials — the
//! born-authenticated acceptance criteria as tests.

use std::sync::Arc;

use fq_edge::{EdgeClient, EdgeIdentity, EdgeRegistry, InvokeRequest, WireError, bind};
use fq_ops::{Authority, Command, Domain, OpId, Receipt, Stability, Verb};
use serde::{Deserialize, Serialize};
use serde_json::json;

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
struct DropInput {
    invocation_id: String,
}

fn drop_command() -> Command {
    Command::new::<DropInput>(
        Domain::Invocation,
        "drop",
        Authority {
            verb: Verb::Write,
            scope: Domain::Invocation,
        },
        "Exemplar drop.",
        Stability::Experimental,
    )
}

async fn serve() -> (std::net::SocketAddr, EdgeIdentity) {
    let identity = EdgeIdentity::provision().expect("provision identity");
    let mut registry = EdgeRegistry::new();
    registry
        .command::<DropInput, _, _>(drop_command(), |input| async move {
            assert!(!input.invocation_id.is_empty());
            Ok(Receipt {
                atoms: vec![fq_ops::AtomRef {
                    domain: Domain::Event,
                    seq: 42,
                }],
            })
        })
        .expect("register command");
    let (addr, serving) = bind("127.0.0.1:0", &identity, Arc::new(registry))
        .await
        .expect("bind edge");
    tokio::spawn(serving);
    (addr, identity)
}

#[tokio::test]
async fn admin_token_invokes_describe_and_commands() {
    let (addr, identity) = serve().await;
    let token = identity.mint_admin_token().unwrap();
    let client = EdgeClient::connect(&addr.to_string(), identity.fingerprint(), &token)
        .await
        .expect("connect with admin token");

    // List(Operation): the surface describing itself.
    let describe = client
        .rpc
        .invoke(
            tarpc::context::current(),
            InvokeRequest {
                op: OpId::List(Domain::Operation),
                version: 1,
                input: json!({}),
                min_seq: None,
            },
        )
        .await
        .expect("rpc")
        .expect("describe");
    let listed = describe.output.to_string();
    assert!(
        listed.contains("invocation"),
        "describe lists the command: {listed}"
    );

    // The typed command round-trips; the receipt is model-native.
    let receipt = client
        .rpc
        .invoke(
            tarpc::context::current(),
            InvokeRequest {
                op: drop_command().op(),
                version: 1,
                input: json!({"invocation_id": "inv-1"}),
                min_seq: None,
            },
        )
        .await
        .expect("rpc")
        .expect("drop");
    let receipt: Receipt = serde_json::from_value(receipt.output).unwrap();
    assert_eq!(receipt.watermark(Domain::Event), Some(42));
}

#[tokio::test]
async fn read_only_token_is_denied_the_command_but_reads_describe() {
    let (addr, identity) = serve().await;
    // The dashboard case: an offline-attenuable read-everything token.
    let token = identity.mint_token("dashboard", &[("read", "*")]).unwrap();
    let client = EdgeClient::connect(&addr.to_string(), identity.fingerprint(), &token)
        .await
        .expect("connect with read token");

    let describe = client
        .rpc
        .invoke(
            tarpc::context::current(),
            InvokeRequest {
                op: OpId::List(Domain::Operation),
                version: 1,
                input: json!({}),
                min_seq: None,
            },
        )
        .await
        .expect("rpc");
    assert!(describe.is_ok(), "read token reads describe: {describe:?}");

    let denied = client
        .rpc
        .invoke(
            tarpc::context::current(),
            InvokeRequest {
                op: drop_command().op(),
                version: 1,
                input: json!({"invocation_id": "inv-1"}),
                min_seq: None,
            },
        )
        .await
        .expect("rpc");
    assert!(
        matches!(denied, Err(WireError::Denied { .. })),
        "write must be denied to a read-only token: {denied:?}"
    );
}

#[tokio::test]
async fn garbage_and_foreign_tokens_are_rejected_at_the_preamble() {
    let (addr, identity) = serve().await;

    let garbage =
        EdgeClient::connect(&addr.to_string(), identity.fingerprint(), "not-a-token").await;
    assert!(
        matches!(garbage, Err(fq_edge::client::ConnectError::TokenRejected)),
        "garbage token must be rejected: {garbage:?}"
    );

    // A token minted under a DIFFERENT root fails signature check.
    let foreign_identity = EdgeIdentity::provision().unwrap();
    let foreign = foreign_identity.mint_admin_token().unwrap();
    let rejected = EdgeClient::connect(&addr.to_string(), identity.fingerprint(), &foreign).await;
    assert!(
        matches!(rejected, Err(fq_edge::client::ConnectError::TokenRejected)),
        "foreign-root token must be rejected: {rejected:?}"
    );
}

#[tokio::test]
async fn wrong_fingerprint_refuses_the_connection() {
    let (addr, identity) = serve().await;
    let token = identity.mint_admin_token().unwrap();
    let wrong = [0u8; 32];
    let refused = EdgeClient::connect(&addr.to_string(), wrong, &token).await;
    assert!(
        matches!(
            refused,
            Err(fq_edge::client::ConnectError::FingerprintMismatch)
        ),
        "a mismatched pin must refuse before any token is sent: {refused:?}"
    );
}

#[tokio::test]
async fn unregistered_ops_resolve_to_not_registered() {
    let (addr, identity) = serve().await;
    let token = identity.mint_admin_token().unwrap();
    let client = EdgeClient::connect(&addr.to_string(), identity.fingerprint(), &token)
        .await
        .unwrap();
    let missing = client
        .rpc
        .invoke(
            tarpc::context::current(),
            InvokeRequest {
                op: OpId::Get(Domain::Worker),
                version: 1,
                input: json!({}),
                min_seq: None,
            },
        )
        .await
        .expect("rpc");
    assert!(
        matches!(missing, Err(WireError::NotRegistered { .. })),
        "request vocabulary is refusable: {missing:?}"
    );
}
