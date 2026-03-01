# Changelog

All notable changes to this project are documented in this file.

## v0.1.2 - 2026-03-01

### Added
- Admin UI server target input now accepts command form directly (for example `npx -y @railway/mcp-server`) and normalizes it to `stdio://...`.
- `gambi_execute` runtime now exposes `tool(name, **kwargs)` and `call_tool(name, **kwargs)` helpers for calling namespaced tools by string, including non-identifier tool names containing `-`.

### Changed
- OAuth refresh recovery now attempts refresh for all detected HTTP auth failures when a refresh token is available.
- Legacy SSE upstream initialization now probes auth failures so 401/403 responses are surfaced as explicit auth-required errors.
- Server names now reject `-` to avoid ambiguous identifier behavior in code-mode dot syntax.

### Docs
- Updated execution guidance in MCP help/tool descriptions and README to document string-based tool calls for dashed tool names.
- Updated admin UI docs to mention command-form server targets.

## v0.1.1 - 2026-02-24

### Added
- `gambi add` now accepts stdio command-style targets directly:
  - `gambi add <name> <command> [args...]`
  - example: `gambi add playwright npx @playwright/mcp@latest`

### Changed
- `gambi add` still accepts `http://`, `https://`, and `stdio://` URLs, and now maps command targets to encoded `stdio://` entries.
- stdio target parsing now preserves host+path command forms (for example `stdio://node_modules/.bin/mcp-server?...`) so local binary paths resolve correctly.

### Docs
- Updated server-add examples to include Playwright MCP command-style setup.
- Updated installer examples to reference `v0.1.1`.
