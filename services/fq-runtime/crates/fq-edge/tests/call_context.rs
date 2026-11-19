//! What a handler is given about its call (`fq_edge::CallContext`): over
//! the real server, the principal of the connection's verified token, each
//! call its own; in-process, no caller; for a token with no principal
//! fact, what the edge names it in every other answer.

use std::collections::BTreeSet;
use std::sync::{Arc, Mutex};

use fq_edge::{CallContext, EdgeClient, EdgeIdentity, EdgeRegistry, InvokeRequest};
use fq_ops::Receipt;
use fq_ops::fixtures::{DropInput, invocation_drop};
use serde_json::json;
use tokio::sync::Barrier;

/// What each call's handler was given: the input's id, and the principal.
type Seen = Arc<Mutex<Vec<(String, Option<String>)>>>;

/// A registry whose one command takes the call's context and records what
/// it was given. With `meet`, the calls wait for each other inside the
/// handler, so they are in flight at once.
fn recording(seen: Seen, meet: Option<Arc<Barrier>>) -> EdgeRegistry {
    let mut registry = EdgeRegistry::new();
    registry
        .command_with_call::<DropInput, _, _>(
            invocation_drop(),
            move |input: DropInput, call: CallContext| {
                let seen = seen.clone();
                let meet = meet.clone();
                async move {
                    if let Some(meet) = &meet {
                        meet.wait().await;
                    }
                    seen.lock()
                        .unwrap()
                        .push((input.invocation_id, call.principal().map(str::to_string)));
                    Ok(Receipt::empty())
                }
            },
        )
        .expect("register the command");
    registry
}

async fn drop_over(client: &EdgeClient, id: &str) {
    client
        .rpc
        .invoke(
            tarpc::context::current(),
            InvokeRequest {
                op: invocation_drop().op(),
                version: 1,
                input: json!({"invocation_id": id, "reason": null}),
                min_seq: None,
            },
        )
        .await
        .expect("rpc")
        .expect("the command answers");
}

async fn served(registry: EdgeRegistry) -> (String, EdgeIdentity) {
    let identity = EdgeIdentity::provision().unwrap();
    let (addr, serving) = fq_edge::bind("127.0.0.1:0", &identity, Arc::new(registry))
        .await
        .unwrap();
    tokio::spawn(serving);
    (addr.to_string(), identity)
}

/// **Each call is given its own caller.** Two connections, with tokens
/// minted for two principals, one of them attenuated (which keeps the
/// minter's principal), call at once; both calls are inside the handler
/// together. Each handler is given the principal of its own connection's
/// token.
#[tokio::test(flavor = "multi_thread")]
async fn each_call_is_given_its_own_callers_principal() {
    let seen = Seen::default();
    let (addr, identity) = served(recording(seen.clone(), Some(Arc::new(Barrier::new(2))))).await;
    let on_call = identity.mint_token("on-call", &[("*", "*")]).unwrap();
    let auditor = fq_edge::attenuate(
        &identity.mint_token("auditor", &[("*", "*")]).unwrap(),
        &[("write".to_string(), "invocation".to_string())],
    )
    .unwrap();
    let first = EdgeClient::connect(&addr, identity.fingerprint(), &on_call)
        .await
        .unwrap();
    let second = EdgeClient::connect(&addr, identity.fingerprint(), &auditor)
        .await
        .unwrap();

    tokio::join!(
        drop_over(&first, "from-on-call"),
        drop_over(&second, "from-auditor")
    );
    let seen: BTreeSet<_> = seen.lock().unwrap().iter().cloned().collect();
    assert_eq!(
        seen,
        BTreeSet::from([
            ("from-auditor".to_string(), Some("auditor".to_string())),
            ("from-on-call".to_string(), Some("on-call".to_string())),
        ])
    );
}

/// **A call made in-process has no caller.** Tests call handlers this
/// way, with [`CallContext::none`].
#[tokio::test]
async fn a_call_made_in_process_has_no_caller() {
    let seen = Seen::default();
    let registry = recording(seen.clone(), None);
    let handler = registry
        .handler(&invocation_drop().op().to_string())
        .expect("the command is registered");
    handler(json!({"invocation_id": "in-process"}), CallContext::none())
        .await
        .expect("the command answers");
    assert_eq!(*seen.lock().unwrap(), [("in-process".to_string(), None)]);
}

/// **A token with no principal fact is `unknown`**, as the edge names it
/// in every other answer, such as a refusal for a missing grant.
#[tokio::test]
async fn a_token_with_no_principal_fact_is_unknown() {
    let seen = Seen::default();
    let (addr, identity) = served(recording(seen.clone(), None)).await;
    let nameless = biscuit_auth::Biscuit::builder()
        .fact(biscuit_auth::builder::fact(
            "grant",
            &[
                biscuit_auth::builder::string("*"),
                biscuit_auth::builder::string("*"),
            ],
        ))
        .unwrap()
        .build(&identity.root)
        .unwrap()
        .to_base64()
        .unwrap();
    let client = EdgeClient::connect(&addr, identity.fingerprint(), &nameless)
        .await
        .expect("a token with no principal still authenticates");
    drop_over(&client, "nameless").await;
    assert_eq!(
        *seen.lock().unwrap(),
        [("nameless".to_string(), Some("unknown".to_string()))]
    );
}
