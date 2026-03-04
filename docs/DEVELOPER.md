# DEVELOPER.md

Guide for webapp developers integrating with hostless.

## Who this is for

You are building a web app and want either (a) safe BYOK access to LLM APIs without exposing provider keys in app runtime, or (b) per-app local isolation during development via a `.localhost` subdomain.

Primary hostless use-cases:

1. Web apps (local or hosted) that need user-provided LLM API keys (BYOK) without exposing those keys to the app runtime.
2. Web apps that run locally and want per-app isolation via a unique `.localhost` subdomain.

## What hostless gives your app

- OpenAI-compatible proxy at `http://localhost:11434/v1/...`
- Bridge tokens (`sk_local_*`) scoped to origin/model/provider/rate/TTL
- Per-app origin isolation via `<name>.localhost` routes
- Optional browser handshake using `hostless://register`

## Fast local setup

From repo root:

```bash
make build
make serve
make keys-add PROVIDER=openai KEY=sk-...
```

Optional quick checks:

```bash
make health
make test-web-status
```

## Two integration patterns

### 1) Browser handshake flow (recommended for web apps)

Use the custom URL scheme to request a token and runtime proxy base from hostless.

High-level sequence:

1. Your page redirects to `hostless://register?...`
2. User approves local native prompt
3. Browser returns to your callback URL
4. You read token from URL fragment and proxy info from query params

Typical callback shape:

```text
https://your-app.local/callback?port=<p>&local_url=http%3A%2F%2Flocalhost%3A<p>&state=...#token=sk_local_...
```

Store and use:

- `token` (from hash fragment)
- `local_url` or `port` (from query)

Then call:

- `${local_url}/v1/chat/completions`
- `Authorization: Bearer <token>`

## 2) Wrapped local dev server flow (recommended during local development)

Run your app with hostless wrapping so route + token wiring are managed for you.

Example:

```bash
hostless run myapp -- npm run dev
```

or with Make helpers in this repo:

```bash
make test-web-wrapped WEB_PORT=4173 PORT=11434
```

This gives you a route like:

- `http://myapp.localhost:11434`

and injects useful env vars into the child process:

- `HOSTLESS_URL`
- `HOSTLESS_API`
- `HOSTLESS_TOKEN` (when auto-token is enabled)
- `PORT`, `HOST`

## Minimal client request

Use OpenAI-compatible JSON:

```bash
curl -X POST http://localhost:11434/v1/chat/completions \
  -H "Authorization: Bearer sk_local_..." \
  -H "Content-Type: application/json" \
  -d '{"model":"gpt-4o","messages":[{"role":"user","content":"Hello"}]}'
```

## Dev workflow commands

```bash
make test-web-wrapped WEB_PORT=4173 PORT=11434   # auto-start proxy if needed
make test-web-status WEB_PORT=4173 PORT=11434    # health/routes/port/processes
make stop                                         # stop tracked daemon PID
make stop-all                                     # stop all hostless processes
```

## Common issues

### `Hostless daemon is not running`

Start it explicitly:

```bash
make serve
```

or use wrapped target that auto-starts proxy:

```bash
make test-web-wrapped
```

### `Route '<name>.localhost' already exists`

Remove stale route:

```bash
hostless route remove <name>
```

### `OSError: [Errno 48] Address already in use`

Something already listens on your app port. Stop it or use a different port.

### Multiple old hostless processes during dev

```bash
make stop-all
```

## Integration checklist

- Never ship provider API keys in frontend code
- Always send `Authorization: Bearer sk_local_...` to hostless
- Prefer `.localhost` app origins for per-app isolation in browser testing
- Use short TTL and scoped providers/models in development tokens when possible
- Treat bridge tokens as secrets (don’t commit them, don’t log full values)

## Related docs

- `README.md`
- `docs/auth-and-security.md`
- `docs/reverse-proxy.md`
- `docs/process-management.md`
- `docs/cli-commands.md`
- `test-web/README.md`
