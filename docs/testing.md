# Testing

Related docs:

- `docs/developer.md`
- `docs/auth-and-security.md`
- `docs/reverse-proxy.md`

## Test Suite Overview

```
cargo test --features internal-testing
```

**139 tests total** (0 warnings):

| Binary | Count | Description |
|---|---|---|
| lib (src/lib.rs) | 41 | Unit tests across all modules |
| bin (src/main.rs) | 41 | Same unit tests compiled in binary context |
| auth_integration | 6 | Auth token flow, origin isolation, scope enforcement |
| proxy_integration | 66 | Reverse proxy dispatch, route management, framework flags, API routing coverage |
| openai_e2e | 3 (ignored) | Live API tests requiring `OPENAI_API_KEY` |

### Running Tests

```bash
# Unit tests only (default, no internal exports)
cargo test

# All tests including integration tests
cargo test --features internal-testing

# Only proxy integration tests
cargo test --features internal-testing --test proxy_integration

# Only auth integration tests
cargo test --features internal-testing --test auth_integration

# E2E tests (requires API key)
OPENAI_API_KEY=sk-... cargo test --features internal-testing --test openai_e2e -- --ignored --nocapture
```

Note: default builds run in tool-first mode (library internals hidden), and `src/lib.rs` suppresses dead-code warnings in that mode to keep CI/local output clean. Enable `--features internal-testing` when you need integration tests that import crate internals.

## Test Infrastructure

### Ephemeral State

**Always use `AppState::new_ephemeral(port, dev_mode)` in tests.** This creates an in-memory vault with a random key — no OS keychain access, no disk I/O.

```rust
let state = hostless::server::AppState::new_ephemeral(0, true);
```

`VaultStore::open()` accesses the OS keychain. Each new test binary triggers macOS keychain approval dialogs. **Never use `AppState::new()` or `VaultStore::open()` in tests.**

### Router Testing Pattern

The proxy integration tests use `tower::ServiceExt::oneshot` to send requests directly through the router without binding to a TCP port:

```rust
use tower::ServiceExt;

let (state, router) = create_test_app(11434);
state.route_table.register("myapp", backend_port, None).await.unwrap();

let req = Request::builder()
    .uri("/path")
    .header("host", "myapp.localhost:11434")
    .body(Body::empty())
    .unwrap();

let resp = router.clone().oneshot(req).await.unwrap();
assert_eq!(resp.status(), StatusCode::OK);
```

### Spawning Test Backends

For reverse proxy tests, spin up a minimal axum server on a random port:

```rust
async fn spawn_backend(status: StatusCode, body: &'static str)
    -> (SocketAddr, JoinHandle<()>)
{
    let app = Router::new().fallback(any(move || async move {
        (status, body).into_response()
    }));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let handle = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (addr, handle)
}
```

Variants: `spawn_backend_with_headers()` (extra response headers), `spawn_echo_backend()` (echoes request headers as JSON).

## Proxy Integration Tests (`tests/proxy_integration.rs`)

66 tests ported from portless's test suite plus hostless-specific hardening/compatibility coverage. Organized into sections:

### Dispatch + Reverse Proxy (18 tests)

Ported from `proxy.test.ts`:

| Test | Portless equivalent |
|---|---|
| `test_unknown_hostname_returns_404` | "returns 404 for unknown hostname" |
| `test_404_mentions_registered_routes` | "includes active routes in 404 page" |
| `test_proxy_to_matching_route` | "proxies request to matching route" |
| `test_forwards_request_headers` | "strips port from Host header when proxying" |
| `test_dead_backend_returns_502` | "returns 502 when backend is not running" |
| `test_loop_detection_508_at_max_hops` | "returns 508 when hop count exceeds limit" |
| `test_loop_detection_allows_below_max` | "allows requests below the hop limit" |
| `test_hop_counter_increments` | "increments hop counter when proxying" |
| `test_bare_localhost_reaches_management_api` | Dispatch boundary validation |
| `test_subdomain_cannot_reach_management` | Dispatch firewall test |
| `test_subdomain_cannot_reach_auth` | Dispatch firewall test |
| `test_subdomain_cannot_reach_llm_proxy` | Dispatch firewall test |
| `test_subdomain_cannot_reach_responses_proxy` | Dispatch firewall test |
| `test_no_host_header_falls_through` | No Host → management API |
| `test_127_0_0_1_falls_through` | 127.0.0.1 → management API |
| `test_strips_hop_by_hop_from_response` | "strips hop-by-hop headers from proxied responses" |
| `test_websocket_upgrade_detected` | WS upgrade detected in oneshot router tests (returns 502) |
| `test_websocket_proxy_roundtrip_echo` | Full websocket upgrade + message echo through reverse proxy |
| `test_responses_proxy_to_openai_compatible_upstream` | `/v1/responses` non-stream proxying |
| `test_responses_stream_passthrough_preserves_events` | `/v1/responses` stream passthrough keeps SSE event names |
| `test_responses_rejects_non_openai_provider_models` | `/v1/responses` guardrail for non-OpenAI provider prefixes |
| `test_realtime_websocket_proxy_roundtrip_with_token` | `/v1/realtime` websocket proxy success path in strict auth mode |
| `test_realtime_websocket_rejects_model_scope_violation` | `/v1/realtime` rejects model scope mismatch before upgrade |

### Route Management API (12 tests)

Ported from `routes.test.ts`:

| Test | Portless equivalent |
|---|---|
| `test_register_route_api` | "addRoute" |
| `test_register_route_no_token` | auto_token=false variant |
| `test_deregister_route_api` | "removeRoute" |
| `test_deregister_unknown_route_returns_404` | "removeRoute non-existent" |
| `test_list_routes_api` | "loadRoutes" |
| `test_list_routes_empty` | Empty state variant |
| `test_register_route_with_provider_scope` | Scoped token provisioning |
| `test_deregister_revokes_token` | Token revocation on deregister |
| `test_replace_stale_route` | "replaces existing route if PID dead" |
| `test_cannot_replace_alive_route` | "does not replace route if PID alive" |
| `test_set_token_on_route` | Token association |
| `test_cleanup_preserves_no_pid_routes` | "preserves routes without PID during cleanup" |
| `test_cleanup_returns_removed_tokens` | Token cleanup verification |
| `test_route_url_includes_port` | "port in links" |
| `test_multiple_routes_listed` | Multi-route list |
| `test_remove_by_app_name` | Remove by name vs hostname |

### Framework Flag Injection (15 tests)

Ported from `cli-utils.test.ts`:

| Test | Framework |
|---|---|
| `test_inject_astro` | astro → `--port --host` |
| `test_inject_react_router` | react-router → `--port --host` |
| `test_inject_ng` | Angular CLI → `--port --host` |
| `test_inject_nuxt` | nuxt → `--port --host` |
| `test_inject_remix` | remix → `--port --host` |
| `test_no_inject_if_port_present` | `--port` already present |
| `test_no_inject_if_short_port_present` | `-p` already present |
| `test_no_inject_node` | node → no injection |
| `test_no_inject_python` | python → no injection |
| `test_inject_npm_vite` | npm + vite → `-- --port --host` |
| `test_inject_pnpm_vite` | pnpm + vite → `-- --port --host` |
| `test_inject_yarn_astro` | yarn + astro → `-- --port --host` |
| `test_inject_empty_command` | empty → no-op |

### Environment & Port (6 tests)

Ported from `cli-utils.test.ts`:

| Test | What it verifies |
|---|---|
| `test_child_env_has_all_vars` | All expected env vars set |
| `test_child_env_no_token` | HOSTLESS_TOKEN absent when no token |
| `test_child_env_inherits_system_env` | PATH inherited |
| `test_find_available_port_is_bindable` | Returned port can be bound |
| `test_find_available_port_varies` | Random allocation produces different ports |
| `test_hostname_edge_cases` | Hostname parsing edge cases |

## Portless Tests NOT Ported

The following portless test categories were intentionally skipped:

| Category | Reason |
|---|---|
| TLS/HTTPS proxy tests (~10) | TLS not yet implemented in hostless |
| HTTP/2 tests (~3) | HTTP/2 not yet supported |
| E2E tests (11 files) | Require full CLI + filesystem + process spawning; covered by unit + integration |
| XSS escaping in 404 page | Hostless returns plain text 404, not HTML |
| Stale lock recovery (filesystem locking) | Hostless uses `fs2` file locking, not directory-based locks |

## Auth Integration Tests (`tests/auth_integration.rs`)

6 tests exercising the token system:

| Test | What it verifies |
|---|---|
| `test_curl_token_workflow` | Wildcard tokens work for CLI (empty origin) |
| `test_provider_scoped_token` | Provider scope blocks wrong providers |
| `test_model_scoped_token` | Model scope with glob patterns |
| `test_origin_bound_token_rejects_other_origin` | Origin binding enforcement |
| `test_localhost_subdomain_isolation` | Two apps get isolated tokens |
| `test_localhost_evil_com_not_bare_localhost` | `localhost.evil.com` rejected |

## Adding New Tests

### For reverse proxy / dispatch behavior

Add to `tests/proxy_integration.rs`. Use the `create_test_app()` + `send_request()` helpers. Spawn a backend if you need a live upstream.

### For route table logic

Add `#[tokio::test]` to the `mod tests` block in `src/server/route_table.rs`, or add to the Route Management section of `proxy_integration.rs` for API-level tests.

### For framework flag injection

Add `#[test]` to `src/process/manager.rs` `mod tests`, or to the Framework section of `proxy_integration.rs`.

### For auth/token behavior

Add to `tests/auth_integration.rs`.

### Adding a New HTTP Route

1. Add handler function in `src/server/routes.rs`
2. Register in `create_router()` in `src/server/mod.rs` (under `api_routes` if it needs auth middleware, `public_routes` if not)

### Working with the Route Table in Tests

```rust
let state = hostless::server::AppState::new_ephemeral(11434, true);
state.route_table.register("myapp", 4001, None).await.unwrap();
let route = state.route_table.lookup("myapp.localhost").await.unwrap();
assert_eq!(route.target_port, 4001);
```
