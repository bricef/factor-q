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

/// The declarations plus their handlers. `List(Operation)` — the
/// surface describing itself — is served by the edge directly from
/// [`EdgeRegistry::describe_value`], the model's one self-referential
/// op; everything else dispatches through a bound handler.
#[derive(Default)]
pub struct EdgeRegistry {
    registry: Registry,
    handlers: HashMap<String, Handler>,
}

impl EdgeRegistry {
    pub fn new() -> Self {
        Self::default()
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
    pub fn view<K, S, F1, F2, Fut1, Fut2>(
        &mut self,
        decl: View,
        get: F1,
        list: F2,
    ) -> Result<(), RegistryError>
    where
        K: DeserializeOwned + Send + 'static,
        S: Serialize,
        F1: Fn(K) -> Fut1 + Send + Sync + 'static,
        Fut1: Future<Output = Result<S, WireError>> + Send + 'static,
        F2: Fn(serde_json::Value) -> Fut2 + Send + Sync + 'static,
        Fut2: Future<Output = Result<serde_json::Value, WireError>> + Send + 'static,
    {
        let domain = decl.domain;
        self.registry.register(decl)?;
        self.bind::<K, S, _, _>(&OpId::Get(domain), get);
        self.bind::<serde_json::Value, serde_json::Value, _, _>(&OpId::List(domain), list);
        Ok(())
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
