# Changelog

## 0.3.0 - engineering mined from semmap's MCP

Capabilities semmap's hand-rolled MCP server had that the crate lacked, now
generalized here (semmap provenance: `src/mcp/{mod,capture,resources,registry}.rs`):

- Panic containment: a panicking tool handler answers with a JSON-RPC internal
  error naming the tool and panic message. Previously a read's panic killed its
  dispatch thread before the completion event (the request hung forever and the
  in-flight accounting leaked), and a mutation's panic killed the FIFO worker
  (every later mutation was silently swallowed). A second backstop wraps the
  dispatch threads themselves so a completion is always emitted.
- MCP resources: `ServerConfig::resources(provider)` serves `resources/list` /
  `resources/read` from an app-enumerated closed set (file-backed or inline;
  no arbitrary-path reads) and advertises the `resources` capability. Absent a
  provider both methods stay method-not-found, as before.
- Tool annotations in `tools/list`: `readOnlyHint` derived from `MutationKind`
  (`Never` → true), `ToolSpec::with_annotations` merges overrides
  (`destructiveHint`, `idempotentHint`, `title`, ...) on top.
- `ServerConfig::before_tool` pre-dispatch hook: runs before every known tool's
  handler and can short-circuit the call with a `ToolError` — the general form
  of semmap's per-call index-freshness gate.
- `capture::capture_stdout`: in-process stdout capture (unix + windows) with
  cwd handling, drain thread, and panic-safe fd restore, so print-first CLI
  core fns can serve as tool handlers.
- `types::INTERNAL_ERROR` (-32603) exported alongside the other JSON-RPC codes.
- Plain-text tool results: a handler returning `Value::String` becomes the raw
  text content block with no `structuredContent` (which the spec types as an
  object), so captured CLI output round-trips unquoted. Object results are
  unchanged.
- `ServerConfig::serial()`: strictly serial transport — each request is handled
  to completion (response written) before the next is consumed, no dispatch
  threads. Required for stdout-capture handlers, whose process-global fd
  redirect a concurrent response write would corrupt.


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
