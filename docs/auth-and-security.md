# Auth, Providers & Vault

Related docs:

- `docs/cli-commands.md`
- `docs/proxy-api.md`
- `docs/reverse-proxy.md`
- `docs/process-management.md`

## Auth Model

### Bridge Tokens

Tokens are prefixed `sk_local_` and stored in-memory in `BridgeTokenManager` (HashMap behind RwLock), with optional persistence configured at server start:

- `off` (default): in-memory only, tokens are lost on restart
- `file`: persisted as plaintext `~/.hostless/tokens.json`
- `keychain`: persisted in `~/.hostless/tokens.json` encrypted with a key stored in OS keychain

Each token has:
- **Origin binding**: exact origin match, or `"*"` wildcard (matches any origin including empty)
- **Provider scope**: optional list of allowed provider keys (`["openai", "anthropic", "google"]`)
- **Model scope**: optional list of allowed model patterns (glob-style, e.g., `"gpt-4o*"`)
- **Rate limit**: optional requests-per-hour
- **TTL**: expiry duration (default 24h for CLI, 1h for browser)
- **App name**: optional human-readable label

### Two Token Creation Paths

1. **`POST /auth/register`** — Browser apps. Shows a native OS dialog (rfd) asking user to approve. Origin required.
2. **`POST /auth/token`** — CLI/local automation path. No dialog. Gated by:
	- valid admin header (`x-hostless-admin`),
	- **no Origin header**, and
	- `Host` restricted to bare localhost (`localhost`, `127.0.0.1`, `[::1]`).

This is what `hostless token create` and local `curl` usage rely on.

Desktop guidance:

- **Webview desktop apps** should use `POST /auth/register` when the renderer presents a stable origin and you want the normal interactive approval flow.
- **Native GUI apps** should currently use `hostless token create` / `POST /auth/token` as a trusted local bootstrap path.
- CLI token creation does not show the approval dialog because possession of the local admin token is treated as machine-owner authorization already.

### URL Scheme Handler Contract (`hostless:`)

Custom URL scheme registration and native app packaging (for example, macOS `.app` handlers) are outside this repo's scope.
Hostless documents only the daemon/API contract consumed by that handler.

Handler-facing expectations:

- The handler forwards registration intent to hostless using `POST /auth/register`.
- The handler discovers the active daemon port from `~/.hostless/hostless.port` (fallback `11434`).
- The handler preserves and returns caller `state` as-is for CSRF-style correlation.
- Callback payload includes resolved runtime `port` and `local_url`.
- Bridge token is returned in URL fragment (`#token=...`) and never in query string.

Desktop apps do not need to use the URL scheme handler unless they are intentionally delegating registration to a separate helper process. A packaged desktop app can instead call `POST /auth/register` directly from a webview shell, or use `hostless token create` for native GUI bootstrap.

### Middleware Flow (`/v1/*` routes, including `/v1/chat/completions`, `/v1/responses`, and `/v1/realtime`)

1. Extract `Origin` header (empty for CLI)
2. **Dev mode** (`--dev-mode`): bare localhost / empty origin → bypass auth entirely (no `ValidatedToken` in extensions → no scope checks)
3. Otherwise: require `Authorization: Bearer sk_local_...`
4. Validate token exists, not expired, origin matches
5. Check rate limit
6. Insert `ValidatedToken` into request extensions
7. Route handler extracts `ValidatedToken`, enforces provider and model scope

### Origin Security

- `is_bare_localhost()` — URL-parses to check host is exactly `"localhost"` or `"127.0.0.1"`. Blocks `localhost.evil.com`.
- `is_localhost_subdomain()` — checks `.localhost` TLD (RFC 6761). e.g., `myapp.localhost:1355` → distinct per-app identity.
- `.localhost` subdomains **always require tokens**, even in dev mode.
- Bridge tokens for browser and webview flows are validated against the request `Origin` header.
- Native GUI clients often send no `Origin` header. For that reason, the current native desktop recommendation is CLI-provisioned tokens rather than browser-style interactive registration.
- `"*"` wildcard tokens match any origin, including empty origin, and are therefore more permissive than `.localhost` or specific-origin tokens.

### Desktop Security Notes

- Prefer webview registration only when the desktop shell can keep a stable, predictable origin across registration and later `/v1/*` calls.
- Prefer CLI provisioning for native GUI apps whose requests come from native HTTP clients without a browser origin model.
- Store desktop bridge tokens in OS-backed credential stores when possible.
- Treat `hostless token create` as an admin/bootstrap workflow. It is appropriate for trusted local apps, but it is not the same consent surface as `POST /auth/register`.

## Provider Routing

Model name determines the upstream provider:
- `claude*` or `anthropic/...` → Anthropic
- `gemini*` or `google/...` → Google
- Everything else → OpenAI (default)
- Explicit prefix: `openai/gpt-4o` strips prefix before forwarding

Current endpoint support:

Detailed request and response contracts for `/v1/*`, `/auth/*`, `/routes/*`, and `/health` live in `docs/proxy-api.md`.

At a high level:

- `/v1/chat/completions`: OpenAI-compatible request surface with Anthropic and Google routing/transforms
- `/v1/responses`: OpenAI-compatible HTTP and WebSocket support for OpenAI-compatible models
- `/v1/realtime`: OpenAI-compatible realtime websocket passthrough
- media routes and `/v1/embeddings`: OpenAI-compatible passthroughs with scope enforcement as described below

### Compatibility Matrix (M1/M2/M3)

| Endpoint | Milestone | Request Type | Transport | Provider Coverage | Scope Enforcement |
|---|---|---|---|---|---|
| `/v1/chat/completions` | baseline | JSON | HTTP | OpenAI + Anthropic + Google (via transforms) | Provider + model |
| `/v1/responses` | M1 | JSON/SSE (HTTP), event frames (WebSocket mode) | HTTP + SSE + WebSocket | OpenAI-compatible only | Provider + model (WebSocket mode requires `?model=...`) |
| `/v1/realtime` | M2 | WebSocket upgrade | WebSocket | OpenAI-compatible only | Provider + model (pre-upgrade) |
| `/v1/audio/speech` | M3 | JSON | HTTP (binary response passthrough) | OpenAI-compatible only | Provider + model (from JSON `model`) |
| `/v1/audio/transcriptions` | M3 | Multipart | HTTP | OpenAI-compatible only | Provider |
| `/v1/audio/translations` | M3 | Multipart | HTTP | OpenAI-compatible only | Provider |
| `/v1/images/generations` | M3 | JSON | HTTP | OpenAI-compatible only | Provider + model (from JSON `model`) |
| `/v1/files` | M3 | Multipart | HTTP | OpenAI-compatible only | Provider |

Notes:
- All `/v1/*` routes are behind the same auth middleware.
- In `--dev-mode`, bare localhost and empty-origin requests bypass token auth, so route-level scope checks apply only when a validated token is present.
- For multipart routes, model-level scope is not currently extracted from body parts.

### WebSocket Mode Troubleshooting

- `401` / authentication errors: Ensure websocket handshakes include `Authorization: Bearer sk_local_...`.
- `403` / scope errors: Token provider/model scope may block the route. Reissue a token allowing `openai` and the target model pattern.
- `400 previous_response_not_found`: With `store=false`, continuation state may not survive reconnects. Start a new chain or resend full context.
- `502` from hostless: Upstream websocket upgrade was rejected or unreachable. Verify OpenAI key, optional base URL override, and outbound network access.
- Long-lived chains: Reconnect before/at upstream connection lifetime limits and continue with `previous_response_id` when available.

Media route scope details:
- Provider scope enforcement is applied to all media routes.
- Model scope enforcement is applied when the request body is JSON with a `model` field (for example `/v1/audio/speech`, `/v1/images/generations`).
- Multipart routes (`/v1/files`, `/v1/audio/transcriptions`, `/v1/audio/translations`) currently enforce provider scope only.

The `Provider` trait handles request/response transformation so clients always use OpenAI-compatible format.

### Adding a New Provider

1. Create `src/providers/newprovider.rs` implementing the `Provider` trait
2. Add match arm in `get_provider()` and detection logic in `resolve_provider()` in `src/providers/mod.rs`
3. Add `pub mod newprovider;` to `src/providers/mod.rs`

## Vault & Encryption

- **API keys**: Stored in plaintext dotenv-style file at `~/.hostless/keys.env`
- **Format**: `HOSTLESS_KEY_<provider>=...` and optional `HOSTLESS_BASE_URL_<provider>=...`
- **Operational note**: This removes OS keychain and password prompts, but secrets are no longer encrypted at rest.

### Working with the Vault in Tests

```rust
let state = hostless::server::AppState::new_ephemeral(0, true);
state.vault.add_key("openai", "sk-test-key", None).await.unwrap();
let (key, base_url) = state.vault.get_key("openai").await.unwrap().unwrap();
```
