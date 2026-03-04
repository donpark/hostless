# Auth, Providers & Vault

## Auth Model

### Bridge Tokens

Tokens are prefixed `sk_local_` and stored in-memory in `BridgeTokenManager` (HashMap behind RwLock). **Tokens are lost on server restart** (persistence not yet implemented).

Each token has:
- **Origin binding**: exact origin match, or `"*"` wildcard (matches any origin including empty)
- **Provider scope**: optional list of allowed provider keys (`["openai", "anthropic", "google"]`)
- **Model scope**: optional list of allowed model patterns (glob-style, e.g., `"gpt-4o*"`)
- **Rate limit**: optional requests-per-hour
- **TTL**: expiry duration (default 24h for CLI, 1h for browser)
- **App name**: optional human-readable label

### Two Token Creation Paths

1. **`POST /auth/register`** — Browser apps. Shows a native OS dialog (rfd) asking user to approve. Origin required.
2. **`POST /auth/token`** — CLI only. No dialog. Gated by requiring **no Origin header** (browsers always send one). This is what `hostless token create` and `curl` use.

For browser handshake via `hostless://register`, the native URL handler forwards registration to the currently running daemon by reading `~/.hostless/hostless.port` (fallback `11434`). Callback payload includes the resolved runtime `port` and `local_url`, while the bridge token is returned in the URL fragment (`#token=...`); clients should use these values rather than hardcoding a port.

### Middleware Flow (`/v1/*` routes)

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

## Provider Routing

Model name determines the upstream provider:
- `claude*` or `anthropic/...` → Anthropic
- `gemini*` or `google/...` → Google
- Everything else → OpenAI (default)
- Explicit prefix: `openai/gpt-4o` strips prefix before forwarding

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
