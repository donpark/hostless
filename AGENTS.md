# AGENTS.md

Instructions for AI coding agents working with this codebase.

## Project Overview

Hostless is a local proxy server (Rust/Axum) with two traffic planes on one port:

1. `.localhost` subdomains -> reverse proxy to local apps
2. `localhost` / `127.0.0.1` -> management API + LLM forward proxy (`/v1/*`)

Core goals:

- Keep provider keys local to the machine.
- Expose an OpenAI-compatible API surface for apps.
- Enforce per-origin token boundaries for browser scenarios.

## Documentation Canonical Sources

Use these as source-of-truth before editing docs or code comments:

- `docs/cli-commands.md`: full command surface and flags
- `docs/auth-and-security.md`: bridge tokens, middleware, provider routing, key storage
- `docs/reverse-proxy.md`: host-header dispatch and reverse proxy internals
- `docs/process-management.md`: run/wrap process lifecycle and daemon interactions
- `docs/testing.md`: test suite conventions and patterns

## Runtime Basics

- Language: Rust 2021, Axum 0.7, Tokio
- Default port: `11434` (`--port`)
- Config dir: `~/.hostless/`

Common files under `~/.hostless/`:

- `config.json`
- `keys.env`
- `tokens.json` (when token persistence is `file` or `keychain`)
- `routes.json`
- `admin.token`
- `hostless.pid`
- `hostless.port`

## CLI Surface (Summary)

Top-level commands are implemented in `src/main.rs`.

- `serve`, `proxy`, `run`, `stop`
- `list`, `route`, `alias`, `hosts`
- `trust`, `keys`, `origins`, `config`, `auth`, `token`

For exact syntax and flags, use `docs/cli-commands.md`.

## Management API Auth

Management endpoints require both:

- local-access constraints (localhost origin rules), and
- `x-hostless-admin: <token>` header (`admin.token` file-backed)

Primary logic:

- `src/auth/admin.rs`
- `src/server/routes.rs` (`ensure_local_management_access`)

## Architecture

```text
Browser app (myapp.localhost:11434)
  -> host dispatch middleware
  -> reverse proxy path
  -> local app upstream

Client app (localhost:11434/v1/*)
  -> auth middleware
  -> provider routing + transforms
  -> upstream provider APIs
```

Security boundary:

- `.localhost` traffic is routed structurally to reverse proxy.
- Management and LLM proxy endpoints are only on localhost path.

## Module Map

| Module | Purpose |
|---|---|
| `src/main.rs` | CLI entry point and command wiring |
| `src/server/dispatch.rs` | Host-header dispatch boundary |
| `src/server/reverse_proxy.rs` | Reverse proxy forwarding logic |
| `src/server/route_table.rs` | Route persistence and lookup |
| `src/server/routes.rs` | HTTP handlers (`/v1/*`, `/auth/*`, `/routes/*`, `/health`) |
| `src/server/streaming.rs` | SSE streaming proxy |
| `src/auth/bridge_token.rs` | Token lifecycle, scopes, expiry, rate limits |
| `src/auth/middleware.rs` | Bearer token validation for `/v1/*` |
| `src/auth/admin.rs` | Admin token load/create and header contract |
| `src/process/manager.rs` | Wrapped process spawn/register/deregister flow |
| `src/providers/*` | Provider adapters and model routing |
| `src/vault/store.rs` | Provider key storage in `keys.env` + legacy migration |
| `src/vault/encryption.rs` | Legacy vault decryption support for migration |
| `src/config.rs` | Persistent config (`config.json`) |
| `src/tls.rs` | Optional local TLS cert generation |

## Testing

```bash
cargo test
cargo test --features internal-testing
OPENAI_API_KEY=sk-... cargo test --features internal-testing --test openai_e2e -- --ignored
```

Critical testing rule:

- Use ephemeral state in tests (`AppState::new_ephemeral`, `VaultStore::open_ephemeral`), not disk/keychain-backed constructors.

See `docs/testing.md` for full patterns and fixture guidance.

## Related Repos (Context Only)

- `../app`: sibling app repo that consumes hostless
- `../test-web`: sibling test web page used in local integration workflows

Hostless docs in this repo should remain focused on hostless behavior/contracts rather than app-side implementation details.

<!-- opensrc:start -->

## Source Code Reference

Source code for dependencies is available in `opensrc/` for deeper understanding of implementation details.

See `opensrc/sources.json` for the list of available packages and their versions.

Use this source code when you need to understand how a package works internally, not just its types/interface.

### Fetching Additional Source Code

```bash
npx opensrc <package>
npx opensrc pypi:<package>
npx opensrc crates:<package>
npx opensrc <owner>/<repo>
```

<!-- opensrc:end -->
