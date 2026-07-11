//! mcp-product-infra is a small toolkit for apps that want to expose themselves to
//! agents through MCP.
//!
//! It is intentionally not an agent framework and not a generic MCP manager. It
//! gives app authors the boring pieces that are easy to get wrong: a stdio MCP
//! loop, typed tool registry, structured responses, read/write dispatch,
//! optional sidecar ownership, and no-clobber host config installers.

pub mod adapters;
pub mod agent_guard;
pub mod capture;
pub mod manifest;
pub mod registry;
pub mod resources;
pub mod response;
pub mod server;
pub mod sidecar;
pub mod types;

pub use adapters::{
    AdapterAction, ClaudeHook, HostConfigFact, HostInstall, HostReadinessReport, HostServer,
    InstallReport,
};
pub use manifest::{
    HandlerCommand, HandlerRequest, HandlerResponse, Manifest, ManifestMutation, ManifestTool,
};
pub use registry::ToolRegistry;
pub use resources::{ResourceContent, ResourceEntry, ResourceProvider};
pub use response::{error_frame, result_frame, tool_ok};
pub use server::{BeforeToolHook, McpServer, MutationHook, OwnerProse, ServerConfig};
pub use sidecar::{OwnerEndpoint, OwnerRecovery, OwnerTransportError, SidecarConfig};
pub use types::{MutationKind, ToolContext, ToolError, ToolResult, ToolSpec};
