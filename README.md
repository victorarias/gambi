# gambi

One MCP endpoint. All your servers. Code that calls them.

A local MCP aggregator that gives your coding agent a single stdio connection to every upstream MCP server you use, plus a built-in execution tool that can call any of them from Python scripts — so the agent stops round-tripping one tool at a time.

![gambi admin dashboard](docs/screenshots/admin-dashboard.png)

## Why this exists

I tried to use an existing MCP hub. I couldn't get it to work. The documentation looked extensive but didn't actually help. My coding agents couldn't figure it out either — they just kept telling me to add more dependencies. The usability to get it running was so painful that I thought: *I can't use this. Even if I fix it, I can't use this.*

At the same time, I'd been thinking about a different problem. MCP is great — small scripts, API calls, structured tool access. But the more servers you add to a hub, the more tools your agent sees, and agents get lost in a sea of dozens of tools. Worse, they call them one by one, round-tripping for every little step of a workflow.

So what if we solve both problems at once? A single aggregator that *just works* — one binary, zero config ceremony — **and** an execution tool that lets the agent write a short Python script to orchestrate multiple tool calls in one shot.

That kind of duct-tape-and-ingenuity solution has a name in Brazilian Portuguese: **gambiarra**. Hence, gambi.

## What it does

- **Single MCP connection** — your agent talks to one stdio server and sees all upstream tools, namespaced (`github:search_issues`, `slack:post_message`)
- **Aggregation** — tools, prompts, and resources from every upstream server, with automatic namespacing and routing
- **`gambi_execute` + `gambi_execute_escalated`** — safe-first execution with explicit escalation when workflows need higher-risk tools
- **Admin UI** — web dashboard to add/remove servers, view tools, manage OAuth, override tool descriptions, and tail logs
- **Execution policy controls** — classify each catalog as `heuristic`, `all-safe`, `all-escalated`, or `custom`, with per-tool overrides
- **OAuth handling** — PKCE, token refresh/rotation, degraded-state reporting — all managed for you
- **Tool description overrides** — rewrite what your agent sees for any upstream tool, useful when upstream descriptions are unhelpful

![gambi admin with servers configured](docs/screenshots/admin-overrides.png)

## Install

Build from source:

```bash
git clone git@github.com:victorarias/gambi.git
cd gambi
cargo build --release
# binary: target/release/gambi
```

Or install directly:

```bash
cargo install --git https://github.com/victorarias/gambi.git
```

Prerequisites: Rust stable toolchain.

## Quick start

### 1. Point your coding agent to gambi

Add this to your MCP client config (Claude Code, Cursor, etc.):

```json
{
  "gambi": {
    "command": "gambi",
    "args": ["serve"]
  }
}
```

That's it. gambi starts automatically when your agent connects.

The admin UI will be at `http://127.0.0.1:3333` by default.

### 2. Add servers

Via CLI:

```bash
gambi add github https://github.com/github/github-mcp-server
gambi add my-tool 'stdio:///usr/local/bin/my-mcp?arg=--stdio'
gambi list
gambi remove github
```

Or through the admin UI at `http://127.0.0.1:3333`.

### 3. Use it

Your agent now sees all upstream tools prefixed with their server name. Ask it to call `github:search_issues` or `my-tool:do_something` — gambi routes the call to the right upstream server.

For multi-step workflows, the agent can use `gambi_execute` (safe mode) to write a Python script that calls multiple tools in sequence:

```python
issues = github.search_issues(query="bug label:critical")
for issue in issues:
    slack.post_message(channel="alerts", text=f"Critical: {issue['title']}")
```

One tool call instead of N+1 round-trips.

## Safe/ Escalated Execution Policy (What, Why, How)

**What**

- `gambi_execute` is the safe execution path.
- `gambi_execute_escalated` is the escalated execution path.
- Every upstream tool gets an effective policy level: `safe` or `escalated`.

**Why**

- You can pre-approve low-risk workflows while still allowing privileged operations.
- Agents get a deterministic escalation signal (`ESCALATION_REQUIRED`) instead of guessing.
- Dynamic MCP catalogs stay manageable as tools appear/disappear.

**How**

- Catalog policy mode can be set to `heuristic`, `all-safe`, `all-escalated`, or `custom`.
- In `custom`, set per-tool overrides (`safe`/`escalated`) for that catalog.
- Default heuristic marks a tool as `safe` when:
  - tool name starts with `get`, `list`, `search`, `lookup`, or `fetch` (case-insensitive), or
  - description starts with `get` (case-insensitive).
- If `gambi_execute` hits an escalated tool, it fails fast with `ESCALATION_REQUIRED`; rerun via `gambi_execute_escalated`.

## Execution Tools

Both execution tools let your agent write Python scripts that call upstream tools using dot syntax:

```python
# <server>.<tool>(**kwargs)
result = github.get_issue(issue_id="123")
slack.post_message(channel="eng", text=f"Issue loaded: {result['id']}")
```

- Calls are namespaced: `server.tool(**kwargs)`
- Keyword arguments only (positional args are rejected)
- Upstream errors (`is_error=true`) fail the execution
- Progress tokens are forwarded through nested calls
- Runs on the [Monty](https://github.com/pydantic/monty) Python runtime with resource limits (not a sandbox)
- `gambi_execute` enforces safe policy
- `gambi_execute_escalated` bypasses safe-policy blocking (still subject to runtime limits)

## Admin UI

The admin panel runs on loopback only and gives you:

- **Status** — health, exec status, server count, discovery failures
- **Servers** — add/remove upstream MCP servers
- **Tools** — see every tool your agent can access, with descriptions
- **Policy** — catalog mode (`heuristic`, `all-safe`, `all-escalated`, `custom`) + per-tool policy overrides
- **Overrides** — rewrite tool descriptions to help your agent understand what a tool does
- **Auth** — OAuth status per server, start/refresh flows
- **Logs** — tail recent server activity

Every panel has a "json" toggle to see the raw API response.

API endpoints are also available directly: `/health`, `/status`, `/servers`, `/tools`, `/logs`, `/auth/status`, `/config/export`, `/config/import`, and more.

## CLI reference

```bash
gambi serve                        # start (default: admin on :3333, exec enabled)
gambi serve --admin-port 4000      # custom admin port
gambi serve --no-exec              # disable both execution tools
gambi add <name> <url>             # add upstream server
gambi add <name> <url> --policy all-escalated
gambi remove <name>                # remove upstream server
gambi policy <name> custom         # set catalog policy mode
gambi list                         # list configured servers
```

## Configuration

Config directory: `~/.config/gambi/`

| File | Purpose |
|------|---------|
| `config.json` | Servers, policy modes/overrides, and tool description overrides |
| `tokens.json` | OAuth tokens (file-backed profiles) |

Token storage profiles:
- `GAMBI_PROFILE=local` (default): file-backed tokens
- `GAMBI_PROFILE=production`: OS keychain

## Environment variables

**Execution limits:**

| Variable | Purpose |
|----------|---------|
| `GAMBI_EXEC_MAX_WALL_MS` | Max wall-clock time |
| `GAMBI_EXEC_MAX_CPU_SECS` | Max CPU seconds |
| `GAMBI_EXEC_MAX_MEM_BYTES` | Max memory |
| `GAMBI_EXEC_MAX_STDOUT_BYTES` | Max stdout capture |

**Upstream behavior:**

| Variable | Default | Purpose |
|----------|---------|---------|
| `GAMBI_UPSTREAM_REQUEST_TIMEOUT_MS` | `3000` | Tool call timeout |
| `GAMBI_UPSTREAM_DISCOVERY_TIMEOUT_MS` | `3000` | Tool discovery timeout |

**Compatibility:**

| Variable | Purpose |
|----------|---------|
| `GAMBI_TOOL_SCHEMA_COMPAT_NORMALIZATION=0` | Disable strict-client schema normalization (enabled by default) |

## Security model

- Admin UI/API: loopback-only (`127.0.0.1` / `::1`)
- MCP endpoint: stdio-only
- Trust model: same OS user (any process under your user can access gambi)
- Config/token files: `0600` permissions, `0700` config directory
- `gambi_execute` / `gambi_execute_escalated`: Monty runtime resource limits, not a full OS sandbox

## Development

```bash
cargo fmt --all --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test
```

Full CI harness:

```bash
./scripts/test-harness.sh ci       # full CI run
./scripts/test-harness.sh smoke    # fast smoke test
./scripts/test-harness.sh ui       # Playwright UI tests only
```

UI tests need: `npm ci && npm run test:ui:install`

## License

See repository license terms.
