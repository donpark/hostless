# test-web

Single test app for validating both hostless use-cases with a dynamic streaming AI chat UI:

1. BYOK handshake flow for web apps (`authmatic://register` -> `sk_local_*` bridge token)
2. Localhost subdomain flow for local dev (`hostless run` -> `<name>.localhost` route isolation)

The app calls `POST /v1/chat/completions` with `stream: true` and renders SSE deltas.

## Quick start

From hostless repo root:

```bash
make build
make serve
make keys-add PROVIDER=openai KEY=sk-...
```

Then choose one flow below.

## Flow A: BYOK handshake flow (web app style)

Run app directly:

```bash
make test-web WEB_PORT=4173
```

Open `http://localhost:4173`, then:

1. Click **Connect with Hostless**
2. Approve native prompt
3. Confirm connection status shows bridge token + proxy base
4. Send a chat prompt and verify streamed assistant output

Expected callback shape:

```text
http://localhost:4173/?port=<p>&local_url=http%3A%2F%2Flocalhost%3A<p>&state=...#token=sk_local_...
```

## Flow B: Localhost subdomain flow (wrapped local dev)

Run wrapped route flow:

```bash
make test-web-wrapped WEB_PORT=4173 PORT=48282
```

Open `http://test-web.localhost:48282`.

The route is proxied through hostless and the page diagnostics should show `wrapped-localhost` mode.
You can still use **Connect with Hostless** to mint a browser token for chat requests.

## Useful commands

```bash
make test-web-status WEB_PORT=4173 PORT=48282
make stop
make stop-all
```

`make test-web-wrapped` automatically:
- starts hostless on `PORT` when needed
- removes stale `test-web` route entries
- frees conflicting listeners on `WEB_PORT`

## URL scheme troubleshooting

If `authmatic://register` has no handler:

```bash
make app-build
make app-scheme-register
make scheme-test WEB_PORT=4173
```

Then fully restart browser and retry.

## Stored local data

The page stores:
- `hostless_token`
- `hostless_proxy_base`
- `hostless_state`
