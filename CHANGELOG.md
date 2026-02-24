# Changelog

All notable changes to this project are documented in this file.

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
