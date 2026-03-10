# Proxy API

This document describes the HTTP endpoints exposed by the Hostless daemon on bare localhost.

Canonical implementation sources:

- `src/server/mod.rs` — router setup and public route list
- `src/server/routes.rs` — handler behavior and Hostless-owned request/response shapes
- `src/auth/middleware.rs` — `/v1/*` authentication rules
- `src/server/dispatch.rs` — host-header boundary between `.localhost` apps and bare-localhost management APIs

Related docs:

- `docs/auth-and-security.md`
- `docs/reverse-proxy.md`
- `docs/cli-commands.md`

## Base URL and Routing Boundary

Hostless listens on bare localhost, typically:

- `http://localhost:11434`
- `http://127.0.0.1:11434`

When TLS is enabled, the same endpoints are available on `https://localhost:<port>`.

Important routing boundary:

- Requests sent to bare localhost reach the Hostless management API and LLM proxy endpoints documented here.
- Requests sent to `<name>.localhost:<port>` are reverse-proxied to the local app bound to that subdomain and **cannot** reach `/auth/*`, `/routes/*`, `/health`, or `/v1/*` on the Hostless daemon.

## Authentication Summary

Endpoint groups use different auth rules.

| Group | Paths | Auth requirement |
|---|---|---|
| LLM proxy | `/v1/*` | Bridge token unless `--dev-mode` allows bare localhost / empty-origin requests |
| Interactive registration | `/auth/register` | No admin header required; normally shows user approval dialog |
| Local admin management | `/auth/token`, `/auth/refresh`, `/auth/revoke`, `/auth/tokens`, `/routes*` | `x-hostless-admin: <token>` and localhost-only access |
| Public utility | `/health`, `/callback` | No bridge token |

### `/v1/*` auth details

Outside `--dev-mode`, all `/v1/*` endpoints require `Authorization: Bearer sk_local_...`.

Origin handling:

- browser and webview clients are validated against the request `Origin` header
- native/CLI clients that send no `Origin` still require a valid token unless `--dev-mode` is enabled
- `.localhost` subdomains always require a token, even in `--dev-mode`

### Management auth details

Management endpoints require:

- `x-hostless-admin: <token>` header
- request origin limited to bare localhost or empty origin

The admin token is stored in `~/.hostless/admin.token`.

## Endpoint Index

| Method | Path | Purpose |
|---|---|---|
| `GET` | `/health` | Daemon health and version |
| `GET` | `/callback` | OAuth callback landing page |
| `POST` | `/auth/register` | Interactive bridge-token registration |
| `POST` | `/auth/token` | Direct CLI/admin bridge-token creation |
| `POST` | `/auth/refresh` | Extend an existing token |
| `POST` | `/auth/revoke` | Revoke a token |
| `GET` | `/auth/tokens` | List active tokens without revealing secrets |
| `GET` | `/routes` | List registered `.localhost` routes |
| `POST` | `/routes/register` | Add a `.localhost` route and optionally auto-mint a token |
| `POST` | `/routes/deregister` | Remove a route and revoke its associated auto-token |
| `POST` | `/v1/chat/completions` | OpenAI-compatible chat endpoint with provider routing |
| `POST` | `/v1/responses` | OpenAI-compatible Responses HTTP endpoint |
| `GET` | `/v1/responses` | WebSocket upgrade mode for Responses API |
| `GET` | `/v1/realtime` | WebSocket passthrough for Realtime API |
| `POST` | `/v1/audio/speech` | OpenAI-compatible text-to-speech passthrough |
| `POST` | `/v1/audio/transcriptions` | OpenAI-compatible transcription passthrough |
| `POST` | `/v1/audio/translations` | OpenAI-compatible translation passthrough |
| `POST` | `/v1/images/generations` | OpenAI-compatible image generation passthrough |
| `POST` | `/v1/files` | OpenAI-compatible file upload passthrough |
| `POST` | `/v1/embeddings` | OpenAI-compatible embeddings passthrough |

## Utility Endpoints

Quick curl setup for localhost management endpoints:

```bash
PORT=11434
ADMIN_TOKEN="$(cat ~/.hostless/admin.token)"
```

### `GET /health`

Returns daemon status and version.

Example response:

```json
{
  "status": "ok",
  "version": "0.1.0",
  "service": "hostless"
}
```

Example request:

```bash
curl "http://localhost:${PORT}/health"
```

### `GET /callback`

OAuth callback landing page used by provider login flows.

Query parameters:

- `code` — required on success
- `state` — optional
- `error` — optional error string

Behavior:

- returns `400` if `error` is present
- returns `400` if `code` is missing
- otherwise returns a simple HTML success page

This endpoint is for Hostless provider OAuth flows. It is not the hosted app callback used by `hostless://register` handshakes.

## Auth and Token Endpoints

### `POST /auth/register`

Interactive bridge-token registration for browser and webview clients.

Auth:

- no bridge token required
- normally prompts the user with a native approval dialog
- if the request is already admin-authenticated with `x-hostless-admin`, Hostless treats it as pre-approved and skips the dialog

Request body:

```json
{
  "origin": "http://myapp.localhost:11434",
  "callback": "https://example.com/callback",
  "state": "opaque-state",
  "allowed_providers": ["openai"],
  "allowed_models": ["gpt-4o-mini", "gpt-4o*"],
  "rate_limit": 100
}
```

Fields:

- `origin` — required origin to bind the token to
- `callback` — optional callback URL; if present Hostless responds with a redirect instead of JSON
- `state` — optional opaque value echoed back to the caller
- `allowed_providers` — optional provider scope
- `allowed_models` — optional model scope using exact strings or `*` suffix globs
- `rate_limit` — optional requests-per-hour limit

JSON response when `callback` is omitted:

```json
{
  "port": 11434,
  "local_url": "http://localhost:11434",
  "token": "sk_local_...",
  "state": "opaque-state",
  "expires_in": 3600
}
```

Redirect behavior when `callback` is present:

- status `303 See Other`
- `Location` header set to:

```text
<callback>?port=<port>&local_url=<encoded-url>&state=<encoded-state>&expires_in=3600#token=<encoded-token>
```

Notes:

- successful registration adds the origin to the persistent allowlist
- token TTL is currently fixed at 1 hour for this endpoint

Example request:

```bash
curl -X POST "http://localhost:${PORT}/auth/register" \
  -H "Content-Type: application/json" \
  -d '{
    "origin": "http://myapp.localhost:11434",
    "allowed_providers": ["openai"],
    "allowed_models": ["gpt-4o-mini"]
  }'
```

### `POST /auth/token`

Direct bridge-token creation for CLI and other trusted local automation.

Auth:

- requires `x-hostless-admin: <token>`
- request must be localhost-only
- request must **not** include an `Origin` header

Request body:

```json
{
  "origin": "*",
  "name": "my-cli-tool",
  "allowed_providers": ["openai"],
  "allowed_models": ["gpt-4o-mini"],
  "rate_limit": 100,
  "ttl": 86400
}
```

Fields:

- `origin` — required token binding; use `"*"` to allow any origin including empty origin
- `name` — optional human-readable label
- `allowed_providers` — optional provider scope
- `allowed_models` — optional model scope
- `rate_limit` — optional requests-per-hour limit
- `ttl` — optional token lifetime in seconds, default `86400`

Response:

```json
{
  "token": "sk_local_...",
  "origin": "*",
  "name": "my-cli-tool",
  "allowed_providers": ["openai"],
  "allowed_models": ["gpt-4o-mini"],
  "rate_limit": 100,
  "expires_in": 86400
}
```

Notes:

- this endpoint does not show a user approval dialog
- non-wildcard origins are added to the persistent allowlist

Example request:

```bash
curl -X POST "http://localhost:${PORT}/auth/token" \
  -H "x-hostless-admin: ${ADMIN_TOKEN}" \
  -H "Content-Type: application/json" \
  -d '{
    "origin": "*",
    "name": "my-cli-tool",
    "allowed_providers": ["openai"],
    "allowed_models": ["gpt-4o-mini"],
    "ttl": 86400
  }'
```

### `POST /auth/refresh`

Refreshes an existing token to a new 1-hour TTL.

Auth:

- requires `x-hostless-admin: <token>`

Request body:

```json
{
  "token": "sk_local_..."
}
```

Response:

```json
{
  "status": "refreshed",
  "expires_in": 3600
}
```

If the token is missing or invalid, Hostless returns `401` with an `authentication_error` payload.

Example request:

```bash
curl -X POST "http://localhost:${PORT}/auth/refresh" \
  -H "x-hostless-admin: ${ADMIN_TOKEN}" \
  -H "Content-Type: application/json" \
  -d '{"token":"sk_local_..."}'
```

### `POST /auth/revoke`

Revokes a token immediately.

Auth:

- requires `x-hostless-admin: <token>`

Request body:

```json
{
  "token": "sk_local_..."
}
```

Response:

```json
{
  "status": "revoked"
}
```

Example request:

```bash
curl -X POST "http://localhost:${PORT}/auth/revoke" \
  -H "x-hostless-admin: ${ADMIN_TOKEN}" \
  -H "Content-Type: application/json" \
  -d '{"token":"sk_local_..."}'
```

### `GET /auth/tokens`

Lists active, non-expired tokens without returning the full secret.

Auth:

- requires `x-hostless-admin: <token>`

Response shape:

```json
{
  "tokens": [
    {
      "token_prefix": "sk_local_abc123...",
      "origin": "http://myapp.localhost:11434",
      "app_name": "myapp",
      "expires_in_secs": 3500,
      "allowed_models": ["gpt-4o-mini"],
      "allowed_providers": ["openai"]
    }
  ]
}
```

Example request:

```bash
curl "http://localhost:${PORT}/auth/tokens" \
  -H "x-hostless-admin: ${ADMIN_TOKEN}"
```

## Route Management Endpoints

These endpoints manage `.localhost` reverse-proxy routes.

All route-management endpoints require `x-hostless-admin: <token>` and localhost-only access.

### `GET /routes`

Lists active routes.

Response shape:

```json
{
  "routes": [
    {
      "hostname": "myapp.localhost",
      "target_port": 4173,
      "pid": 12345,
      "app_name": "myapp",
      "url": "http://myapp.localhost:11434"
    }
  ]
}
```

Example request:

```bash
curl "http://localhost:${PORT}/routes" \
  -H "x-hostless-admin: ${ADMIN_TOKEN}"
```

### `POST /routes/register`

Registers a route mapping `<name>.localhost` to a loopback port.

Request body:

```json
{
  "name": "myapp",
  "port": 4173,
  "pid": 12345,
  "auto_token": true,
  "allowed_providers": ["openai"],
  "allowed_models": ["gpt-4o-mini"],
  "rate_limit": 100,
  "ttl": 86400
}
```

Fields:

- `name` — required app name; route becomes `<name>.localhost`
- `port` — required target loopback port
- `pid` — optional process ID used for stale-route cleanup
- `auto_token` — optional, defaults to `true`
- `allowed_providers` — optional scope for the auto-provisioned token
- `allowed_models` — optional model scope for the auto-provisioned token
- `rate_limit` — optional requests-per-hour limit for the auto-token
- `ttl` — optional auto-token lifetime in seconds, default `86400`

Response:

```json
{
  "hostname": "myapp.localhost",
  "url": "http://myapp.localhost:11434",
  "target_port": 4173,
  "pid": 12345,
  "token": {
    "token": "sk_local_...",
    "origin": "http://myapp.localhost:11434",
    "expires_in": 86400
  }
}
```

Notes:

- if `auto_token` is `false`, the `token` field is `null`
- if the hostname already exists and its recorded PID is still alive, Hostless returns `409 Conflict`

Example request:

```bash
curl -X POST "http://localhost:${PORT}/routes/register" \
  -H "x-hostless-admin: ${ADMIN_TOKEN}" \
  -H "Content-Type: application/json" \
  -d '{
    "name": "myapp",
    "port": 4173,
    "auto_token": true,
    "allowed_providers": ["openai"],
    "allowed_models": ["gpt-4o-mini"]
  }'
```

### `POST /routes/deregister`

Removes a route and revokes its associated auto-provisioned token if one exists.

Request body:

```json
{
  "name": "myapp"
}
```

`name` may be either the app name or a full hostname such as `myapp.localhost`.

Success response:

```json
{
  "removed": true,
  "hostname": "myapp.localhost",
  "app_name": "myapp"
}
```

If no route exists, Hostless returns `404`.

Example request:

```bash
curl -X POST "http://localhost:${PORT}/routes/deregister" \
  -H "x-hostless-admin: ${ADMIN_TOKEN}" \
  -H "Content-Type: application/json" \
  -d '{"name":"myapp"}'
```

## LLM Proxy Endpoints

These endpoints expose an OpenAI-compatible surface while routing to configured providers.

Common behavior:

- all `/v1/*` endpoints are behind auth middleware unless `--dev-mode` allows bypass for bare localhost / empty-origin requests
- on auth failures, Hostless returns OpenAI-style error payloads where practical
- upstream provider keys are loaded from Hostless vault storage, never from the client request

### Provider routing

`POST /v1/chat/completions` uses model name routing:

- `claude*` or `anthropic/...` → Anthropic
- `gemini*` or `google/...` → Google
- all other models → OpenAI
- `openai/<model>` explicitly selects OpenAI and strips the prefix before forwarding

`/v1/responses`, `/v1/realtime`, `/v1/embeddings`, and the media endpoints currently support OpenAI-compatible upstreams only.

### `POST /v1/chat/completions`

OpenAI-compatible chat endpoint with provider translation.

Auth:

- requires a bridge token outside `--dev-mode`

Request body:

- OpenAI-compatible `chat/completions` request
- Hostless inspects `model` to choose the provider
- `stream: true` enables streaming passthrough/translation

Behavior:

- validates token provider and model scope before contacting upstream
- transforms requests and responses for Anthropic and Google when needed
- returns upstream/provider errors using a normalized JSON error envelope when possible

### `POST /v1/responses`

OpenAI-compatible HTTP Responses endpoint.

Auth:

- requires a bridge token outside `--dev-mode`

Restrictions:

- OpenAI-compatible models only
- use `/v1/chat/completions` for Anthropic or Google routed requests

Behavior:

- accepts standard OpenAI Responses JSON bodies
- if `stream: true`, returns SSE/event-stream passthrough
- otherwise returns upstream JSON as-is

### `GET /v1/responses`

WebSocket upgrade mode for the Responses API.

Auth:

- requires a bridge token outside `--dev-mode`

Requirements:

- request must be a WebSocket upgrade
- query string must include `?model=<model>` so Hostless can enforce scope before upgrade
- OpenAI-compatible models only

Failure cases:

- `400` if not a websocket upgrade
- `400` if `model` is missing
- `400` if the model does not resolve to OpenAI
- `502` if upstream rejects the websocket upgrade

### `GET /v1/realtime`

OpenAI Realtime websocket passthrough.

Auth:

- requires a bridge token outside `--dev-mode`

Requirements:

- request must be a WebSocket upgrade
- `model` query parameter is optional; defaults to `gpt-4o-realtime-preview`
- OpenAI-compatible models only

Behavior:

- Hostless replaces the client bearer token with the configured upstream provider key
- Hostless adds `openai-beta: realtime=v1`
- upgrade is proxied bidirectionally after pre-upgrade auth and scope checks

### OpenAI-compatible media endpoints

These endpoints currently proxy only to an OpenAI-compatible upstream:

- `POST /v1/audio/speech`
- `POST /v1/audio/transcriptions`
- `POST /v1/audio/translations`
- `POST /v1/images/generations`
- `POST /v1/files`

Behavior:

- request and response bodies are passed through with the upstream provider key injected by Hostless
- provider scope is enforced for all of them
- model scope is enforced for JSON endpoints that carry a `model` field: `/v1/audio/speech` and `/v1/images/generations`
- multipart endpoints currently enforce provider scope only

### `POST /v1/embeddings`

OpenAI-compatible embeddings passthrough.

Auth:

- requires a bridge token outside `--dev-mode`

Restrictions:

- OpenAI-compatible provider only

Behavior:

- forwards the JSON request body to the configured OpenAI-compatible upstream
- returns upstream JSON directly on success

## Error Conventions

Hostless does not use a single error envelope for every route, but common patterns are:

```json
{
  "error": {
    "message": "Human-readable explanation",
    "type": "authentication_error"
  }
}
```

Common `type` values include:

- `authentication_error`
- `token_expired`
- `origin_mismatch`
- `scope_error`
- `configuration_error`
- `invalid_request_error`
- `upstream_error`
- `internal_error`

Some management endpoints return simpler payloads such as:

```json
{
  "error": "User denied access"
}
```

## Notes for Integrators

- Use bare localhost for Hostless APIs. Do not call `/auth/*`, `/routes/*`, or `/v1/*` on a `.localhost` app hostname.
- Use `/auth/register` for interactive browser or webview consent.
- Use `/auth/token` or `hostless token create` only for trusted local admin or bootstrap workflows.
- Keep bridge token origin semantics in mind. A token bound to one origin will not work from another unless it was minted with `"*"`.