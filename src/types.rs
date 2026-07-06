use serde_json::Value;
use std::path::PathBuf;
use std::sync::Arc;

pub const PARSE_ERROR: i64 = -32700;
pub const INVALID_REQUEST: i64 = -32600;
pub const METHOD_NOT_FOUND: i64 = -32601;
pub const INVALID_PARAMS: i64 = -32602;
pub const SERVER_ERROR: i64 = -32000;
/// Generic counterpart to Ishoo's STOR-22 `STORE_SERVICE_UNAVAILABLE` code.
/// Returned when a mutating tool call cannot safely reach the resident owner.
pub const OWNER_SERVICE_UNAVAILABLE: i64 = -32010;

/// Context passed to every tool handler.
#[derive(Clone, Debug)]
pub struct ToolContext {
    pub app_name: String,
    pub workspace_root: PathBuf,
}

impl ToolContext {
    pub fn new(app_name: impl Into<String>, workspace_root: impl Into<PathBuf>) -> Self {
        Self {
            app_name: app_name.into(),
            workspace_root: workspace_root.into(),
        }
    }
}

/// A typed tool failure that becomes a JSON-RPC error.
#[derive(Clone, Debug)]
pub struct ToolError {
    pub code: i64,
    pub message: String,
}

impl ToolError {
    pub fn new(code: i64, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }

    pub fn invalid_params(message: impl Into<String>) -> Self {
        Self::new(INVALID_PARAMS, message)
    }

    pub fn server(message: impl Into<String>) -> Self {
        Self::new(SERVER_ERROR, message)
    }
}

pub type ToolResult = Result<Value, ToolError>;
pub type Handler = Arc<dyn Fn(&ToolContext, &Value) -> ToolResult + Send + Sync + 'static>;
pub type MutationClassifier = Arc<dyn Fn(&Value) -> bool + Send + Sync + 'static>;

/// How the server should classify a tool call for dispatch.
#[derive(Clone)]
pub enum MutationKind {
    Never,
    Always,
    Dynamic(MutationClassifier),
}

impl MutationKind {
    pub fn mutates(&self, args: &Value) -> bool {
        match self {
            MutationKind::Never => false,
            MutationKind::Always => true,
            MutationKind::Dynamic(classifier) => classifier(args),
        }
    }
}

/// A single MCP tool exposed by the app.
#[derive(Clone)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
    pub mutation: MutationKind,
    pub handler: Handler,
}

impl ToolSpec {
    pub fn read(
        name: impl Into<String>,
        description: impl Into<String>,
        input_schema: Value,
        handler: impl Fn(&ToolContext, &Value) -> ToolResult + Send + Sync + 'static,
    ) -> Self {
        Self {
            name: name.into(),
            description: description.into(),
            input_schema,
            mutation: MutationKind::Never,
            handler: Arc::new(handler),
        }
    }

    pub fn write(
        name: impl Into<String>,
        description: impl Into<String>,
        input_schema: Value,
        handler: impl Fn(&ToolContext, &Value) -> ToolResult + Send + Sync + 'static,
    ) -> Self {
        Self {
            name: name.into(),
            description: description.into(),
            input_schema,
            mutation: MutationKind::Always,
            handler: Arc::new(handler),
        }
    }

    pub fn dynamic(
        name: impl Into<String>,
        description: impl Into<String>,
        input_schema: Value,
        mutates: impl Fn(&Value) -> bool + Send + Sync + 'static,
        handler: impl Fn(&ToolContext, &Value) -> ToolResult + Send + Sync + 'static,
    ) -> Self {
        Self {
            name: name.into(),
            description: description.into(),
            input_schema,
            mutation: MutationKind::Dynamic(Arc::new(mutates)),
            handler: Arc::new(handler),
        }
    }
}
