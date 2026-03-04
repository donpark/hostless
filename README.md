# Hostless

A local AI proxy that manages LLM API keys and injects them into forwarded requests. Your keys never leave your machine.

```
┌──────────────┐     ┌─────────────────┐     ┌──────────────────┐
│   Web App    │────▶│    Hostless      │────▶│  LLM Provider    │
│  (browser)   │◀────│  localhost:11434 │◀────│  (OpenAI, etc.)  │
│              │     │                  │     │                  │
│  No API keys │     │  ✓ Encrypted     │     │  Receives key    │
│  stored here │     │    key vault     │     │  from vault      │
└──────────────┘     └─────────────────┘     └──────────────────┘
```

## Features

- **Simple Local Key File** — API keys stored in `~/.hostless/keys.env` (dotenv-style plaintext)
- **Multi-Provider** — Routes to OpenAI, Anthropic, and Google Gemini; auto-detects provider from model name
- **OpenAI-Compatible API** — Drop-in replacement on `localhost:11434/v1/chat/completions`
- **Streaming** — Full SSE streaming support with per-provider format translation
- **Webapp Handshake** — `hostless://` custom URL scheme for zero-config webapp authorization
- **Bridge Tokens** — Time-limited, origin-scoped, model-scoped tokens with rate limiting
- **OAuth/PKCE** — Full OAuth2 flow with PKCE for providers that support it
- **Dynamic CORS** — Only approved webapp origins can make requests
- **Optional TLS** — Auto-generated localhost certificates

## Quick Start

```bash
# Build (ad-hoc codesigns so macOS Keychain "Always Allow" persists)
make clean            # reset
make build            # debug + copies ./hostless
make release          # optimized + copies ./hostless

# Run
make serve            # build + serve on 11434
make serve-port PORT=15055
make stop             # stop daemon tracked in PID file
make stop-all         # stop all running hostless processes

# Store an API key
make keys-add PROVIDER=openai KEY=sk-your-key-here
make keys-add PROVIDER=anthropic KEY=sk-ant-your-key-here
make keys-add PROVIDER=google KEY=AIza-your-key-here
make keys-migrate

# Register URL scheme through the desktop app bundle
make app-scheme-register

# Test it
make health
curl -X POST http://localhost:11434/v1/chat/completions \
  -H "Content-Type: application/json" \
  -d '{"model":"gpt-4o","messages":[{"role":"user","content":"Hello!"}]}'

# Wrapped local test webapp (auto-starts proxy if needed)
make test-web-wrapped WEB_PORT=4173 PORT=11434
make test-web-status WEB_PORT=4173 PORT=11434
```

## CLI Reference

### Top-Level Commands

| Command | Purpose |
|---|---|
| `hostless serve [--port 11434] [--tls] [--verbose] [--dev-mode] [--daemon]` | Start the proxy server |
| `hostless run <name> [options] -- <command...>` | Portless-clone: wrap an app, register `<name>.localhost`, and auto-provision token |
| `hostless stop` | Stop daemon mode server |
| `hostless route <list\|add\|remove> ...` | Portless-clone route management |
| `hostless trust` | Trust local hostless cert/CA in system store |
| `hostless keys <add\|list\|remove\|migrate> ...` | Manage provider API keys |
| `hostless origins <add\|list\|remove> ...` | Manage allowed origins |
| `hostless auth login <provider>` | Start provider OAuth login flow |
| `hostless token <create\|list\|revoke> ...` | Token-swapping: create/manage bridge tokens |

### Subcommands and Flags

| Command | Key flags / args |
|---|---|
| `hostless keys add <provider> <api_key>` | `--base-url <url>` |
| `hostless keys list` | none |
| `hostless keys remove <provider>` | none |
| `hostless keys migrate` | best-effort legacy vault migration |
| `hostless origins add <origin>` | origin URL |
| `hostless origins list` | none |
| `hostless origins remove <origin>` | origin URL |
| `hostless route list` | none |
| `hostless route add <name> --port <port>` | `--daemon-port <port>` |
| `hostless route remove <name>` | `--daemon-port <port>` |
| `hostless token create` | `--name`, `--origin`, `--providers`, `--models`, `--rate-limit`, `--ttl` |
| `hostless token list` | none |
| `hostless token revoke <token_or_prefix>` | token string or prefix |
| `hostless auth login <provider>` | provider is currently `openrouter` |
| `hostless run <name> -- <command...>` | `--port`, `--daemon-port`, `--providers`, `--models`, `--rate-limit`, `--ttl`, `--no-token` |

### CLI Execution Architecture

`hostless` uses a mixed execution model: some commands are local operations, and some are thin clients to the running daemon HTTP API.

- **Calls daemon API endpoints:**
  - `route list|add|remove` → `/routes`, `/routes/register`, `/routes/deregister`
  - `token create|list|revoke` → `/auth/token`, `/auth/tokens`, `/auth/revoke`
  - `run` (indirectly via process manager) → daemon health check + route register/deregister
- **Executes locally (no daemon management API call):**
  - `serve`, `stop`, `keys add|list|remove|migrate`, `origins add|list|remove`, `auth login`, `trust`

## Model Routing

The proxy auto-detects the provider from the model name:

| Model Pattern | Provider | Notes |
|---|---|---|
| `gpt-4o`, `o1-*`, `chatgpt-*` | OpenAI | Default for unknown models |
| `openai/gpt-4o` | OpenAI | Explicit prefix |
| `claude-*`, `anthropic/claude-*` | Anthropic | Transforms to Messages API |
| `gemini-*`, `google/gemini-*` | Google | Transforms to generateContent |
| Custom provider with `--base-url` | OpenAI-compat | Any OpenAI-compatible API |

## Webapp Integration

### 1. Custom URL Scheme Handshake

```javascript
// Step 1: Webapp triggers registration
const state = crypto.randomUUID();
const config = {
  origin: window.location.origin,
  callback: `${window.location.origin}/setup-success`,
  state: state,
};
window.location.href = `hostless://register?data=${encodeURIComponent(JSON.stringify(config))}`;

// Step 2: User sees native OS dialog "Allow myapp.com to use your AI credits?"

// Step 3: Proxy redirects back with token
// https://myapp.com/setup-success?port=<runtime-port>&local_url=http%3A%2F%2Flocalhost%3A<runtime-port>&state=...&expires_in=3600#token=sk_local_...
// Note: <runtime-port> is discovered from the running hostless daemon (not hardcoded).

// Step 4: Use the token
const response = await fetch('http://localhost:11434/v1/chat/completions', {
  method: 'POST',
  headers: {
    'Content-Type': 'application/json',
    'Authorization': `Bearer ${bridgeToken}`,
  },
  body: JSON.stringify({
    model: 'gpt-4o',
    messages: [{ role: 'user', content: 'Hello!' }],
  }),
});
```

When using the custom URL scheme flow, always read `port` / `local_url` from the callback payload and build your proxy base URL dynamically. Do not assume `11434`.

### Connect with Hostless Button (Drop-in Snippet)

```html
<button id="connect-hostless" type="button">Connect with Hostless</button>

<script>
  function readHostlessCallback() {
    const query = new URLSearchParams(window.location.search);
    const hash = new URLSearchParams(window.location.hash.replace(/^#/, ""));

    const token = hash.get("token");
    const localUrl = query.get("local_url");
    const port = query.get("port");
    const state = query.get("state");

    if (!token || (!localUrl && !port)) return null;

    const proxyBase = localUrl || `http://localhost:${port}`;
    return { token, proxyBase, state };
  }

  document.getElementById("connect-hostless").addEventListener("click", () => {
    const state = crypto.randomUUID();
    const callback = `${window.location.origin}${window.location.pathname}`;
    const payload = {
      origin: window.location.origin,
      callback,
      state,
    };

    window.location.href = `hostless://register?data=${encodeURIComponent(JSON.stringify(payload))}`;
  });

  const result = readHostlessCallback();
  if (result) {
    // Persist where your app expects provider settings.
    localStorage.setItem("hostless_token", result.token);
    localStorage.setItem("hostless_proxy_base", result.proxyBase);

    // Optional cleanup: remove token from URL after parsing.
    history.replaceState({}, document.title, window.location.pathname);
  }
</script>
```

### 2. Direct Registration (No URL Scheme)

```javascript
const response = await fetch('http://localhost:11434/auth/register', {
  method: 'POST',
  headers: { 'Content-Type': 'application/json' },
  body: JSON.stringify({
    origin: window.location.origin,
    callback: `${window.location.origin}/setup-success`,
    state: crypto.randomUUID(),
  }),
});
```

## Security Model

| Layer | Protection |
|---|---|
| **Key Storage** | AES-256-GCM encryption, master key in OS keychain |
| **Bridge Tokens** | Time-limited (1h default), origin-bound, high-entropy |
| **Origin Scoping** | CORS + token validation ensures only approved webapps connect |
| **Model Scoping** | Optional per-token model restrictions |
| **Rate Limiting** | Per-token request limits (token-bucket) |
| **PKCE** | OAuth code exchange protected against interception |
| **CSRF State** | Handshake uses random state parameter verification |

## Management API Authentication

Hostless management endpoints now require a local admin header in addition to localhost routing/origin checks.

- Header: `x-hostless-admin: <token>`
- Token file: `~/.hostless/admin.token`
- Token lifecycle: generated automatically by `hostless serve` if missing
- Scope: required for management endpoints (`/auth/token`, `/auth/refresh`, `/auth/revoke`, `/auth/tokens`, `/routes*`)

The `hostless` CLI and internal helpers automatically load and send this token when talking to the daemon.

### Upgrade note

When upgrading between versions that add or change management authentication, restart the daemon so command mode and server mode use the same protocol:

```bash
hostless stop
hostless serve --daemon
```

### Troubleshooting: 401 Missing or invalid management authentication

If `hostless route ...` or `hostless token ...` starts returning a 401-style error:

1. Restart daemon to regenerate/sync `~/.hostless/admin.token`:

```bash
hostless stop
hostless serve --daemon
```

2. Verify daemon health:

```bash
curl http://localhost:11434/health
```

3. Retry the command (for example):

```bash
hostless route list
```

## Architecture

```
src/
├── main.rs              # CLI (clap), server startup, signal handling
├── config.rs            # Persisted config (~/.hostless/config.json)
├── scheme.rs            # Custom URL scheme registration (macOS/Linux)
├── tls.rs               # Auto-generated TLS certificates (rcgen)
├── server/
│   ├── mod.rs           # Axum router, AppState
│   ├── routes.rs        # /health, /v1/chat/completions, /callback, /auth/register
│   ├── cors.rs          # Dynamic CORS allowlist
│   └── streaming.rs     # SSE stream proxy with per-provider transform
├── auth/
│   ├── bridge_token.rs  # Token issuance, validation, scoping, rate limits
│   ├── middleware.rs     # Axum auth middleware (token + origin check)
│   ├── handshake.rs     # hostless:// URL handler, webapp registration
│   └── oauth.rs         # OAuth2/PKCE flow for provider login
├── vault/
│   ├── encryption.rs    # AES-256-GCM encrypt/decrypt
│   ├── keychain.rs      # OS keychain (master key) + Argon2 fallback
│   └── store.rs         # Encrypted key persistence (~/.hostless/keys.vault)
└── providers/
    ├── mod.rs           # Provider trait, model→provider routing
    ├── openai.rs        # OpenAI/compatible passthrough
    ├── anthropic.rs     # Anthropic Messages API adapter
    └── google.rs        # Google Gemini generateContent adapter
```

## Data Storage

All data stored in `~/.hostless/`:

| File | Contents |
|---|---|
| `config.json` | Allowed origins, OAuth client configs, provider URL overrides |
| `keys.env` | Provider API keys + base URLs (`HOSTLESS_KEY_<provider>=...`) |
| `localhost.crt` | TLS certificate (when `--tls` used) |
| `localhost.key` | TLS private key (when `--tls` used) |

## License

MIT
