//! Tool trait, built-in tool implementations, and runtime sandbox
//! enforcement for factor-q.
//!
//! The sandbox is the security boundary that prevents an agent from
//! touching files or resources outside what its definition grants.
//! Every built-in tool must validate requests against the sandbox
//! before performing any side effect; see [`sandbox`] for the threat
//! model and the enforcement primitives.

pub mod builtin;
pub mod sandbox;
pub mod tool;

pub use sandbox::{SandboxError, ToolSandbox};
pub use tool::{Tool, ToolContext, ToolError, ToolResult};
