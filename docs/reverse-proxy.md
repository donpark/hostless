# Reverse Proxy & Host-Header Dispatch

Hostless integrates a reverse proxy (inspired by Vercel's [portless](https://github.com/vercel-labs/portless)) that gives each locally-running app its own `.localhost` subdomain. This solves a critical security problem: without per-app subdomains, all apps on `localhost` share the same origin and can reuse each other's bearer tokens.

## Architecture

```
                         ┌─────────────────────────────────────────────────┐
                         │                 hostless (:11434)               │
                         │                                                 │
  Browser                │  ┌──────────────────────────────┐               │
  myapp.localhost:11434 ─┼─►│  Host-Header Dispatch        │               │
                         │  │  (dispatch.rs)               │               │
                         │  │  *.localhost → reverse proxy  │               │
                         │  │  localhost   → fall-through   │               │
                         │  └──────┬───────────┬───────────┘               │
                         │         │           │                           │
                         │    subdomain     bare localhost                 │
                         │         │           │                           │
                         │  ┌──────▼──────┐  ┌─▼──────────────────────┐   │
                         │  │ Reverse     │  │ Management + LLM Proxy │   │
                         │  │ Proxy       │  │ /health, /auth/*,      │   │
                         │  │ (→ app port)│  │ /v1/*, /routes/*       │   │
                         │  └──────┬──────┘  └────────────────────────┘   │
                         │         │                                       │
                         └─────────┼───────────────────────────────────────┘
                                   │
                            ┌──────▼──────┐
                            │ Local App   │
                            │ :4001       │
                            └─────────────┘
```

## Host-Header Dispatch (structural security boundary)

**File**: `src/server/dispatch.rs`

The outermost middleware layer on the Axum router. It inspects the `Host` header and makes a binary routing decision:

| Host header | Dispatch target |
|---|---|
| `<name>.localhost` or `<name>.localhost:<port>` | **Reverse proxy** → app's target port |
| `localhost`, `127.0.0.1`, or missing | **Fall-through** → Axum router (management API + LLM proxy) |

### Security Guarantee

Requests to `.localhost` subdomains **structurally cannot reach**:
- `/auth/*` — token creation/revocation
- `/v1/*` — LLM proxy endpoints
- `/routes/*` — route management
- `/health` — health check

This is not a filter or access-control check — the dispatch layer routes subdomain traffic to a completely different code path. A compromised app running at `evil.localhost:11434` cannot craft requests to the management plane.

### Key Functions

- `extract_hostname(req)` — Extracts hostname from `Host` header, strips port
- `is_subdomain_host(hostname)` — Returns `true` for `*.localhost` but not bare `localhost` or `.localhost`
- `host_dispatch(state, req, next)` — The axum middleware entry point

## Reverse Proxy

**File**: `src/server/reverse_proxy.rs`

Forwards HTTP requests to `127.0.0.1:<target_port>` and pipes the response back.

### Features

- **X-Forwarded headers**: Sets `X-Forwarded-Host`, `X-Forwarded-Proto` (`http`), `X-Forwarded-Port`
- **Hop-by-hop header stripping**: Removes `Connection`, `Keep-Alive`, `Proxy-Authenticate`, `Proxy-Authorization`, `TE`, `Trailers`, `Transfer-Encoding`, `Upgrade` (RFC 2616 §13.5.1)
- **Loop detection**: Tracks `X-Hostless-Hops` header. Returns `508 Loop Detected` when hops ≥ `MAX_HOPS` (5). Increments hop counter on each proxy pass.
- **Error handling**: Returns branded HTML pages for browser-facing `404`/`502`/`508` proxy errors.

### WebSocket Support

WebSocket upgrade detection is implemented but the actual proxying is **stubbed** (returns `501 Not Implemented`). The handler:
1. Detects `Upgrade: websocket` header
2. Connects to upstream via TCP
3. Sends the upgrade request
4. Parses the 101 response
5. Returns 501 — full bidirectional pipe requires hyper's `on_upgrade` API (tracked as future work)

### Constants

| Constant | Value | Purpose |
|---|---|---|
| `MAX_HOPS` | 5 | Loop detection threshold |
| `HOP_BY_HOP_HEADERS` | 8 entries | Headers stripped from proxied requests/responses |

## Route Table

**File**: `src/server/route_table.rs`

Maps `.localhost` hostnames to local app ports. Stored in-memory (`HashMap<String, AppRoute>` behind `RwLock`) with file-backed persistence to `~/.hostless/routes.json`.

### AppRoute Fields

| Field | Type | Description |
|---|---|---|
| `hostname` | `String` | e.g., `"myapp.localhost"` |
| `target_port` | `u16` | Local port the app listens on |
| `pid` | `Option<u32>` | Process ID (if managed by `hostless run`) |
| `app_name` | `String` | Human-readable name |
| `registered_at` | `Instant` | Registration timestamp |
| `token` | `Option<String>` | Auto-provisioned bridge token |

### Operations

| Method | Description |
|---|---|
| `register(name, port, pid)` | Add route. Rejects if hostname taken with alive PID. Replaces if PID is dead. |
| `lookup(hostname)` | Find route by full hostname |
| `lookup_with_wildcard(hostname, enabled)` | Exact match first; optional wildcard fallback |
| `remove(name)` | Remove by app name or full hostname |
| `list()` | All active routes as `Vec<RouteInfo>` |
| `set_token(hostname, token)` | Associate a bridge token with a route |
| `cleanup_stale()` | Remove routes whose PIDs are dead. Returns removed count + revoked tokens. |
| `load_from_disk()` | Restore from `~/.hostless/routes.json` on startup (filters dead PIDs) |

### Persistence

Routes are persisted to `~/.hostless/routes.json` on every mutation (register/remove/cleanup). Uses `fs2` file locking for safe concurrent access. On startup, `load_from_disk()` restores routes but filters out entries with dead PIDs.

### Wildcard Routing (gated)

When `HOSTLESS_ENABLE_WILDCARD_ROUTES=1` is set at server startup, dispatch may resolve unmatched subdomains by suffix:

- Registered: `myapp.localhost`
- Request: `tenant.myapp.localhost`
- Result: forwards to `myapp.localhost` target

Exact hostname matches always take priority over wildcard fallback.

### Stale Route Cleanup

PID liveness is checked via `kill(pid, 0)` (signal 0 — checks permission without sending a signal). A background tokio task runs every 300s to clean up stale routes and revoke their associated tokens.

## HTTP Endpoints

| Method | Path | Purpose |
|---|---|---|
| POST | `/routes/register` | Register a route (localhost-only, auto-provisions bridge token) |
| POST | `/routes/deregister` | Remove a route and revoke its token |
| GET | `/routes` | List active routes |

### POST /routes/register

Request body:
```json
{
  "name": "myapp",
  "port": 4001,
  "pid": 12345,
  "auto_token": true,
  "allowed_providers": ["openai"],
  "allowed_models": ["gpt-4o*"],
  "rate_limit": 100,
  "ttl": 86400
}
```

Response:
```json
{
  "hostname": "myapp.localhost",
  "url": "http://myapp.localhost:11434",
  "target_port": 4001,
  "pid": 12345,
  "token": {
    "token": "sk_local_...",
    "origin": "http://myapp.localhost:11434",
    "expires_in": 86400
  }
}
```

The auto-provisioned token is **origin-scoped** to `http://<name>.localhost:<server_port>`, meaning it can only be used by requests originating from that specific subdomain.

### POST /routes/deregister

Request: `{ "name": "myapp" }` — accepts app name or full hostname.

Removes the route AND revokes the associated bridge token.

## Dependencies

| Crate | Version | Purpose |
|---|---|---|
| `hyper` | 1.x | Low-level HTTP client for reverse proxy |
| `hyper-util` | 0.1 | `Client` builder with `TokioExecutor` |
| `http-body-util` | 0.1 | Body utilities for hyper ↔ axum conversion |
| `nix` | 0.29 | `kill(pid, 0)` for PID liveness checks |
| `fs2` | 0.4 | File locking for concurrent route persistence |
