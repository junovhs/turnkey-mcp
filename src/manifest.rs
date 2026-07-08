use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Language-agnostic app description for the future `turnkey-mcp serve --manifest` path.
///
/// The manifest mode is intentionally simple: Rust owns MCP framing and lifecycle;
/// the app-owned handler process owns behavior.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Manifest {
    pub name: String,
    #[serde(default)]
    pub version: Option<String>,
    #[serde(default)]
    pub instructions: Option<String>,
    pub handler: HandlerCommand,
    #[serde(default)]
    pub tools: Vec<ManifestTool>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct HandlerCommand {
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub cwd: Option<String>,
    #[serde(default)]
    pub env: std::collections::BTreeMap<String, String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ManifestTool {
    pub name: String,
    pub description: String,
    #[serde(rename = "inputSchema")]
    pub input_schema: Value,
    #[serde(default)]
    pub mutation: ManifestMutation,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ManifestMutation {
    #[default]
    Never,
    Always,
    Dynamic,
}

/// Wire request sent from the turnkey-mcp sidecar to a language-specific handler.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct HandlerRequest {
    pub tool: String,
    pub arguments: Value,
    pub workspace: String,
}

/// Wire response returned by a language-specific handler.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct HandlerResponse {
    pub ok: bool,
    #[serde(default)]
    pub result: Option<Value>,
    #[serde(default)]
    pub code: Option<i64>,
    #[serde(default)]
    pub message: Option<String>,
}

impl HandlerResponse {
    pub fn success(result: Value) -> Self {
        Self {
            ok: true,
            result: Some(result),
            code: None,
            message: None,
        }
    }

    pub fn error(code: i64, message: impl Into<String>) -> Self {
        Self {
            ok: false,
            result: None,
            code: Some(code),
            message: Some(message.into()),
        }
    }
}
