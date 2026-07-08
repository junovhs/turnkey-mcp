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

**Ishoo runs on this crate.** The extraction is complete for the server + sidecar layers: Ishoo's MCP runtime (`src/mcp/`) is now a thin product composition over `turnkey-mcp`, and Ishoo's full pre-extraction MCP behavior suite (126 tests: handshake, dispatch ordering, owner recovery, fail-closed writes) passes unchanged against it. The repo keeps the originally copied Ishoo source under `origin/ishoo/` so the extraction stays auditable.

Hard-won behaviors carried over from production incidents, at parity with current Ishoo:

- bounded owner lifetime: a detached resident owner idle-reaps instead of lingering forever holding the installed binary locked
- graceful upgrade handoff: a rebuilt/reinstalled app retires the stale-build owner via its build fingerprint (app version + binary signature)
- post-reinstall executable resolution: a `current_exe()` that reads `… (deleted)` after an in-place reinstall still resolves to the replacement binary
- zombie-aware liveness on Unix: an exited-but-unreaped owner reads as dead, so recovery re-elects instead of wedging writes forever (and spawned owners are reaped, so the zombie never forms)
- exit-code-aware liveness on Windows: an `OpenProcess` handle to a terminated pid is not proof of life
- fail-closed writes: a live-but-unreachable owner refuses mutations rather than ever spawning a rival writer

## What it gives you

- stdio MCP JSON-RPC server loop
- typed tool registry
- `initialize`, `tools/list`, `tools/call`, `ping`, and notification handling
- optional MCP resources (`resources/list` / `resources/read`) from an app-supplied closed set
- `tools/list` annotations: `readOnlyHint` derived from each tool's read/write classification, per-tool overrides for the rest
- structured MCP responses with text fallback plus `structuredContent`
- read/write dispatch: read tools can run concurrently, mutating tools serialize in arrival order
- panic containment: a panicking handler answers with a JSON-RPC internal error instead of hanging the request or killing the mutation worker
- `before_tool` pre-dispatch hook (per-call readiness/freshness gates that can short-circuit a call)
- in-process stdout capture (`capture::capture_stdout`) so print-first CLI core fns can serve as tool handlers without a rewrite
- plain-text tool results (a `Value::String` result is served as raw text, no `structuredContent`)
- `ServerConfig::serial()` for apps whose handlers must never overlap a response write (the capture case)
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

## Resources, annotations, and pre-dispatch gates

Serve app artifacts as MCP resources (a **closed set** — `resources/read` only
serves a URI the provider enumerated, never an arbitrary path):

```rust
use turnkey_mcp::{ResourceContent, ResourceEntry};

let config = config.resources(|ctx| {
    let map = ctx.workspace_root.join("MAP.md");
    let mut entries = Vec::new();
    if map.is_file() {
        entries.push(ResourceEntry::file(map, "MAP.md", "The rendered map.", "text/markdown"));
    }
    entries
});
```

`tools/list` always advertises annotations: `readOnlyHint` is derived from the
tool's dispatch classification (`ToolSpec::read` → `true`), and
`ToolSpec::with_annotations(json!({ "destructiveHint": true }))` merges
anything the classification cannot know. Note a `ToolSpec::dynamic` tool
advertises `readOnlyHint: false` — a static host hint cannot depend on the
call's arguments — so register a never-mutating tool with `ToolSpec::read`.

Gate every call on app readiness with `before_tool` (e.g. semmap-style
index freshness: refresh before any navigation tool answers, fail the call
loudly when the refresh fails):

```rust
let config = config.before_tool(|ctx, name, _args| {
    if name == "myapp_generate" {
        return Ok(()); // the tool that produces the index is exempt
    }
    refresh_index(&ctx.workspace_root)
        .map_err(|e| turnkey_mcp::ToolError::server(format!("auto-refresh failed: {e}")))
});
```

## Print-first CLIs: stdout capture

If your core fns print their result instead of returning a typed value,
`capture::capture_stdout(cwd, f)` runs `f` with the process-global stdout
redirected into a pipe (unix `dup2`, windows `SetStdHandle`), the working
directory set to `cwd`, a draining thread against pipe-buffer deadlock, and a
panic-safe restore. Calls are serialized by an internal global lock. This is
how a CLI's exact output becomes a tool's text content without rewriting every
command. Caveat: the redirect cannot be observed under `cargo test`'s output
capture — test it by spawning your real binary.

Capture-based handlers need two config choices: return the captured text as
`Value::String` (served as a raw text block, no `structuredContent`), and run
the transport with `ServerConfig::serial()` — the fd redirect is process-global,
so a concurrent response write on the main loop would land inside the capture
pipe (and the capture's text inside your JSON-RPC stream).

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

In the hidden owner process, build the same server **without** `.sidecar(...)`, run your state startup in the `init` hook (it executes after the singleton lock is won and before the socket exists, so a doomed second owner never runs rival startup work), and pass `handle_line` to the owner runtime:

```rust
use turnkey_mcp::{sidecar, McpServer, ServerConfig, SidecarConfig};

let sidecar_config = SidecarConfig::new("todo", ".", ".todo/cache")
    .app_version(env!("CARGO_PKG_VERSION"))
    .owner_args(["--path", ".", "mcp-owner"]);

let owner_server = McpServer::new(
    ServerConfig::new("todo", env!("CARGO_PKG_VERSION"), ".")
        // .tool(...)
);

sidecar::run_owner_server(
    sidecar_config,
    || {
        // App state startup: sync, background workers, open the store…
        Ok(())
    },
    move |line| owner_server.handle_line(line),
)?;
```

That split is the key non-regression target for Ishoo: the stdio process is disposable host glue; the resident owner is the app/store authority.

`SidecarConfig` carries the lifecycle knobs:

- `.app_version(...)` — folded into the owner build fingerprint so an app release retires stale owners
- `.idle_timeout(...)` / `.idle_timeout_env("MYAPP_OWNER_IDLE_TIMEOUT_MS")` — the bounded owner lifetime (default 10 minutes without a client request)
- `.liveness_path(...)` — the owner exits when this path disappears (e.g. your state dir), so an owner for a deleted workspace never lingers
- `.drain(|| …)` — runs before any owner exit (idle-reap or shutdown handoff) so in-flight serialized work finishes first

## Speaking your app's language

Agents should see your app's vocabulary, not this library's. `ServerConfig` exposes the product seams:

```rust
use turnkey_mcp::OwnerProse;

ServerConfig::new("todo", env!("CARGO_PKG_VERSION"), ".")
    // The fail-closed owner-unavailable errors, in your words:
    .owner_prose(OwnerProse {
        code: -32010,
        prefix: "todo service unavailable — write refused; no changes were made.".into(),
        owner_noun: "resident todo owner".into(),
        restart_hint: "Restart Todo (or run `todo mcp-owner`)".into(),
    })
    // Env prefix for {PREFIX}_MCP_SHUTDOWN_DRAIN_MS / {PREFIX}_MCP_PARENT_WATCHDOG_MS:
    .env_prefix("TODO")
    // Annotate only your status tool when a read degrades past an unreachable
    // owner (default: every degraded read is annotated):
    .annotate_degraded_reads(["todo_status"])
    .read_annotation_source("todo_transport")
    // Tolerate a failed owner election when your app has no state yet (the
    // "opened in a brand-new repo" case) instead of exiting:
    .sidecar_optional()
    // Attach durability facts to every successful mutation result:
    .mutation_hook(|_ctx, _tool, value| {
        // e.g. snapshot app state, then fold the outcome into `value`
        Ok(())
    });
```

## Testing your integration

`turnkey_mcp::sidecar::test_support` gives your suite the owner-recovery shapes without child processes: `unreachable_endpoint` (a registration whose socket refuses), `write_endpoint` (persist a registration), and `start_owner_thread` (a scripted in-process owner). The `Dispatch`/`ServerEvent` runtime internals are exposed `#[doc(hidden)]` for apps porting an existing MCP regression suite.

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
