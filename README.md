# turnkey-mcp

Turnkey MCP for apps.

`turnkey-mcp` is a small, boring toolkit for developers who want their app to be operated by agents through MCP without rebuilding setup, lifecycle, config, and recovery glue every time.

It is **Rust-native**, because the first extraction comes from Ishoo's Rust MCP runtime. It is not intended to be **Rust-only**. The target shape is:

```text
Rust apps
  import the crate directly

Non-Rust apps
  run a small turnkey-mcp sidecar or app-owned helper binary
  and connect handlers through a manifest / local process boundary
```

This is not an agent framework, marketplace, generic MCP manager, desktop app, or cloud service. It is the app-owned MCP layer.

## Status

This is an initial extraction package, not a polished release. The repo intentionally includes the copied Ishoo source under `origin/ishoo/` so the extraction is auditable and can continue in the normal developer way: copy working code first, then delete/generalize what is app-specific.

## What it gives you

- stdio MCP JSON-RPC server loop
- typed tool registry
- `initialize`, `tools/list`, `tools/call`, `ping`, and notification handling
- structured MCP responses with text fallback plus `structuredContent`
- read/write dispatch: read tools can run concurrently, mutating tools serialize in arrival order
- optional resident sidecar owner for app state that needs a single writer
- crash-safe owner election through a lock file
- stale owner retirement after rebuilds/reinstalls
- no-clobber Claude Code and Codex config installers
- repo-scope or user-scope host setup
- optional managed blocks for `CLAUDE.md` and `AGENTS.md`
- host readiness facts
- manifest structs for the future language-agnostic sidecar path

## Why this exists

MCP gives hosts and servers a protocol. It does not give app developers the operational layer needed to make an app pleasant for agents to use.

The first tool handler is usually easy. The annoying parts are:

- how does the host discover the server?
- where does config go?
- how do you avoid littering repos by default?
- what happens when the app binary changes?
- what happens when a server process is orphaned?
- how do writes avoid racing each other?
- how does a tool result tell the agent what actually happened?
- how do you repair setup without hand-editing JSON/TOML?

`turnkey-mcp` packages those boring hard parts.

## Provenance

This repo is extracted from the MCP work in **Ishoo**, an app whose agent surface is MCP-first.

The source files copied from Ishoo are in `origin/ishoo/`:

```text
origin/ishoo/src/mcp/mod.rs
origin/ishoo/src/mcp/transport.rs
origin/ishoo/src/mcp/registry.rs
origin/ishoo/src/mcp/tests.rs
origin/ishoo/src/model/adapters.rs
origin/ishoo/src/main_cli.rs
origin/ishoo/src/main_dispatch.rs
origin/ishoo/src/main_dispatch/handlers.rs
origin/ishoo/config/.mcp.json
origin/ishoo/config/codex-config.toml
origin/ishoo/config/claude-settings.local.json
origin/ishoo/config/CLAUDE.md
```

`origin/ishoo/SHA256SUMS` records hashes for the copied files.

See [`EXTRACTION.md`](./EXTRACTION.md) for the source map.

## Install

For local development:

```toml
[dependencies]
turnkey-mcp = { path = "../turnkey-mcp" }
serde_json = "1"
```

When published:

```toml
[dependencies]
turnkey-mcp = "0.1"
serde_json = "1"
```

## Rust-native usage

```rust
use serde_json::json;
use turnkey_mcp::{McpServer, ServerConfig, ToolError, ToolSpec};

fn main() {
    let server = McpServer::new(
        ServerConfig::new("todo", env!("CARGO_PKG_VERSION"), ".")
            .instructions("Use todo_* MCP tools. Start with todo_status.")
            .tool(ToolSpec::read(
                "todo_status",
                "Return app status.",
                json!({ "type": "object", "properties": {}, "additionalProperties": false }),
                |ctx, _args| Ok(json!({ "app": ctx.app_name, "ok": true })),
            ))
            .tool(ToolSpec::write(
                "todo_create",
                "Create a todo item.",
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
```

Run the example:

```bash
cargo run --example minimal
```

A host can launch your app's MCP entrypoint like this:

```json
{
  "mcpServers": {
    "todo": {
      "command": "todo",
      "args": ["mcp"]
    }
  }
}
```

## Non-Rust apps

A Rust crate does not mean only Rust apps can use the idea.

The project should support two integration modes:

### Mode 1: app-owned helper binary

Any app can ship a small MCP helper binary built with `turnkey-mcp`:

```text
Electron / Python / Go / Swift app
        owns app behavior
        invokes or talks to
small Rust MCP helper built with turnkey-mcp
        exposes MCP to
Claude Code / Codex / Cursor / other hosts
```

This is often the cleanest production shape: your app remains written in its own language, while the MCP lifecycle layer is a small native sidecar.

### Mode 2: manifest / process bridge

The intended language-agnostic next step is a manifest mode:

```bash
turnkey-mcp serve --manifest ./mcp.manifest.json
```

Example manifest:

```json
{
  "name": "todo",
  "version": "0.1.0",
  "instructions": "Use todo_* tools to operate this app.",
  "handler": {
    "command": "python",
    "args": ["./todo_handlers.py"]
  },
  "tools": [
    {
      "name": "todo_status",
      "description": "Return app status.",
      "mutation": "never",
      "inputSchema": {
        "type": "object",
        "properties": {},
        "additionalProperties": false
      }
    },
    {
      "name": "todo_create",
      "description": "Create a todo item.",
      "mutation": "always",
      "inputSchema": {
        "type": "object",
        "properties": { "title": { "type": "string" } },
        "required": ["title"],
        "additionalProperties": false
      }
    }
  ]
}
```

The sidecar owns MCP framing, host config, lifecycle, and dispatch. The app-owned handler process owns behavior.

This first package includes manifest types and docs. The production manifest runner is the next implementation step.

## Tool results

Handlers return typed JSON:

```rust
Ok(json!({ "id": "TODO-1", "title": "Ship it" }))
```

The server wraps it as an MCP success payload:

```json
{
  "content": [
    { "type": "text", "text": "{\n  \"id\": \"TODO-1\"\n}" }
  ],
  "structuredContent": {
    "id": "TODO-1",
    "title": "Ship it"
  },
  "isError": false
}
```

That gives every host a text fallback while preserving structured data for hosts that consume it.

## Read/write dispatch

`ToolSpec::read` tools are handled concurrently. `ToolSpec::write` tools go through one FIFO mutation worker.

That gives useful defaults for app-owned state:

- slow writes do not block status/read tools
- dependent writes cannot reorder
- mutating calls have one serialization point

For tools where mutation depends on arguments, use `ToolSpec::dynamic`:

```rust
ToolSpec::dynamic(
    "todo_item",
    "Read or mutate todos depending on op.",
    schema,
    |args| args.get("op").and_then(|v| v.as_str()) != Some("show"),
    handler,
)
```

## Host setup

`HostInstall` provides no-clobber config writers for app-owned MCP servers:

```rust
use turnkey_mcp::{HostInstall, HostServer};

let install = HostInstall::new("todo")
    .server(HostServer::stdio("todo", "/usr/local/bin/todo", ["mcp"]));

install.install_repo(std::path::Path::new("."))?;
install.install_user()?;
```

By default, host config should be changed only by explicit install commands, never by random app startup or tool calls.

## Sidecar owner

The resident owner pattern is for apps with local state that require one writer:

```text
host-spawned MCP stdio process
  attaches to / elects
resident owner process
  owns app state and serialized mutations
```

That pattern came directly from Ishoo's MCP transport. The generic module keeps the endpoint, token, lock, fingerprint, and recovery pieces while leaving app-specific state startup to the app.

In the host-spawned MCP process, configure the server with a sidecar:

```rust
use turnkey_mcp::{McpServer, ServerConfig, SidecarConfig};

let sidecar = SidecarConfig::new("todo", ".", ".todo/cache")
    .owner_args(["--path", ".", "mcp-owner"]);

let server = McpServer::new(
    ServerConfig::new("todo", env!("CARGO_PKG_VERSION"), ".")
        .sidecar(sidecar)
        // .tool(...)
);

std::process::exit(server.run_stdio());
```

In the hidden owner process, build the same server **without** `.sidecar(...)` and pass `handle_line` to the owner runtime:

```rust
use turnkey_mcp::{sidecar, McpServer, ServerConfig, SidecarConfig};

let sidecar_config = SidecarConfig::new("todo", ".", ".todo/cache")
    .owner_args(["--path", ".", "mcp-owner"]);

let owner_server = McpServer::new(
    ServerConfig::new("todo", env!("CARGO_PKG_VERSION"), ".")
        // .tool(...)
);

sidecar::run_owner_server(sidecar_config, move |line| owner_server.handle_line(line))?;
```

That split is the key non-regression target for Ishoo: the stdio process is disposable host glue; the resident owner is the app/store authority.

## Project boundaries

This package should stay small.

It should not become:

- an agent framework
- a general MCP marketplace
- a GUI manager for every MCP server
- a cloud service
- a workflow engine
- an LLM library

It should remain:

```text
The boring parts of app-owned MCP, packaged.
```
