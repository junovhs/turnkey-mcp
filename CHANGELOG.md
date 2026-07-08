# Changelog

## 0.2.0 - parity with current Ishoo; Ishoo consumes the crate

- Ishoo's MCP runtime now depends on this crate (Ishoo MCP-62): its local stdio
  loop and owner transport are deleted, and its full pre-extraction MCP behavior
  suite passes unchanged against this runtime.
- Sidecar: bounded owner lifetime (idle-reap after a configurable/env-overridable
  idle timeout, with a poll cadence clamp), `drain` hook run before any owner
  exit, `init` hook in `run_owner_server` (runs after the singleton lock is won
  and before the socket binds), configurable `liveness_path` for the owner
  watchdog, `app_version` folded into the build fingerprint.
- Liveness hardening: an exited-but-unreaped (zombie) owner reads as dead on
  Linux so recovery re-elects instead of wedging writes forever; spawned owners
  are reaped so the zombie never forms; Windows liveness confirms via
  `GetExitCodeProcess` (a handle to a terminated pid is not proof of life).
- Server: `OwnerProse` (app-vocabulary fail-closed errors incl. the JSON-RPC
  code), `sidecar_optional()` fail-open startup, scoped degraded-read
  annotation (`annotate_degraded_reads` + `read_annotation_source`), per-app
  `env_prefix` for runtime override env vars, `handle_line_with_owner`.
- Test support: `sidecar::test_support` (unreachable endpoint, endpoint writer,
  scripted in-thread owner); `Dispatch`/`ServerEvent`/watchdog internals exposed
  `#[doc(hidden)]` for apps porting existing MCP regression suites; 11 Ishoo
  transport tests ported (22 tests total in this repo).

## 0.1.0 - extraction work in progress

- Copy-first extraction package from Ishoo MCP runtime.
- Added `origin/ishoo/` provenance files.
- Added Rust-native stdio MCP server, registry, response helpers, sidecar owner, host adapters, and manifest structs.
- Restored sidecar owner routing in `McpServer::run_stdio()` so the live crate follows Ishoo's `mcp` -> `mcp-owner` pattern instead of handling tool calls only in-process.
- Added fail-closed owner recovery semantics for mutating calls.
- Added read fallback annotation for unreachable owners.
- Restored Unix/Linux parent-death behavior and Windows parent watchdog scaffolding.
- Updated host readiness and adapter skip behavior closer to Ishoo.
