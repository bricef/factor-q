//! The edge round-trip and its authentication matrix, exercised
//! through the shared test support (`fq_edge::testing`): the mock
//! domain service serving the fixture catalogue, and clients across
//! the full spectrum of credentials — the born-authenticated
//! acceptance criteria as tests.

use fq_edge::testing::spawn_edge;
use fq_edge::{EdgeClient, EdgeIdentity, InvokeRequest, WireError};
use fq_ops::fixtures::{InvocationState, invocation_drop};
use fq_ops::{Domain, OpId, Receipt};
use serde_json::json;

#[tokio::test]
async fn admin_token_drives_the_mock_domain_observably() {
    let edge = spawn_edge().await.unwrap();
    let client = EdgeClient::connect(&edge.addr.to_string(), edge.fingerprint, &edge.admin_token)
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
    assert!(describe.output.to_string().contains("invocation"));

    // Get before, drop, Get after — the mutation is observable
    // through the public surface alone.
    let before = client
        .rpc
        .invoke(
            tarpc::context::current(),
            InvokeRequest {
                op: OpId::Get(Domain::Invocation),
                version: 1,
                input: json!({"invocation_id": "inv-1"}),
                min_seq: None,
            },
        )
        .await
        .expect("rpc")
        .expect("get");
    let before: InvocationState = serde_json::from_value(before.output).unwrap();
    assert_eq!(before.phase, "running");

    let receipt = client
        .rpc
        .invoke(
            tarpc::context::current(),
            InvokeRequest {
                op: invocation_drop().op(),
                version: 1,
                input: json!({"invocation_id": "inv-1", "reason": null}),
                min_seq: None,
            },
        )
        .await
        .expect("rpc")
        .expect("drop");
    let receipt: Receipt = serde_json::from_value(receipt.output).unwrap();
    assert!(receipt.watermark(Domain::Event).is_some());

    let after = client
        .rpc
        .invoke(
            tarpc::context::current(),
            InvokeRequest {
                op: OpId::Get(Domain::Invocation),
                version: 1,
                input: json!({"invocation_id": "inv-1"}),
                min_seq: None,
            },
        )
        .await
        .expect("rpc")
        .expect("get after drop");
    let after: InvocationState = serde_json::from_value(after.output).unwrap();
    assert_eq!(after.phase, "failed", "the drop is observable via Get");

    // And directly via the mock handle, for consumers that assert on
    // state rather than surface.
    assert_eq!(edge.domain.invocation("inv-1").unwrap().phase, "failed");
}

#[tokio::test]
async fn read_only_token_is_denied_the_command_but_reads_the_surface() {
    let edge = spawn_edge().await.unwrap();
    let token = edge
        .identity
        .mint_token("dashboard", &[("read", "*")])
        .unwrap();
    let client = EdgeClient::connect(&edge.addr.to_string(), edge.fingerprint, &token)
        .await
        .expect("connect with read token");

    let get = client
        .rpc
        .invoke(
            tarpc::context::current(),
            InvokeRequest {
                op: OpId::Get(Domain::Invocation),
                version: 1,
                input: json!({"invocation_id": "inv-2"}),
                min_seq: None,
            },
        )
        .await
        .expect("rpc");
    assert!(get.is_ok(), "read token reads the surface: {get:?}");

    let denied = client
        .rpc
        .invoke(
            tarpc::context::current(),
            InvokeRequest {
                op: invocation_drop().op(),
                version: 1,
                input: json!({"invocation_id": "inv-2", "reason": null}),
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
    let edge = spawn_edge().await.unwrap();

    let garbage =
        EdgeClient::connect(&edge.addr.to_string(), edge.fingerprint, "not-a-token").await;
    assert!(
        matches!(garbage, Err(fq_edge::client::ConnectError::TokenRejected)),
        "garbage token must be rejected: {garbage:?}"
    );

    let foreign_identity = EdgeIdentity::provision().unwrap();
    let foreign = foreign_identity.mint_admin_token().unwrap();
    let rejected = EdgeClient::connect(&edge.addr.to_string(), edge.fingerprint, &foreign).await;
    assert!(
        matches!(rejected, Err(fq_edge::client::ConnectError::TokenRejected)),
        "foreign-root token must be rejected: {rejected:?}"
    );
}

#[tokio::test]
async fn wrong_fingerprint_refuses_the_connection() {
    let edge = spawn_edge().await.unwrap();
    let refused = EdgeClient::connect(&edge.addr.to_string(), [0u8; 32], &edge.admin_token).await;
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
    let edge = spawn_edge().await.unwrap();
    let client = EdgeClient::connect(&edge.addr.to_string(), edge.fingerprint, &edge.admin_token)
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
