//! The edge registry: fq-ops declarations bound to handlers, in one
//! typed call each — the registration form the value-type conversion
//! set up. The declaration's generic slot types the handler, so the
//! cross-site type safety traded away in the contract crate returns
//! here, at the only place a handler exists.

use std::collections::HashMap;
use std::future::Future;
use std::sync::Arc;

use fq_ops::{Command, OpId, Receipt, Registry, RegistryError, Report, Synthetic, View};
use futures::future::BoxFuture;
use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::wire::WireError;

type Handler = Arc<
    dyn Fn(serde_json::Value) -> BoxFuture<'static, Result<serde_json::Value, WireError>>
        + Send
        + Sync,
>;

/// The read gate: called by the server before dispatching a Get/List
/// whose request carries `min_seq` — the daemon installs one backed
/// by the projection watermark; without one (the mock, a daemon
/// without a projection) gated reads are refused rather than served
/// stale. `Err(applied)` reports how far the fold actually got.
/// A stream handler: `(filter, from_seq, max_wait_ms)` to one long-poll
/// batch. Bound per stream op by [`EdgeRegistry::atom`].
pub type StreamHandler = Arc<
    dyn Fn(
            serde_json::Value,
            u64,
            u64,
        ) -> BoxFuture<'static, Result<crate::wire::StreamBatch, WireError>>
        + Send
        + Sync,
>;

pub type ReadGate = Arc<dyn Fn(u64) -> BoxFuture<'static, Result<(), u64>> + Send + Sync>;

/// The declarations plus their handlers. `List(Operation)` — the
/// surface describing itself — is served by the edge directly from
/// [`EdgeRegistry::describe_value`], the model's one self-referential
/// op; everything else dispatches through a bound handler.
#[derive(Default)]
pub struct EdgeRegistry {
    registry: Registry,
    handlers: HashMap<String, Handler>,
    stream_handlers: HashMap<String, StreamHandler>,
    read_gate: Option<ReadGate>,
}

impl EdgeRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Install the read gate (the daemon's projection watermark).
    pub fn with_read_gate(mut self, gate: ReadGate) -> Self {
        self.read_gate = Some(gate);
        self
    }

    pub fn read_gate(&self) -> Option<ReadGate> {
        self.read_gate.clone()
    }

    fn bind<I, O, F, Fut>(&mut self, op: &OpId, handler: F)
    where
        I: DeserializeOwned + Send + 'static,
        O: Serialize,
        F: Fn(I) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<O, WireError>> + Send + 'static,
    {
        let name = op.to_string();
        let dispatch_name = name.clone();
        let handler: Handler = Arc::new(move |input: serde_json::Value| {
            let parsed: Result<I, _> = serde_json::from_value(input);
            match parsed {
                Ok(input) => {
                    let fut = handler(input);
                    Box::pin(async move {
                        let output = fut.await?;
                        serde_json::to_value(output).map_err(|e| WireError::Internal {
                            message: format!("serialising output: {e}"),
                        })
                    })
                }
                Err(e) => {
                    let name = dispatch_name.clone();
                    Box::pin(async move {
                        Err(WireError::InvalidInput {
                            op: name,
                            message: e.to_string(),
                        })
                    })
                }
            }
        });
        self.handlers.insert(name, handler);
    }

    /// Register a command with its typed handler — declaration and
    /// binding in one call, typed through the same generic slot the
    /// declaration's constructor used.
    pub fn command<I, F, Fut>(&mut self, decl: Command, handler: F) -> Result<(), RegistryError>
    where
        I: DeserializeOwned + Send + 'static,
        F: Fn(I) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<Receipt, WireError>> + Send + 'static,
    {
        let op = decl.op();
        self.registry.register(decl)?;
        self.bind::<I, Receipt, _, _>(&op, handler);
        Ok(())
    }

    /// Register a report with its typed handler.
    pub fn report<P, O, F, Fut>(&mut self, decl: Report, handler: F) -> Result<(), RegistryError>
    where
        P: DeserializeOwned + Send + 'static,
        O: Serialize,
        F: Fn(P) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<O, WireError>> + Send + 'static,
    {
        let op = decl.op();
        self.registry.register(decl)?;
        self.bind::<P, O, _, _>(&op, handler);
        Ok(())
    }

    /// Register a synthetic resource with its Get handler (a machinery
    /// singleton's Get takes no input).
    pub fn synthetic<O, F, Fut>(&mut self, decl: Synthetic, get: F) -> Result<(), RegistryError>
    where
        O: Serialize,
        F: Fn() -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<O, WireError>> + Send + 'static,
    {
        let op = OpId::Get(decl.domain);
        self.registry.register(decl)?;
        self.bind::<serde_json::Value, O, _, _>(&op, move |_| get());
        Ok(())
    }

    /// Register a view with its Get and List handlers. (Atoms — with
    /// stream handlers — arrive with the Phase-3 Turn exemplar.)
    pub fn view<K, S, X, F, F1, F2, Fut1, Fut2>(
        &mut self,
        decl: View,
        get: F1,
        list: F2,
    ) -> Result<(), RegistryError>
    where
        K: DeserializeOwned + Send + 'static,
        S: Serialize,
        X: Serialize,
        F: DeserializeOwned + Send + 'static,
        F1: Fn(K) -> Fut1 + Send + Sync + 'static,
        Fut1: Future<Output = Result<S, WireError>> + Send + 'static,
        F2: Fn(F) -> Fut2 + Send + Sync + 'static,
        Fut2: Future<Output = Result<Vec<X>, WireError>> + Send + 'static,
    {
        let domain = decl.domain;
        self.registry.register(decl)?;
        self.bind::<K, S, _, _>(&OpId::Get(domain), get);
        // List is typed by the declaration's Filter and Index row —
        // the same generic slot discipline as Get.
        self.bind::<F, Vec<X>, _, _>(&OpId::List(domain), list);
        Ok(())
    }

    /// Register an atom with its Get, List, and Stream handlers —
    /// the full derived surface of the only streamable nature. Get
    /// and List follow the view discipline; Stream binds the
    /// long-poll handler `next_batch` dispatches to.
    ///
    /// `X` is the List row. It is `S` for an atom declared with
    /// [`fq_ops::Atom::new`] — listing facts hands back facts — and
    /// the declared index row for one declared with
    /// `Atom::with_index`. The slot is spelled out for the same reason
    /// [`EdgeRegistry::view`] spells it out: the declaration and the
    /// handler must agree on the shape List answers with, and this is
    /// the only place a handler exists to check it against.
    #[allow(clippy::type_complexity)]
    pub fn atom<K, S, X, F, F1, F2, F3, Fut1, Fut2, Fut3>(
        &mut self,
        decl: fq_ops::Atom,
        get: F1,
        list: F2,
        stream: F3,
    ) -> Result<(), RegistryError>
    where
        K: DeserializeOwned + Send + 'static,
        S: Serialize,
        X: Serialize,
        F: DeserializeOwned + Send + 'static,
        F1: Fn(K) -> Fut1 + Send + Sync + 'static,
        Fut1: Future<Output = Result<S, WireError>> + Send + 'static,
        F2: Fn(F) -> Fut2 + Send + Sync + 'static,
        Fut2: Future<Output = Result<Vec<X>, WireError>> + Send + 'static,
        F3: Fn(F, u64, u64) -> Fut3 + Send + Sync + 'static,
        Fut3: Future<Output = Result<crate::wire::StreamBatch, WireError>> + Send + 'static,
    {
        let domain = decl.domain;
        self.registry.register(decl)?;
        self.bind::<K, S, _, _>(&OpId::Get(domain), get);
        self.bind::<F, Vec<X>, _, _>(&OpId::List(domain), list);
        let stream = Arc::new(stream);
        let stream_op = OpId::Stream(domain).to_string();
        self.stream_handlers.insert(
            stream_op.clone(),
            Arc::new(move |filter_value, from_seq, max_wait_ms| {
                let stream = stream.clone();
                let op = stream_op.clone();
                Box::pin(async move {
                    let filter: F = serde_json::from_value(filter_value).map_err(|e| {
                        WireError::InvalidInput {
                            op,
                            message: format!("filter: {e}"),
                        }
                    })?;
                    stream(filter, from_seq, max_wait_ms).await
                })
            }),
        );
        Ok(())
    }

    pub fn stream_handler(&self, name: &str) -> Option<StreamHandler> {
        self.stream_handlers.get(name).cloned()
    }

    pub fn registry(&self) -> &Registry {
        &self.registry
    }

    /// The List(Operation) payload: the declarations themselves.
    pub fn describe_value(&self) -> Result<serde_json::Value, WireError> {
        serde_json::to_value(self.registry.describe()).map_err(|e| WireError::Internal {
            message: format!("serialising describe: {e}"),
        })
    }

    pub fn handler(&self, name: &str) -> Option<Handler> {
        self.handlers.get(name).cloned()
    }
}
