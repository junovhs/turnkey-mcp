use turnkey_mcp::{McpServer, ServerConfig, ToolError, ToolSpec};
use serde_json::json;

fn main() {
    let server = McpServer::new(
        ServerConfig::new("todo", env!("CARGO_PKG_VERSION"), ".")
            .instructions("This app is controlled through todo_* MCP tools. Start with todo_status.")
            .tool(ToolSpec::read(
                "todo_status",
                "Return a tiny app status payload.",
                json!({ "type": "object", "properties": {}, "additionalProperties": false }),
                |ctx, _args| {
                    Ok(json!({
                        "app": ctx.app_name.clone(),
                        "workspace": ctx.workspace_root.display().to_string(),
                        "ok": true
                    }))
                },
            ))
            .tool(ToolSpec::write(
                "todo_create",
                "Create a todo item. This example only echoes the item back.",
                json!({
                    "type": "object",
                    "properties": { "title": { "type": "string" } },
                    "required": ["title"],
                    "additionalProperties": false
                }),
                |_ctx, args| {
                    let title = args
                        .get("title")
                        .and_then(|v| v.as_str())
                        .ok_or_else(|| ToolError::invalid_params("todo_create requires title"))?;
                    Ok(json!({ "id": "TODO-1", "title": title }))
                },
            )),
    );

    std::process::exit(server.run_stdio());
}
