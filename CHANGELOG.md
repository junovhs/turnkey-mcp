# Changelog

## 0.1.0 - extraction work in progress

- Copy-first extraction package from Ishoo MCP runtime.
- Added `origin/ishoo/` provenance files.
- Added Rust-native stdio MCP server, registry, response helpers, sidecar owner, host adapters, and manifest structs.
- Restored sidecar owner routing in `McpServer::run_stdio()` so the live crate follows Ishoo's `mcp` -> `mcp-owner` pattern instead of handling tool calls only in-process.
- Added fail-closed owner recovery semantics for mutating calls.
- Added read fallback annotation for unreachable owners.
- Restored Unix/Linux parent-death behavior and Windows parent watchdog scaffolding.
- Updated host readiness and adapter skip behavior closer to Ishoo.
