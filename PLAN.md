# gambi

A local MCP aggregator with code execution. Single Rust binary, just works.

## The Problem

You want one local server that:
- Aggregates multiple MCP servers (Port, Atlassian, GitHub, etc.)
- Handles auth to remote servers itself (OAuth, API keys) via a web UI
- Exposes everything to clients (Claude Code, Codex) over a simple, unauthenticated local connection
- Clients need **one config line**, not one per MCP server
- Supports a **code execution mode** where agents chain calls across servers in one shot

Existing solutions (mcp-hub) are poorly documented, have broken UX, and don't work with Claude Code's SSE/OAuth implementation.

## The Solution

A single Rust binary with two interfaces:

1. **MCP endpoint** (stdio) — clients connect here, get all tools from all configured servers + a `gambi_execute` code execution tool
2. **Admin web UI** (`http://localhost:<port>`) — manage servers, authenticate, see status

```
┌─────────────┐     stdio      ┌───────┐     SSE/HTTP     ┌────────────┐
│ Claude Code  │───────────────►│       │────────────────►│ Port        │
└─────────────┘                │       │                  └────────────┘
                               │ gambi │     OAuth         ┌────────────┐
┌─────────────┐     stdio      │       │────────────────►│ Atlassian   │
│ Codex        │───────────────►│       │                  └────────────┘
└─────────────┘                │       │     API key       ┌────────────┐
                               │       │────────────────►│ GitHub      │
                               └───┬───┘                  └────────────┘
                                   │
                                   │ :3333
                                   ▼
                              ┌─────────┐
                              │ Admin UI │
                              │ (web)    │
                              └─────────┘
```

## Two Modes

Agents get **both** modes simultaneously:

### Direct Mode
Each upstream tool exposed individually with namespacing. Good for simple, single calls.
- `port:list_entities`, `atlassian:search_issues`, `github:create_pr`

### Code Mode
A single `gambi_execute` tool that accepts Python code. Good for chaining, batching, and complex workflows.

```python
entities = port.list_entities(blueprint="service")
results = []
for e in entities:
    ticket = atlassian.search(query=e["name"])
    results.append({"entity": e["name"], "has_ticket": ticket is not None})
return results
```

Powered by **Monty** (pydantic) — a minimal, sandboxed Python interpreter in Rust. Script calls pause execution on tool calls, gambi makes the real MCP call, then resumes. Built-in memory limits, allocation tracking, and execution time caps.

## Architecture

### Core Components

1. **Server Manager** — connects to upstream MCP servers, manages lifecycle
2. **Auth Manager** — handles OAuth flows, stores tokens, refreshes them
3. **Tool Router** — namespaces and routes tool calls to the right upstream server
4. **Code Executor** — Monty sandbox that exposes upstream tools as Python functions
5. **MCP Interface** — exposes aggregated tools + `gambi_execute` to clients via stdio
6. **Admin API + UI** — web interface for managing servers and auth

### Client Connection

Clients connect via **stdio** (spawned as a subprocess). Most reliable transport — no OAuth issues, no SSE problems, just works.

Claude Code / Codex / any MCP client:
```json
"gambi": {
  "command": "gambi",
  "args": ["serve"]
}
```

### Upstream Server Connection

gambi connects to upstream servers using whatever transport they need:
- **SSE** with OAuth (Atlassian, Port)
- **Streamable HTTP** with API keys
- **stdio** (local MCP servers)

### Auth Flow

1. User opens admin UI at `http://localhost:3333`
2. Clicks "Add Server" → enters URL (e.g., `https://mcp.atlassian.com/v1/mcp`)
3. gambi detects auth requirements (OAuth discovery)
4. User clicks "Authenticate" → browser opens OAuth flow
5. gambi receives callback, stores tokens
6. Server appears as "connected" in the UI
7. Tokens are persisted locally and refreshed automatically

### Token Storage

Tokens stored in `~/.config/gambi/`:
- `config.json` — server definitions
- `tokens.json` — OAuth tokens (plaintext JSON for MVP)

### Tool Namespacing

To avoid conflicts, tools are namespaced by server name:
- `port:list_entities`
- `atlassian:search_issues`
- `github:create_pr`

### Capability Aggregation

gambi aggregates **all three** MCP capability types from upstream servers:
- **Tools** — namespaced and routed
- **Resources** — namespaced and proxied
- **Prompts** — namespaced and proxied

## Tech Stack

| Component | Tech | Why |
|-----------|------|-----|
| Language | Rust | Single binary, no runtime deps, Monty is native Rust |
| MCP Server | `rmcp` (official Rust SDK) | stdio server for clients |
| MCP Client | `rmcp` + `reqwest`/`eventsource` for SSE | Connect to upstream servers |
| Code Execution | `pydantic-monty` | Sandboxed Python, pause/resume on tool calls, resource limits |
| Admin UI | `axum` + embedded static files (htmx + minimal CSS) | No build step, bundled in binary |
| Auth | OAuth 2.0 client (`oxide-auth` or custom with `reqwest`) | For remote servers requiring OAuth |
| Storage | JSON files in `~/.config/gambi/` | No database needed |
| CLI | `clap` | `gambi serve`, `gambi add`, `gambi list` |

## MVP Scope

### Phase 1: Core (make it work)
- [ ] Rust project scaffolding with `rmcp` and `axum`
- [ ] stdio MCP server that aggregates upstream servers
- [ ] Config file for defining upstream servers (`~/.config/gambi/config.json`)
- [ ] Connect to upstream stdio servers
- [ ] Connect to upstream SSE/HTTP servers (no auth)
- [ ] Tool, resource, and prompt namespacing and routing
- [ ] CLI: `gambi serve`, `gambi add <name> <url>`, `gambi list`, `gambi remove <name>`

### Phase 2: Code Execution (make it powerful)
- [ ] Integrate Monty as sandboxed Python executor
- [ ] Expose upstream tools as Python functions in the sandbox
- [ ] Implement pause/resume for tool calls during script execution
- [ ] `gambi_execute` tool with resource limits (memory, time)

### Phase 3: Auth (make it useful)
- [ ] OAuth flow for remote servers (browser-based)
- [ ] Token storage and refresh
- [ ] Admin web UI — server list, status, authenticate button
- [ ] Auto-reconnect on failure

### Phase 4: Polish (make it nice)
- [ ] Health checks and status reporting
- [ ] Hot-reload when config changes
- [ ] Log viewer in admin UI
- [ ] Export config for sharing

## Decisions Made

- **Single process** — `gambi serve` starts both the stdio MCP interface and the admin web UI
- **Aggregate everything** — tools, resources, and prompts from day one
- **Plaintext token storage** — `~/.config/gambi/tokens.json` for MVP
- **Rust** — single binary, Monty is native, no runtime deps
- **Both direct and code mode** — agents choose per-call

## Future Improvements

- **OS keychain for token storage** — use platform-native secure storage (macOS Keychain / Linux libsecret / Windows Credential Vault) instead of plaintext JSON
- **Streamable HTTP server** — expose gambi over HTTP too (not just stdio) for clients that prefer it
- **Script library** — save and reuse common scripts from the admin UI
