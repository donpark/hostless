# Hostless

<img width="128" height="128" alt="authmatic-128" src="https://github.com/user-attachments/assets/97039365-cbd1-41b0-bc54-f5b3f4568355" />

Hostless is a local AI proxy and local reverse proxy.

- Forward proxy plane: OpenAI-compatible API on `localhost` that injects locally stored provider keys.
- Reverse proxy plane: per-app `.localhost` subdomains for local-origin isolation.

## What It Solves

- Keeps provider API keys out of browser/app runtime.
- Provides origin-scoped bridge tokens (`sk_local_*`) instead of raw provider keys.
- Lets local apps run under unique origins like `myapp.localhost:48282`.

## What It Implements

- Full localhost API reference: `docs/proxy-api.md`
- OpenAI-compatible localhost proxy surface for chat, responses, realtime, embeddings, and media APIs
- Supports these Ollama API endpoints on bare localhost: `GET /api/tags`, `POST /api/show`, `GET /api/ps`
- `.localhost` reverse proxy routing for local apps with per-app origin isolation

## Status

Mostly feature complete. Needs more integration tests, especially all the APIs endpoints.

## License

Apache 2.0

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
curl -X POST http://localhost:48282/v1/chat/completions \
  -H "Authorization: Bearer sk_local_..." \
  -H "Content-Type: application/json" \
  -d '{"model":"gpt-4o-mini","messages":[{"role":"user","content":"hello"}]}'

# OpenAI Responses API (OpenAI-compatible models)
curl -X POST http://localhost:48282/v1/responses \
  -H "Authorization: Bearer sk_local_..." \
  -H "Content-Type: application/json" \
  -d '{"model":"gpt-4o-mini","input":"hello"}'
```

Responses WebSocket mode through hostless:

```javascript
// npm i ws
import WebSocket from "ws";

const token = process.env.HOSTLESS_TOKEN; // sk_local_...
const ws = new WebSocket("ws://localhost:48282/v1/responses?model=gpt-4o-mini", {
  headers: {
    Authorization: `Bearer ${token}`,
  },
});

let lastResponseId = null;

ws.on("open", () => {
  ws.send(
    JSON.stringify({
      type: "response.create",
      model: "gpt-4o-mini",
      store: false,
      input: [
        {
          type: "message",
          role: "user",
          content: [{ type: "input_text", text: "Find fizz_buzz()" }],
        },
      ],
      tools: [],
    }),
  );
});

ws.on("message", (raw) => {
  const event = JSON.parse(raw.toString());
  if (event.type === "response.completed" && event.response?.id) {
    lastResponseId = event.response.id;

    // Chain the next turn with only incremental input.
    ws.send(
      JSON.stringify({
        type: "response.create",
        model: "gpt-4o-mini",
        store: false,
        previous_response_id: lastResponseId,
        input: [
          {
            type: "message",
            role: "user",
            content: [{ type: "input_text", text: "Now optimize it." }],
          },
        ],
        tools: [],
      }),
    );
  }
});
```

```python
# pip install websocket-client
import json
import os
from websocket import create_connection

token = os.environ["HOSTLESS_TOKEN"]  # sk_local_...
ws = create_connection(
    "ws://localhost:48282/v1/responses?model=gpt-4o-mini",
    header=[f"Authorization: Bearer {token}"],
)

# First turn
ws.send(
    json.dumps(
        {
            "type": "response.create",
            "model": "gpt-4o-mini",
            "store": False,
            "input": [
                {
                    "type": "message",
                    "role": "user",
                    "content": [{"type": "input_text", "text": "Find fizz_buzz()"}],
                }
            ],
            "tools": [],
        }
    )
)

previous_response_id = None
while True:
    event = json.loads(ws.recv())
    if event.get("type") == "response.completed":
        previous_response_id = event.get("response", {}).get("id")
        break

# Chained second turn
ws.send(
    json.dumps(
        {
            "type": "response.create",
            "model": "gpt-4o-mini",
            "store": False,
            "previous_response_id": previous_response_id,
            "input": [
                {
                    "type": "message",
                    "role": "user",
                    "content": [{"type": "input_text", "text": "Now optimize it."}],
                }
            ],
            "tools": [],
        }
    )
)
```

WebSocket mode troubleshooting:

- `401` / authentication errors: Ensure `Authorization: Bearer sk_local_...` is set on the websocket handshake.
- `403` / scope errors: Token provider/model scope may block the request. Reissue a token with `openai` provider access and compatible model scope.
- `400 previous_response_not_found`: If using `store=false`, a reconnect may lose continuation state. Start a new chain or resend full context.
- `502` from hostless: Upstream websocket upgrade was rejected or unreachable. Verify OpenAI key, base URL overrides, and network egress.
- Long-lived sessions: Reconnect before/at the upstream connection lifetime limit and continue with `previous_response_id` when available.

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
- Detailed endpoint coverage and request contracts live in `docs/proxy-api.md`.
- Ollama support is limited to the endpoint subset above; Hostless does not claim full Ollama runtime compatibility.
- SSE streaming is supported; `/v1/responses` stream events are passed through without event-name rewriting.
- WebSocket upgrade pass-through is supported for reverse-proxied local apps and `/v1/realtime` on the local API plane.
- Full endpoint compatibility matrix (M1/M2/M3) is documented in `docs/auth-and-security.md`.

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

- `docs/proxy-api.md`: canonical HTTP endpoint reference for `/health`, `/auth/*`, `/routes/*`, `/v1/*`, and supported Ollama API endpoints
- `docs/cli-commands.md`: canonical CLI reference
- `docs/auth-and-security.md`: token model, auth middleware, provider routing, key storage
- `docs/reverse-proxy.md`: host-header dispatch, reverse proxy internals, route table
- `docs/process-management.md`: `hostless run`, framework flag injection, daemon lifecycle
- `docs/testing.md`: test suite structure and testing patterns
- `AGENTS.md`: architecture map for maintainers and coding agents

## License

Apache 2.0

