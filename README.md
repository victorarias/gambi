# gambi

Local MCP aggregator in Rust: one local MCP endpoint that combines multiple upstream MCP servers, manages auth, and optionally runs multi-step tool workflows through `gambi_execute`.

![gambi admin dashboard](docs/screenshots/admin-dashboard.png)

![gambi admin overrides](docs/screenshots/admin-overrides.png)

## What gambi gives you

- Single MCP connection for coding agents (`stdio` server)
- Aggregation of tools, prompts, and resources from multiple upstream MCP servers
- Namespaced routing (`<server>:<tool>`, `<server>:<prompt>`, reversible namespaced resource URIs)
- Built-in admin web UI for server registration, auth status, logs, and tool-description overrides
- OAuth flow handling (PKCE, state validation, refresh/rotation, degraded-state reporting)
- `gambi_execute` tool (enabled by default) for Python workflows that call namespaced upstream tools

## Security and boundary model

- Admin API/UI is loopback-only (`127.0.0.1` / `::1`)
- MCP endpoint is stdio-only in MVP
- Same-user local trust model: any process under your OS user can access gambi
- Config/token files are owner-only on Unix (`0600` files, `0700` config dir)
- `gambi_execute` uses Monty runtime resource limits; it is not a full OS/container sandbox

## Install

### Option 1: Build from source (recommended during development)

Prerequisites:
- Rust stable toolchain
- Node.js + npm (for Playwright UI tests only)

```bash
git clone git@github.com:victorarias/gambi.git
cd gambi
cargo build --release
```

Binary path:
- `target/release/gambi`

### Option 2: Install directly with Cargo

```bash
cargo install --git https://github.com/victorarias/gambi.git
```

## Quick start

### 1. Start gambi

```bash
gambi serve
```

Defaults:
- Admin UI: `http://127.0.0.1:3333`
- `gambi_execute`: enabled

Useful flags:

```bash
gambi serve --admin-port 3333
gambi serve --no-exec
```

### 2. Configure servers

Use either CLI:

```bash
gambi add github https://example.com/mcp
gambi add local-fixture 'stdio:///usr/local/bin/my-mcp?arg=--stdio'
gambi list
gambi remove github
```

Or the admin UI (`http://127.0.0.1:3333`) for add/remove/auth/override flows.

### 3. Point your coding agent to gambi

Example MCP config entry:

```json
{
  "gambi": {
    "command": "gambi",
    "args": ["serve"]
  }
}
```

### 4. Validate

From a connected MCP client, list tools and verify namespaced upstream tools appear (for example `fixture:fixture_echo`).

## Admin UI and API

Admin UI endpoint:
- `GET /`

Core API endpoints:
- `GET /health`
- `GET /status`
- `GET /servers`
- `POST /servers`
- `DELETE /servers/{name}`
- `GET /tools`
- `POST /tool-descriptions`
- `POST /tool-descriptions/remove`
- `GET /logs?limit=200`
- `GET /config/export`
- `POST /config/import`
- `GET /auth/status`
- `POST /auth/start`
- `POST /auth/refresh`
- `GET /auth/callback`

Server lifecycle note:
- Removing a server or importing config prunes orphaned OAuth token/health entries.

## Configuration files

Default config directory:
- `~/.config/gambi/`

Files:
- `config.json`
- `tokens.json` (file-backed profiles)

Token storage profile behavior:
- `GAMBI_PROFILE=local|dev` (default `local`): file-backed `tokens.json`
- `GAMBI_PROFILE=production`: keychain-backed by default (`GAMBI_TOKEN_STORE=keychain`)

## Runtime environment variables

Execution/runtime:
- `GAMBI_EXEC_MAX_WALL_MS`
- `GAMBI_EXEC_MAX_CPU_SECS`
- `GAMBI_EXEC_MAX_MEM_BYTES`
- `GAMBI_EXEC_MAX_ALLOC_BYTES`
- `GAMBI_EXEC_MAX_ALLOCATIONS`
- `GAMBI_EXEC_MAX_STDOUT_BYTES`

Upstream behavior:
- `GAMBI_UPSTREAM_REQUEST_TIMEOUT_MS` (default `3000`)
- `GAMBI_UPSTREAM_DISCOVERY_TIMEOUT_MS` (default `3000`)

Schema compatibility toggle:
- `GAMBI_TOOL_SCHEMA_COMPAT_NORMALIZATION=1` to enable legacy normalization for strict/buggy clients
- By default, upstream schemas are preserved (strict conformance-first)

## `gambi_execute` behavior

- Upstream calls from code are namespaced via dot syntax: `<server>.<tool>(**kwargs)`
- Keyword args only after tool name (positional payload args are rejected)
- Zero-argument calls are supported
- Upstream `is_error=true` results fail execution
- Progress tokens are forwarded through nested tool calls

## Development and test

Fast local checks:

```bash
cargo fmt --all --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test
```

Full CI-equivalent harness:

```bash
./scripts/test-harness.sh ci
```

Other harness profiles:

```bash
./scripts/test-harness.sh smoke
./scripts/test-harness.sh ui
GAMBI_STRESS_LOOPS=5 ./scripts/test-harness.sh full
GAMBI_STRESS_LOOPS=10 ./scripts/test-harness.sh soak
```

UI test prerequisites:
- `npm ci`
- `npm run test:ui:install`

## License

See repository license terms.
