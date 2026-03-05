# Hostless

Hostless is a local AI proxy and local reverse proxy.

- Forward proxy plane: OpenAI-compatible API on `localhost` that injects locally stored provider keys.
- Reverse proxy plane: per-app `.localhost` subdomains for local-origin isolation.

## What It Solves

- Keeps provider API keys out of browser/app runtime.
- Provides origin-scoped bridge tokens (`sk_local_*`) instead of raw provider keys.
- Lets local apps run under unique origins like `myapp.localhost:11434`.

## Quick Start

```bash
# Build
make clean
make build

# Start daemon
make serve

# Add a provider key
make keys-add PROVIDER=openai KEY=sk-your-key-here

# Health check
make health
```

Minimal test request:

```bash
curl -X POST http://localhost:11434/v1/chat/completions \
  -H "Authorization: Bearer sk_local_..." \
  -H "Content-Type: application/json" \
  -d '{"model":"gpt-4o-mini","messages":[{"role":"user","content":"hello"}]}'
```

## Command Surface

Top-level commands are documented in `docs/cli-commands.md`.

- `hostless proxy`
- `hostless serve` (blocking version of `proxy start`)
- `hostless run`
- `hostless stop`
- `hostless list`
- `hostless route`
- `hostless alias`
- `hostless hosts`
- `hostless trust`
- `hostless keys`
- `hostless origins`
- `hostless config`
- `hostless auth`
- `hostless token` (bridge token lifecycle)

## Security Notes

- API keys are stored in `~/.hostless/keys.env` (plaintext dotenv format).
- Management endpoints require `x-hostless-admin: <token>` plus localhost access constraints.
- `POST /auth/token` is local-only: requires admin auth, no `Origin` header, and localhost `Host` (`localhost`, `127.0.0.1`, `[::1]`).
- Token persistence modes: `off` (default), `file`, `keychain`.
- SSE streaming is supported; WebSocket upgrade pass-through is supported for reverse-proxied local apps.

## Data Files

Hostless stores runtime/config files in `~/.hostless/`.

- `config.json`
- `keys.env`
- `tokens.json` (when persistence is `file` or `keychain`)
- `routes.json`
- `admin.token`
- `hostless.pid`
- `hostless.port`
- `localhost.crt`, `localhost.key` (when TLS is enabled)

## Documentation Map

- `docs/cli-commands.md`: canonical CLI reference
- `docs/auth-and-security.md`: token model, auth middleware, provider routing, key storage
- `docs/reverse-proxy.md`: host-header dispatch, reverse proxy internals, route table
- `docs/process-management.md`: `hostless run`, framework flag injection, daemon lifecycle
- `docs/testing.md`: test suite structure and testing patterns
- `docs/developer.md`: hostless local development workflow (repo maintainers)
- `AGENTS.md`: architecture map for maintainers and coding agents

## License

MIT
