# AGENTS.md

Instructions for AI coding agents working with this codebase.

## Project Overview

**hostless** is a local AI proxy server (Rust/Axum) that manages LLM API keys securely. Applications send OpenAI-compatible requests to hostless, which injects the real API key and forwards to the upstream provider. Keys never leave the machine or reach client apps.

Additionally, hostless acts as a **reverse proxy** for local dev servers, assigning each app a unique `.localhost` subdomain (e.g., `myapp.localhost:11434`). This provides per-app origin isolation so apps cannot share bearer tokens. See [docs/reverse-proxy.md](docs/reverse-proxy.md).

- **Language/Framework**: Rust 2021 edition, Axum 0.7, Tokio async runtime
- **Default port**: 11434 (configurable via `--port`)
- **Config directory**: `~/.hostless/` (config.json, keys.vault, salt, routes.json, hostless.pid, hostless.port)

## Related repos

- **app**: `app` folder in the sibling directory is an Electrobun app that bundles and uses this proxy. It can be found using relative path `../app`.
- **test-web**: `test-web` folder in the sibling directory is a single page HTML to be served using `http.server` as a webapp to test hostless with.

## Detailed Documentation

| Document | Contents |
|----------|----------|
| [docs/auth-and-security.md](docs/auth-and-security.md) | Bridge tokens, middleware flow, origin security, provider routing, vault encryption |
| [docs/reverse-proxy.md](docs/reverse-proxy.md) | Host-header dispatch, reverse proxy, route table, security model |
| [docs/process-management.md](docs/process-management.md) | Process wrapping, daemon mode, framework detection, env injection |
| [docs/cli-commands.md](docs/cli-commands.md) | CLI command reference (serve/run/stop/route/trust/keys/origins/auth/token) |
| [docs/testing.md](docs/testing.md) | Full test suite inventory (139 tests), test patterns, portless test mapping |

## CLI Command Inventory

Top-level commands implemented in `src/main.rs`:

- `hostless serve [--port] [--tls] [--verbose] [--dev-mode] [--daemon]`
- `hostless run <name> [--port] [--daemon-port] [--providers] [--models] [--rate-limit] [--ttl] [--no-token] -- <command...>`
- `hostless stop`
- `hostless route <list|add|remove> ...`
- `hostless trust`
- `hostless keys <add|list|remove|migrate> ...`
- `hostless origins <add|list|remove> ...`
- `hostless auth login <provider>`
- `hostless token <create|list|revoke> ...`

Subcommand details:

- `keys add <provider> <api_key> [--base-url <url>]`
- `keys list`
- `keys remove <provider>`
- `keys migrate`
- `origins add <origin>`
- `origins list`
- `origins remove <origin>`
- `route list`
- `route add <name> --port <port> [--daemon-port <port>]`
- `route remove <name> [--daemon-port <port>]`
- `auth login <provider>`
- `token create [--name <name>] [--origin <origin|*>] [--providers <csv>] [--models <csv>] [--rate-limit <n>] [--ttl <seconds>]`
- `token list`
- `token revoke <token-or-prefix>`

## CLI Execution Architecture

`hostless` uses a mixed execution model: some commands are local operations inside the binary, while others are thin clients to the running daemon HTTP API.

Commands that call daemon API endpoints:

- `route list|add|remove` → `/routes`, `/routes/register`, `/routes/deregister`
- `token create|list|revoke` → `/auth/token`, `/auth/tokens`, `/auth/revoke`
- `run` (indirectly via process manager) → daemon health check + route register/deregister

Commands that execute locally (no daemon management API call):

- `serve` (starts server / daemonizes)
- `stop` (PID-file + SIGTERM)
- `keys add|list|remove|migrate` (vault/key storage)
- `origins add|list|remove` (config file)
- `auth login` (OAuth flow initiation)
- `trust` (OS cert trust commands)

## Architecture

```
┌──────────────┐                           ┌──────────────────┐
│ Browser App  │──myapp.localhost:11434──► │                  │
│ (page load)  │◄───────────────────────── │                  │    ┌──────────────┐
└──────────────┘  reverse proxy            │                  │───►│ Local App    │
                                           │    hostless      │◄───│ :4001        │
┌──────────────┐                           │    (:11434)      │    └──────────────┘
│ Client App   │──localhost:11434────────► │                  │
│ (LLM call)   │◄───────────────────────── │                  │───►┌──────────────────┐
└──────────────┘  LLM forward proxy        │                  │◄───│ OpenAI / Anthro  │
                                           └────────┬─────────┘    │ / Google API     │
                                              ┌─────┴──────┐       └──────────────────┘
                                              │ OS Keychain│
                                              │ keys.vault │
                                              └────────────┘
```

Two traffic planes on a single port, separated by Host-header dispatch:
1. **`<name>.localhost`** → Reverse proxy to local app (browser→app direction)
2. **`localhost` / `127.0.0.1`** → Management API + LLM forward proxy (app→LLM direction)

See [docs/reverse-proxy.md](docs/reverse-proxy.md) for the full dispatch architecture.

### Module Map

| Module | Purpose |
|--------|---------|
| `src/main.rs` | CLI entry point (clap). Subcommands: `serve`, `keys`, `origins`, `auth`, `token`, `run`, `stop`, `route`, `trust` |
| `src/lib.rs` | Library crate exposing `auth`, `config`, `process`, `providers`, `server`, `vault` for integration tests |
| `src/server/mod.rs` | `AppState` (shared state), `create_router()` (Axum router), background token + route cleanup task |
| `src/server/dispatch.rs` | **Host-header dispatch** — structural security boundary. Routes `*.localhost` → reverse proxy, `localhost` → management API |
| `src/server/reverse_proxy.rs` | HTTP reverse proxy: forwards to `127.0.0.1:<port>`, X-Forwarded headers, hop-by-hop stripping, loop detection |
| `src/server/route_table.rs` | `RouteTable` — maps `.localhost` hostnames to local ports. In-memory + file-backed persistence to `~/.hostless/routes.json` |
| `src/server/routes.rs` | HTTP handlers: `chat_completions`, `embeddings`, `register_origin`, `create_token`, `auth_refresh`, `auth_revoke`, `auth_list_tokens`, `health`, `oauth_callback`, `register_route`, `deregister_route`, `list_routes` |
| `src/server/streaming.rs` | SSE streaming proxy for `stream: true` requests |
| `src/server/cors.rs` | CORS layer with URL-parsing origin checks (not `starts_with`) |
| `src/process/mod.rs` | Process management module |
| `src/process/manager.rs` | Process wrapping: `spawn_and_manage()`, `find_available_port()`, `inject_framework_flags()`, `build_child_env()`, daemon communication |
| `src/auth/bridge_token.rs` | `BridgeTokenManager` — in-memory token store with origin binding, provider/model scoping, rate limiting, expiry, cleanup |
| `src/auth/middleware.rs` | Axum middleware for `/v1/*` routes. Validates bearer tokens, checks origin, stashes `ValidatedToken` in request extensions |
| `src/auth/oauth.rs` | OAuth2/PKCE flow (for OpenRouter) |
| `src/providers/mod.rs` | `Provider` trait, `resolve_provider()` (model name → provider key), `get_provider()` |
| `src/providers/openai.rs` | OpenAI provider (passthrough, default) |
| `src/providers/anthropic.rs` | Anthropic provider (request/response transform to/from OpenAI format) |
| `src/providers/google.rs` | Google Gemini provider (request/response transform, API key via query param) |
| `src/vault/store.rs` | `VaultStore` — encrypted API key storage (AES-256-GCM), file-backed at `~/.hostless/keys.vault` |
| `src/vault/keychain.rs` | OS keychain integration for master key (`keyring` crate, apple-native/linux-native) |
| `src/vault/encryption.rs` | AES-256-GCM encrypt/decrypt (nonce \|\| ciphertext \|\| tag, base64 encoded) |
| `src/config.rs` | `AppConfig` — allowed origins, OAuth client configs, provider URL overrides. Persisted to `~/.hostless/config.json` |
| `src/tls.rs` | Optional TLS with auto-generated local certs (rcgen) |

## Auth, Providers & Vault

Bridge tokens (`sk_local_*`) provide origin-bound, provider/model-scoped, rate-limited access. Two creation paths: browser dialog (`/auth/register`) and CLI (`/auth/token`). Middleware on `/v1/*` validates tokens, checks origin, enforces scopes. Provider routing maps model names to upstream APIs (OpenAI/Anthropic/Google) with automatic request/response transformation. API keys are AES-256-GCM encrypted in `~/.hostless/keys.vault` with a master key from the OS keychain.

See [docs/auth-and-security.md](docs/auth-and-security.md) for full details on tokens, middleware flow, origin security, provider routing, and vault encryption.

## HTTP Endpoints

| Method | Path | Auth | Purpose |
|--------|------|------|---------|
| GET | `/health` | None | Health check |
| POST | `/v1/chat/completions` | Bearer token (or dev-mode bypass) | Main proxy endpoint |
| POST | `/v1/embeddings` | Bearer token (or dev-mode bypass) | OpenAI embeddings proxy |
| POST | `/auth/register` | None | Browser app registration (shows native dialog) |
| POST | `/auth/token` | None (CLI only, no Origin) | Direct token creation |
| POST | `/auth/refresh` | None | Extend token TTL |
| POST | `/auth/revoke` | None | Revoke a token |
| GET | `/auth/tokens` | None | List active tokens |
| GET | `/callback` | None | OAuth callback |
| POST | `/routes/register` | None (localhost-only guard) | Register a `.localhost` route + auto-provision token |
| POST | `/routes/deregister` | None (localhost-only guard) | Remove a route + revoke token |
| GET | `/routes` | None | List active routes |

See [docs/cli-commands.md](docs/cli-commands.md) for the full CLI reference.

## Building & Running

```bash
cargo build                          # Build
cargo run -- serve [--dev-mode] [--daemon] [--tls]  # Start server
cargo run -- run myapp -- npm run dev               # Wrap an app
cargo run -- keys add openai sk-...                  # Store API key
cargo run -- token create --origin "*"               # Create bridge token
```

See [docs/cli-commands.md](docs/cli-commands.md) for the full CLI reference with all flags and examples.

## Testing

```bash
cargo test                          # 139 tests, no API key needed
OPENAI_API_KEY=sk-... cargo test --test openai_e2e -- --ignored  # E2E (3 tests)
```

**Critical**: Always use `AppState::new_ephemeral()` / `VaultStore::open_ephemeral()` in tests — never `::new()` / `::open()`, which trigger macOS keychain dialogs.

See [docs/testing.md](docs/testing.md) for the full test inventory, test patterns, and portless test mapping.

## Key Design Decisions

1. **In-memory tokens**: Bridge tokens live in a `HashMap<String, BridgeToken>` behind `RwLock`. Fast but lost on restart. Persistence (encrypted to `~/.hostless/tokens.json`) is planned but not implemented.
2. **Dev mode opt-in**: `--dev-mode` flag relaxes auth for local development. Without it, ALL requests need tokens.
3. **OpenAI-compatible API surface**: Clients talk OpenAI format; providers transform internally.
4. **No remote network in unit tests**: All unit/integration tests are offline. E2e tests are `#[ignore]` and require explicit opt-in.
5. **Background cleanup**: A tokio task runs every 300s to remove expired tokens and stale routes.
6. **Native dialogs for browser apps**: `rfd` crate shows OS-native Yes/No dialog when a browser app requests access via `/auth/register`. CLI path (`/auth/token`) skips this since the caller is the machine owner.
7. **Host-header dispatch firewall**: `.localhost` subdomain traffic is structurally isolated from management/LLM proxy endpoints. This is a routing decision, not an access-control check — subdomain requests physically cannot reach `/auth/*` or `/v1/*`.
8. **Auto-token provisioning**: Route registration auto-creates an origin-scoped bridge token. The token is bound to the app's `.localhost` origin, so even if leaked it cannot be reused by a different app.
9. **Per-app origin isolation**: Each app gets its own `.localhost` subdomain, creating a distinct browser origin. Apps cannot read each other's cookies, localStorage, or bearer tokens.

## Common Patterns

See the detailed docs for step-by-step recipes:
- **Adding a provider** → [docs/auth-and-security.md](docs/auth-and-security.md#adding-a-new-provider)
- **Adding an HTTP route** → [docs/testing.md](docs/testing.md#adding-a-new-http-route)
- **Adding a proxy/dispatch test** → [docs/testing.md](docs/testing.md#for-reverse-proxy--dispatch-behavior)
- **Vault/route table in tests** → [docs/testing.md](docs/testing.md#working-with-the-route-table-in-tests) and [docs/auth-and-security.md](docs/auth-and-security.md#working-with-the-vault-in-tests)

Electrobun app that uses this proxy is at @app.

<!-- opensrc:start -->

## Source Code Reference

Source code for dependencies is available in `opensrc/` for deeper understanding of implementation details.

See `opensrc/sources.json` for the list of available packages and their versions.

Use this source code when you need to understand how a package works internally, not just its types/interface.

### Fetching Additional Source Code

To fetch source code for a package or repository you need to understand, run:

```bash
npx opensrc <package>           # npm package (e.g., npx opensrc zod)
npx opensrc pypi:<package>      # Python package (e.g., npx opensrc pypi:requests)
npx opensrc crates:<package>    # Rust crate (e.g., npx opensrc crates:serde)
npx opensrc <owner>/<repo>      # GitHub repo (e.g., npx opensrc vercel/ai)
```

<!-- opensrc:end -->
