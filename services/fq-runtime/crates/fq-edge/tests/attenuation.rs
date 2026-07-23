//! Offline attenuation narrows and never widens: a token attenuated
//! to `read:*` still reads the surface but is denied the commands its
//! parent could run; chained attenuations authorise the intersection;
//! and the grant segments are validated before they reach datalog
//! source. Exercised end-to-end against the mock domain service —
//! the daemon only ever sees the attenuated token, never the parent.

use fq_edge::testing::spawn_edge;
use fq_edge::{EdgeClient, InvokeRequest, WireError, attenuate};
use fq_ops::fixtures::invocation_drop;
use fq_ops::{Domain, OpId};
use serde_json::json;

async fn get_inv_1(
    client: &EdgeClient,
) -> Result<Result<serde_json::Value, WireError>, tarpc::client::RpcError> {
    client
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
        .map(|r| r.map(|resp| resp.output))
}

async fn drop_inv_1(
    client: &EdgeClient,
) -> Result<Result<serde_json::Value, WireError>, tarpc::client::RpcError> {
    client
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
        .map(|r| r.map(|resp| resp.output))
}

#[tokio::test]
async fn attenuated_read_token_reads_but_cannot_command() {
    let edge = spawn_edge().await.unwrap();
    let scoped = attenuate(&edge.admin_token, &[("read".to_string(), "*".to_string())]).unwrap();
    let client = EdgeClient::connect(&edge.addr.to_string(), edge.fingerprint, &scoped)
        .await
        .expect("attenuated token connects");

    let get = get_inv_1(&client).await.expect("rpc");
    assert!(get.is_ok(), "read:* attenuation must still read: {get:?}");

    let denied = drop_inv_1(&client).await.expect("rpc");
    assert!(
        matches!(denied, Err(WireError::Denied { .. })),
        "read:* attenuation must be denied the command, got {denied:?}"
    );
    // The domain is untouched — the denial happened before dispatch.
    assert_eq!(edge.domain.invocation("inv-1").unwrap().phase, "running");
}

#[tokio::test]
async fn attenuation_scopes_to_the_named_domain() {
    let edge = spawn_edge().await.unwrap();
    // Write on trigger only: invocation.drop is outside the grant even
    // though the parent token could run it.
    let scoped = attenuate(
        &edge.admin_token,
        &[("write".to_string(), "trigger".to_string())],
    )
    .unwrap();
    let client = EdgeClient::connect(&edge.addr.to_string(), edge.fingerprint, &scoped)
        .await
        .expect("attenuated token connects");

    let denied = drop_inv_1(&client).await.expect("rpc");
    assert!(
        matches!(denied, Err(WireError::Denied { .. })),
        "write:trigger attenuation must not reach invocation.drop, got {denied:?}"
    );
}

#[tokio::test]
async fn chained_attenuation_is_an_intersection() {
    let edge = spawn_edge().await.unwrap();
    // read:* then write:* — nothing satisfies both checks, so the
    // chain authorises nothing. Narrowing can never re-widen.
    let read_only = attenuate(&edge.admin_token, &[("read".to_string(), "*".to_string())]).unwrap();
    let contradiction = attenuate(&read_only, &[("write".to_string(), "*".to_string())]).unwrap();
    let client = EdgeClient::connect(&edge.addr.to_string(), edge.fingerprint, &contradiction)
        .await
        .expect("the token still verifies — it just authorises nothing");

    let get = get_inv_1(&client).await.expect("rpc");
    assert!(
        matches!(get, Err(WireError::Denied { .. })),
        "read is outside the intersection, got {get:?}"
    );
    let cmd = drop_inv_1(&client).await.expect("rpc");
    assert!(
        matches!(cmd, Err(WireError::Denied { .. })),
        "write is outside the intersection, got {cmd:?}"
    );
}

#[test]
fn hostile_grant_segments_are_refused() {
    // Segments are spliced into datalog source; anything but a
    // snake_case word or "*" must be refused outright.
    for hostile in [
        "read\") || true || (\"",
        "read;allow if true",
        "Read",
        "read write",
        "",
    ] {
        let result = attenuate("unused-token", &[(hostile.to_string(), "*".to_string())]);
        assert!(
            result.is_err(),
            "hostile segment {hostile:?} must be refused"
        );
    }
}
