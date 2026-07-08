# Extraction notes

This project is extracted from Ishoo's MCP implementation.

Status 2026-07-07: the server + sidecar extraction is **complete and consumed** — Ishoo depends on this crate and deleted its local MCP runtime (Ishoo issue MCP-62). The sidecar was additionally hardened to Ishoo's current transport (bounded owner lifetime/idle-reap, fingerprint upgrade handoff, post-reinstall exe resolution, zombie-aware liveness + child reaping, Windows exit-code liveness, drain hooks, init hook, test_support). The host-adapters layer (`src/adapters/`) is extracted but not yet consumed by Ishoo — that migration is tracked in Ishoo.

The extraction philosophy is **copy first, delete second**:

1. copy working Ishoo MCP/runtime code
2. preserve failure modes and comments that explain hard-won behavior
3. remove Ishoo-specific product nouns
4. turn product-specific seams into app-provided hooks
5. only then clean up names and public API

## What is copied into this repo

The uploaded Ishoo zip was unpacked and these files were copied into `origin/ishoo/`:

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

Hashes are recorded in `origin/ishoo/SHA256SUMS`.

The public `src/` files are the generalized extraction layer. They should be reviewed against `origin/ishoo/` before the first public release.

## Source map

### `src/server.rs` from `origin/ishoo/src/mcp/mod.rs`

Retained concepts:

- stdio JSON-RPC loop
- newline framing
- stdin reader thread
- parent watchdog
- shutdown drain
- `initialize`
- `tools/list`
- `tools/call`
- `ping`
- notification no-response handling
- JSON-RPC error frames
- structured tool success payload
- read/write dispatch split
- serial mutation worker
- mutation hook semantics

Removed/replaced:

- Ishoo-specific `SERVER_INSTRUCTIONS`
- `.ishoo` storage warnings
- `refs/ishoo/store` sync/publish result attachment
- Ishoo-specific resident-owner attachment path
- Ishoo-specific recovery prose

Generic seams:

- `ServerConfig::instructions`
- `ServerConfig::mutation_hook`
- `ToolSpec` and `ToolContext`

### `src/sidecar.rs` from `origin/ishoo/src/mcp/transport.rs`

Retained concepts:

- resident owner endpoint
- token-authenticated owner requests
- loopback TCP owner channel
- owner fingerprint
- stale owner retirement
- executable resolution after rebuild/reinstall
- owner election
- owner recovery
- singleton lock file
- owner watchdog
- endpoint reassertion

Removed/replaced:

- `crate::model::git_remote::canonical_workspace_root`
- `crate::model::publisher::spawn`
- `crate::model::reconciler::spawn`
- `crate::model::store_owner::start`
- hard-coded `.ishoo/cache` paths
- Ishoo-specific owner command shape

Generic seams:

- `SidecarConfig`
- `run_owner_server(config, handler)`
- app-provided owner subcommand / state startup

### `src/registry.rs` and `src/types.rs` from `origin/ishoo/src/mcp/registry.rs`

Retained concepts:

- `ToolSpec`
- `ToolError`
- handler type
- read/write/dynamic mutation classification
- op-dispatched tools
- `op_is_read`
- registry-rendered `tools/list`

Removed/replaced:

- actual `ishoo_*` tool list
- capability inventory names
- CLI parity inventory details
- issue/plan/decision handlers

Generic seams:

- `ToolSpec::read`
- `ToolSpec::write`
- `ToolSpec::dynamic`
- app-provided handlers

### `src/adapters/mod.rs` from `origin/ishoo/src/model/adapters.rs`

Retained concepts:

- explicit install only
- repo-scope `.mcp.json`
- repo-scope `.codex/config.toml`
- repo-scope `.claude/settings.local.json`
- repo-scope `.claude/.gitignore`
- managed `CLAUDE.md` / `AGENTS.md` block
- user-scope `~/.codex/config.toml`
- user-scope `~/.claude.json`
- idempotent byte-diff action reporting
- skip instead of clobber when parse fails
- owned-entry detection before overwrite/remove
- host readiness facts

Removed/replaced:

- `ishoo` / `semmap` companion server defaults
- `ishoo_candidates` and `semmap_generate` approval defaults
- Ishoo-specific managed markdown text
- Ishoo-specific Claude permissions

Generic seams:

- `HostInstall`
- `HostServer`
- app-provided managed markdown block
- app-provided Claude permissions
- app-provided Codex tool approval overrides

## Public API target

The public crate should make these patterns feel app-native:

```rust
McpServer::new(ServerConfig::new("my-app", version, workspace))
    .tool(ToolSpec::read(...))
    .tool(ToolSpec::write(...))
    .run_stdio();
```

For non-Rust apps, the target is:

```bash
turnkey-mcp serve --manifest ./mcp.manifest.json
turnkey-mcp install --manifest ./mcp.manifest.json --user
turnkey-mcp doctor --manifest ./mcp.manifest.json
```

That runner is not complete yet. The manifest types are included as the next seam.

## Review checklist before first public release

- Run `cargo fmt`.
- Run `cargo test`.
- Compare `src/server.rs` against `origin/ishoo/src/mcp/mod.rs` and pull over any comments that explain important failure modes.
- Compare `src/sidecar.rs` against `origin/ishoo/src/mcp/transport.rs`, especially Windows parent/lock behavior.
- Compare `src/adapters/mod.rs` against `origin/ishoo/src/model/adapters.rs` and make sure no-clobber behavior is preserved.
- Decide whether `origin/ishoo/` should remain in the public repo or be moved to a branch/tag before publishing.
