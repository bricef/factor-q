//! The manager's *request* surface: what the host asks a server that is
//! already connected.
//!
//! Its own module because it is a different question from the rest of
//! [`McpClientManager`](super::McpClientManager). Everything there is
//! about a server's *life* — dialling it, giving up on it, dialling it
//! again, tearing it down — and everything here is about one request
//! against a server whose life is somebody else's problem. The split
//! was named as the right one when the module was first broken up
//! (#191) and is paid for here, where the lifecycle grew enough
//! machinery to make the file exceed its cap.
//!
//! These are inherent methods on the manager rather than a separate
//! handle: they need the connection table to resolve a server name, and
//! a handle over that is exactly what
//! [`McpResourceReader`](super::McpResourceReader) already is for the
//! one caller that needs the manager's `&mut` lifecycle out of the way.

use std::collections::BTreeMap;
use std::sync::Arc;

use rmcp::model::{
    CallToolResult, CompletionContext, CompletionInfo, GetPromptRequestParams, JsonObject,
    LoggingLevel, Prompt, ReadResourceRequestParams, ReadResourceResult, Resource,
    ResourceTemplate, SetLevelRequestParams, SubscribeRequestParams,
};
use serde_json::Value;

use super::prompt_convert::prompt_seed_from_rmcp;
use super::{McpClient, McpClientManager, McpError};

impl McpClientManager {
    /// Find the client handle for a running server by name.
    fn client_for(&self, name: &str) -> Result<&Arc<McpClient>, McpError> {
        self.servers
            .iter()
            .find(|server| server.name == name)
            .map(|server| &server.client)
            .ok_or_else(|| McpError::UnknownServer {
                name: name.to_string(),
            })
    }

    /// List all resources a running server exposes (auto-paginated).
    pub async fn list_resources(&self, server: &str) -> Result<Vec<Resource>, McpError> {
        self.client_for(server)?
            .list_all_resources()
            .await
            .map_err(|err| McpError::ResourceOp {
                server: server.to_string(),
                reason: err.to_string(),
            })
    }

    /// Read a single resource from a running server by URI.
    pub async fn read_resource(
        &self,
        server: &str,
        uri: &str,
    ) -> Result<ReadResourceResult, McpError> {
        self.client_for(server)?
            .read_resource(ReadResourceRequestParams::new(uri))
            .await
            .map_err(|err| McpError::ResourceOp {
                server: server.to_string(),
                reason: err.to_string(),
            })
    }

    /// List the resource templates a running server exposes (auto-paginated).
    pub async fn list_resource_templates(
        &self,
        server: &str,
    ) -> Result<Vec<ResourceTemplate>, McpError> {
        self.client_for(server)?
            .list_all_resource_templates()
            .await
            .map_err(|err| McpError::ResourceOp {
                server: server.to_string(),
                reason: err.to_string(),
            })
    }

    /// List the prompts a running server exposes (auto-paginated).
    /// Returns rmcp's discovery type, mirroring [`Self::list_resources`];
    /// the owned, lossless representation is reserved for the fetched
    /// prompt itself (see [`Self::get_prompt`]).
    pub async fn list_prompts(&self, server: &str) -> Result<Vec<Prompt>, McpError> {
        self.client_for(server)?
            .list_all_prompts()
            .await
            .map_err(|err| McpError::PromptOp {
                server: server.to_string(),
                reason: err.to_string(),
            })
    }

    /// Fetch a prompt by name with bound arguments and materialise it
    /// into an owned, reusable [`PromptSeed`](crate::prompt::PromptSeed)
    /// (Step 4's seed value: message sequence + bound args + provenance).
    /// This is the rmcp boundary — the seed itself is provider-neutral.
    pub async fn get_prompt(
        &self,
        server: &str,
        name: &str,
        arguments: BTreeMap<String, String>,
    ) -> Result<crate::prompt::PromptSeed, McpError> {
        let mut params = GetPromptRequestParams::new(name);
        if !arguments.is_empty() {
            let obj: JsonObject = arguments
                .iter()
                .map(|(k, v)| (k.clone(), Value::String(v.clone())))
                .collect();
            params = params.with_arguments(obj);
        }
        let result = self
            .client_for(server)?
            .get_prompt(params)
            .await
            .map_err(|err| McpError::PromptOp {
                server: server.to_string(),
                reason: err.to_string(),
            })?;
        Ok(prompt_seed_from_rmcp(server, name, arguments, result))
    }

    /// Request argument completion for a prompt argument
    /// (`completion/complete`). Per ADR-0017 prompts are
    /// model-controlled, so this is the agent's tool, not a human menu.
    /// `context` carries previously-resolved arguments for
    /// dependent completions (e.g. the everything server's `name`
    /// argument depends on `department`).
    pub async fn complete_prompt(
        &self,
        server: &str,
        prompt: &str,
        argument: &str,
        value: &str,
        context: Option<CompletionContext>,
    ) -> Result<CompletionInfo, McpError> {
        self.client_for(server)?
            .complete_prompt_argument(prompt, argument, value, context)
            .await
            .map_err(|err| McpError::PromptOp {
                server: server.to_string(),
                reason: err.to_string(),
            })
    }

    /// Subscribe to update notifications for a resource on a server.
    /// Updates arrive via [`Self::recv_notification`].
    pub async fn subscribe(&self, server: &str, uri: &str) -> Result<(), McpError> {
        self.client_for(server)?
            .subscribe(SubscribeRequestParams::new(uri))
            .await
            .map(|_| ())
            .map_err(|err| McpError::ResourceOp {
                server: server.to_string(),
                reason: err.to_string(),
            })
    }

    /// Call a tool, racing it against a `cancel` future. If the tool
    /// completes first, return its result as `Some`. If `cancel` fires
    /// first, send `notifications/cancelled` to the server (asking it
    /// to abort) and return `None`, abandoning the in-flight request.
    ///
    /// This is the host's own cancellation — shutdown, budget, a
    /// superseded step. The agent's tool calls take the same path
    /// through [`McpTool`](super::McpTool), which supplies its deadline
    /// as the `cancel` future; both go through the same private `call`
    /// module, so
    /// there is one implementation of what cancelling a call means.
    pub async fn call_tool_cancellable<F>(
        &self,
        server: &str,
        tool_name: &str,
        arguments: JsonObject,
        cancel: F,
    ) -> Result<Option<CallToolResult>, McpError>
    where
        F: std::future::Future<Output = ()>,
    {
        let canonical_prefix = format!("{server}__");
        let remote_tool_name = tool_name
            .strip_prefix(&canonical_prefix)
            .unwrap_or(tool_name);
        super::call::call_tool_cancellable(
            super::call::OutboundCall {
                client: self.client_for(server)?,
                server,
                remote_tool_name,
                tool_name,
                arguments,
                progress: &self.progress,
                // The host's own cancellation is not an agent tool
                // call, so there is no invocation to attribute
                // progress to.
                call: None,
            },
            cancel,
        )
        .await
    }

    /// Set the minimum logging level the server should send
    /// (`logging/setLevel`); only messages at or above `level` reach the
    /// notification sink thereafter. MCP deprecated the request in
    /// SEP-2577 (rmcp 1.8 flags it); allowed until servers drop it.
    #[allow(deprecated)]
    pub async fn set_logging_level(
        &self,
        server: &str,
        level: LoggingLevel,
    ) -> Result<(), McpError> {
        self.client_for(server)?
            .set_level(SetLevelRequestParams::new(level))
            .await
            .map(|_| ())
            .map_err(|err| McpError::LoggingOp {
                server: server.to_string(),
                reason: err.to_string(),
            })
    }
}
