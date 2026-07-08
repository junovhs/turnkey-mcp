# Notes

This package is being driven toward one specific goal: Ishoo should be able to depend on `turnkey-mcp` and remove its local MCP runtime without losing robustness.

The source of truth for robustness is the copied Ishoo code in `origin/ishoo/`.

## Current extraction pass

This pass restores the most important behavior that must not regress:

- `McpServer::run_stdio()` can now attach to a resident owner through `ServerConfig::sidecar(...)`.
- Tool calls are routed through the owner when sidecar config is present.
- Mutations remain FIFO serialized.
- Reads remain concurrent.
- Mutations fail closed when the owner is alive-but-unreachable.
- Dead owners are re-elected and retried once.
- Read calls can degrade to in-process handling and annotate `mcp_owner` in structured content.
- Unix and Windows parent watchdog behavior is copied forward from Ishoo.
- Host adapter TOML parse failures are skipped per file instead of aborting the whole install.
- Host readiness now has Ishoo-style `connectivity`, `primary_action`, and `secondary_action` fields.

## Desired local-agent task

When this repo compiles and tests pass, the next local coding agent should be able to do this in Ishoo:

1. Add `turnkey-mcp` as a dependency.
2. Replace Ishoo's stdio MCP loop with `turnkey_mcp::McpServer`.
3. Replace Ishoo's resident owner transport with `turnkey_mcp::sidecar`.
4. Replace Ishoo's host adapter materialization with `turnkey_mcp::HostInstall`.
5. Keep Ishoo's actual tool handlers, instructions, store sync hooks, status annotations, and product semantics inside Ishoo.
6. Delete the local duplicated MCP runtime only after parity tests pass.

## Non-regression checklist

Status 2026-07-07: **complete** — Ishoo's local MCP runtime (`src/mcp/transport.rs`, the runtime half of `src/mcp/mod.rs`) was removed in Ishoo MCP-62; every item below is covered by Ishoo's unchanged 126-test MCP suite passing against this crate plus this repo's own 22 tests, and a live `ishoo mcp` smoke (initialize/tools list/read/mutation through a real resident owner).

Before removing Ishoo's local MCP code, verify all of these still hold:

- initialize echoes protocol version and returns server instructions
- tools/list reflects the complete Ishoo registry
- tools/call returns text content and structuredContent
- malformed frames return JSON-RPC parse errors
- notifications return no response
- missing/uninitialized store errors do not kill stdio transport
- reads keep moving while a mutation is slow
- dependent mutations execute in arrival order
- stdio process exits when host parent dies on Unix and Windows
- resident owner is singleton by lock
- stale owner is retired after rebuild/reinstall
- dead owner can be re-elected
- live-but-unreachable owner does not spawn a rival writer
- config installers never clobber foreign host config
- repo-level broken config can shadow user/global readiness facts
