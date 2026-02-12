# gambi

A local MCP aggregator with code execution. Single Rust binary, strict localhost boundary, strict MCP conformance.

## The Problem

You want one local server that:
- Aggregates multiple MCP servers (Port, Atlassian, GitHub, etc.)
- Handles auth to remote servers itself (OAuth, API keys) via a web UI
- Exposes everything to clients (Claude Code, Codex) over one local config
- Supports a code execution mode where agents chain calls across servers in one shot

Existing solutions are poorly documented, have broken UX, and do not interoperate cleanly with Claude Code SSE/OAuth behavior.

## The Solution

A single Rust binary with two interfaces:

1. MCP endpoint (stdio): clients connect here and get all aggregated MCP capabilities plus `gambi_execute`
2. Admin web UI (`http://127.0.0.1:<port>`): manage servers, auth, and status

`gambi_execute` is enabled by default.

## Security Contract (MVP)

This section is non-optional for MVP.

### Local-only boundary

- Admin API/UI binds only to loopback (`127.0.0.1` and `::1`)
- OAuth callback receiver binds only to loopback
- No remote-access mode in MVP
- MCP client interface is stdio-only in MVP

### Local trust model

- gambi assumes the local OS account boundary is trusted
- Any process running as the same local user can use gambi and act with available credentials
- This is documented clearly in CLI help and README

### Secret storage baseline

- Tokens use profile-aware storage:
  - `local` / `dev`: plaintext JSON with strict file permissions (`0600`)
  - `production`: OS keychain-backed token storage by default
- Config and token files are created with owner-only access
- A startup warning is emitted if permissions are weaker than required

### Execution safety baseline

- `gambi_execute` is on by default
- Hard resource limits are required: wall time, CPU budget, memory cap, allocation cap
- Execution isolation is interpreter-level (Monty resource limiting), not a full OS/container sandbox
- Provide an emergency disable flag (`--no-exec`) for incident response

## Two Modes

Agents get both modes simultaneously.

### Direct Mode

Each upstream tool is exposed individually with namespacing:
- `port:list_entities`
- `atlassian:search_issues`
- `github:create_pr`

### Code Mode

`gambi_execute` accepts Python code for chaining/batching workflows.

```python
entities = port.list_entities(blueprint="service")
results = []
for e in entities:
    ticket = atlassian.search(query=e["name"])
    results.append({"entity": e["name"], "has_ticket": ticket is not None})
return results
```

Powered by Monty (pydantic): a sandboxed Python interpreter in Rust. Script execution pauses on tool calls, gambi performs real MCP calls, then resumes.

## Architecture

### Core Components

1. Server Manager: connects to upstream MCP servers and manages lifecycle/reconnect
2. Auth Manager: handles OAuth flows, token refresh, and token persistence
3. Capability Router: namespaces and routes tools/resources/prompts
4. Code Executor: Monty sandbox that exposes upstream tools as Python functions
5. MCP Interface: exposes aggregated capabilities plus `gambi_execute` via stdio
6. Admin API/UI: web interface for server config, auth, and status
7. Storage Layer: atomic and locked reads/writes for config/tokens/state

### Client Connection

Clients connect via stdio (spawned subprocess). Example:

```json
"gambi": {
  "command": "gambi",
  "args": ["serve"]
}
```

### Upstream Server Connection

gambi connects to upstream servers using the required transport:
- SSE with OAuth (Atlassian, Port)
- Streamable HTTP with API keys
- stdio for local MCP servers

## Strict MCP Conformance

gambi must preserve MCP semantics for tools, resources, and prompts.

### Capability aggregation

- Aggregate all advertised upstream capabilities
- Keep capability metadata intact unless a namespacing rewrite is required
- Fail closed: if a capability cannot be mapped without semantic loss, do not expose it and report why

### Canonical namespacing and reverse mapping

- Tools: `<server_name>:<tool_name>`
- Prompts: `<server_name>:<prompt_name>`
- Resources: rewritten to a reversible gambi URI form, with original upstream URI preserved in metadata for debugging/auditing
- Every routed call must map deterministically back to exactly one upstream server + original identifier

### Protocol behavior

- Preserve upstream error codes/messages where spec-compatible
- Preserve cancellation and progress-token semantics end-to-end
- Preserve pagination/cursor semantics for list operations
- Preserve JSON schema constraints on tool input/output

## OAuth Requirements (MVP)

OAuth support is required for primary target servers and lands before claiming broad remote compatibility.

### Required flow mechanics

- Authorization Code + PKCE (`S256`)
- CSRF protection with per-request `state` validation
- Provider metadata discovery where available, with explicit fallback config when discovery is missing/broken
- Loopback callback listener with bounded lifetime and timeout

### Token lifecycle

- Persist `access_token`, `refresh_token` (if present), expiry, scopes, and token type
- Refresh automatically before expiry with jittered backoff
- Handle refresh-token rotation correctly (replace old token atomically)
- Mark server degraded and surface actionable status when refresh fails

## Persistence and Concurrency

Config path: `~/.config/gambi/`
- `config.json`
- `tokens.json`

### Correctness guarantees

- Atomic writes (temp file + fsync + rename)
- File locking or process locking around all mutations
- Crash-safe reads (reject partial/corrupt files with recovery guidance)
- In-memory cache invalidation on successful writes and reload events

## Process and Lifecycle

`gambi serve` starts both stdio MCP handling and admin web UI in one process.

### Startup behavior

- If admin port is unavailable, fail fast with a clear error and remediation
- Support explicit `--admin-port` override
- Validate config/tokens on startup and report non-fatal issues in status output

### Shutdown behavior

- Graceful shutdown on SIGINT/SIGTERM
- In-flight upstream requests are cancelled or drained with timeout
- Persist any pending token updates before process exit

## Tech Stack

| Component | Tech | Why |
|-----------|------|-----|
| Language | Rust | Single binary, no runtime deps, Monty is native Rust |
| MCP Server | `rmcp` (official Rust SDK) | stdio server for clients |
| MCP Client | `rmcp` + `reqwest`/`eventsource` | Upstream transports and auth-capable HTTP |
| Code Execution | Monty interpreter (`pydantic/monty`) | Rust-native Python runtime with pause/resume tool bridge and resource limiting |
| Admin UI | `axum` + embedded static files (htmx + minimal CSS) | No build step, bundled in binary |
| Auth | OAuth 2.0/2.1 patterns via `reqwest` (+ optional helper crate) | PKCE/state/refresh control |
| Storage | JSON files with atomic write + locking | Simple MVP with correctness guarantees |
| CLI | `clap` | `gambi serve`, `gambi add`, `gambi list`, `gambi remove` |

## MVP Scope

## Current Implementation Snapshot (February 11, 2026)

Implemented now:
- Rust project scaffold with CLI, config storage, admin web server, and stdio MCP server
- Config persistence with atomic writes, lock-based mutation control, and strict Unix permissions (`0600` files, `0700` config dir)
- Profile-aware token persistence:
  - `local`/`dev`: `tokens.json` with atomic writes + lock coordination
  - `production`: OS keychain-backed token entry (with explicit file override)
- Loopback-only admin enforcement and `--no-exec` behavior wiring
- Real MCP stdio serving via `rmcp` with:
  - local tools: `gambi_list_servers`, `gambi_list_upstream_tools`, conditional `gambi_execute`
  - namespaced passthrough routing for upstream stdio tools/prompts/resources
  - aggregated `list_*` pagination using numeric cursors
  - upstream MCP error pass-through when upstream returns MCP protocol errors
  - routed call cancellation wired to MCP request cancellation tokens plus upstream `notifications/cancelled`
  - routed upstream request metadata preserved across the boundary (including caller progress token metadata)
- Reversible resources namespacing with upstream URI metadata preserved (`x-gambi-upstream-uri`)
- Admin API + local dashboard:
  - `/health`, `/status`, `/servers`, `/tools`, `/logs`, `/config/export`, `/config/import`
  - `/auth/status`, `/auth/start`, `/auth/refresh`, `/auth/callback`
  - server registration/removal via admin API + web UI (`POST /servers`, `DELETE /servers/{name}`)
  - per-server upstream tool description overrides via admin API + web UI (`/tool-descriptions`)
  - `/tools` response includes both flat names and `tool_details` for assertable descriptions in UI/integration tests
  - mutation failure messages are surfaced directly in admin UI (error pane) for actionable operator feedback
- OAuth manager with:
  - Authorization Code + PKCE (`S256`) + single-use state validation
  - Optional provider discovery URL with strict fallback to configured endpoints
  - token persistence (`access_token`, `refresh_token`, `expires_at`, `scopes`, `token_type`)
  - refresh-token rotation handling
  - degraded-state persistence and reporting via admin auth status
  - background pre-expiry auto-refresh loop with bounded backoff/jitter
- CLI export/import support:
  - `gambi export [--out <path>]`
  - `gambi import <path>`
- Expanded tests for config safety/concurrency, namespacing, and MCP pagination behavior
- Managed stdio upstream client lifecycle (reused upstream processes instead of per-request spawning)
- Managed streamable HTTP upstream client lifecycle with OAuth bearer-token attachment
- Bounded reconnect retry/backoff policy for upstream streamable HTTP transports
- Bounded-parallel stdio upstream discovery for tools/prompts/resources with app-owned cache invalidation
- Bounded-parallel discovery for stdio + HTTP upstreams with auth-aware cache keys
- Configurable upstream timeout knobs via env (`GAMBI_UPSTREAM_REQUEST_TIMEOUT_MS`, `GAMBI_UPSTREAM_DISCOVERY_TIMEOUT_MS`)
- Strict-by-default tool schema passthrough for upstream MCP conformance, with optional compatibility normalization flag (`GAMBI_TOOL_SCHEMA_COMPAT_NORMALIZATION`)
- Integration test coverage for routed upstream MCP error pass-through
- Integration test coverage for routed upstream progress notification passthrough (tool/prompt/resource)
- Integration test coverage for routed upstream cancellation passthrough (tool/prompt/resource)
- `gambi_execute` real execution engine with:
  - Monty-based Python runtime embedded in-process
  - namespaced upstream tool exposure as Python-callable functions (`<server>.<tool>(**kwargs)`)
  - pause/resume tool-call bridge across the MCP boundary
  - zero-argument upstream tool-call rewriting support
  - explicit keyword-only payload argument contract (clear errors for positional payload args)
  - upstream `is_error=true` tool responses propagated as execution failures
  - nested progress-token forwarding through execution-initiated upstream calls
  - hard limits: wall timeout, CPU, memory cap, allocation cap
  - bounded stdout capture with deterministic conversion between Monty objects and JSON values
- Integration coverage for execution bridge behavior and limits
- Integration coverage for execution bridge regressions (zero-arg calls, positional-arg rejection, upstream `is_error` behavior)
- Integration coverage for `gambi_execute` across multiple upstream servers and transports (stdio + streamable HTTP)
- Integration coverage for nested progress passthrough via `gambi_execute`
- Integration coverage for auth-backed upstream HTTP routing + reconnect across remote restart
- Crash-safety atomic-write simulation test for pre-rename failure behavior
- OAuth refresh/rotation simulation test coverage against local provider fixture
- Comprehensive test harness with:
  - property-based namespacing invariant tests
  - parallel mixed-operation reliability stress tests (execution + routing)
  - parameterized reliability controls via env (`mixed jobs/rounds`, `bounce cycles/workers`, retry/backoff cadence)
  - Playwright browser E2E suite covering admin UI server management + override lifecycle + restart persistence
- Admin dashboard polling tuned to keep fast status/log feedback while throttling expensive tools discovery refresh
- Admin UI option rendering hardened against HTML injection by eliminating unsafe `innerHTML` interpolation
- Server lifecycle cleanup now prunes orphaned OAuth token/health state on remove/import (admin + CLI)
- Remote HTTP auth integration harness startup hardened against free-port TOCTOU race
  - deterministic test startup via runtime-selected admin port (`--admin-port 0`) + port-file handoff
  - repeated harness profiles via `scripts/test-harness.sh` (`smoke`, `ci`, `full`, `soak`, `ui`)
  - CI workflow wiring for automated PR validation and on-demand deep stress runs (including heavy-matrix toggle)

## Next Execution Steps (Post-MVP Hardening, from February 11, 2026)

1. Monty execution contract hardening
   - Expand docs with a precise supported/unsupported language-runtime matrix for `gambi_execute`.
   - Add additional conformance cases for unsupported imports and unsupported OS calls.
   - Exit criteria: users can predict what execution features are/are not available without reading code.

2. Execution observability polish
   - Add per-execution telemetry fields in admin/log outputs (limit hit reason, tool-call count, elapsed time buckets).
   - Add redaction-safe logging for execution failures that avoids leaking script bodies.
   - Exit criteria: operators can diagnose limit failures quickly from logs/status.

3. Performance and scale envelope
   - Benchmark high-cardinality upstream discovery and repeated `gambi_execute` tool-call bridging.
   - Tune cache invalidation and bounded parallelism defaults based on observed bottlenecks.
   - Exit criteria: documented baseline throughput/latency targets with reproducible benchmark commands.

### Phase 1: Foundation + Conformance

- [x] Rust scaffolding with `rmcp` and `axum`
- [x] stdio MCP server that aggregates upstream servers
- [x] Config management (`~/.config/gambi/config.json`)
- [x] Connect to upstream stdio servers (tools/prompts/resources discovery + routing)
- [x] Strict namespacing and routing for tools/resources/prompts
- [x] Protocol-correct mapping for errors/cancellation/progress/pagination
- [x] Storage layer with atomic writes + locking + permission enforcement
- [x] CLI: `gambi serve`, `gambi add <name> <url>`, `gambi list`, `gambi remove <name>`, `gambi export`, `gambi import`

### Phase 2: OAuth-first Remote Connectivity

- [x] OAuth Authorization Code + PKCE + state flow
- [x] Provider discovery + explicit fallback config
- [x] Token persistence, refresh, rotation, and degraded-state reporting
- [x] Admin API auth status/start/refresh/callback flow (loopback-only)
- [x] Connect to upstream SSE/HTTP servers that require auth
- [x] Auto-reconnect with bounded retry and backoff

### Phase 3: Code Execution

- [x] Integrate sandboxed Python execution runtime
- [x] Expose upstream tools as Python functions
- [x] Pause/resume tool calls during script execution
- [x] `gambi_execute` with hard resource limits
- [x] `--no-exec` startup flag

### Phase 4: Polish

- [x] Health checks and status reporting
- [x] Hot-reload on config changes (request-time config loads)
- [x] Log viewer in admin UI
- [x] Export/import config
- [x] Admin error-surface UX + disabled destructive controls when no servers are configured

## Acceptance Criteria and Test Plan

### Conformance tests

- [x] Golden tests for tool/resource/prompt namespacing and reverse mapping
- [x] MCP protocol tests for errors, cancellation, progress, and pagination passthrough
- [x] Schema validation tests for transformed tool definitions
- [x] Admin UI browser tests (Playwright) for server CRUD + tool-description override behavior

### Security and storage tests

- [x] Verify loopback-only binds for admin/callback listeners
- [x] Verify `0600` permission enforcement on secret-bearing files
- [x] Verify atomic-write behavior under forced crash scenarios
- [x] Verify lock behavior under concurrent CLI + UI writes

### OAuth integration tests

- [x] PKCE and state validation tests
- [x] Refresh and rotation tests with simulated provider responses
- [x] Expiry/degraded state baseline tests (status + degraded flag updates)

### Execution safety tests

- [x] Enforce time/memory/allocation limits in `gambi_execute`
- [x] Verify tool-call pause/resume correctness under retries/timeouts
- [x] Verify `--no-exec` removes `gambi_execute` from exposed capabilities

## Decisions Made

- Single process: `gambi serve` starts stdio MCP + admin web UI
- Strict local-only model: no remote admin exposure in MVP
- Aggregate all capabilities from day one: tools, resources, prompts
- Strict MCP conformance is required, not best-effort
- Profile-aware token storage: file for `local`/`dev`, OS keychain default in `production`
- Rust for single binary delivery
- Direct mode + code mode both enabled, with `gambi_execute` on by default

## Future Improvements

- Optional streamable HTTP exposure for gambi itself (separate hardening profile)
- Script library in admin UI for reusable workflows
