# Mixed-Provider CLI Quickstart (OpenAI + Anthropic + Gemini)

This is a concrete, end-to-end setup for using `hostless` with multiple OpenAI API-compatible models and a mix of OpenAI, Anthropic, and Gemini.

## 1) Start hostless

```bash
hostless serve --port 11434
```

Optional (local dev bypass for bare `localhost` / no-origin requests):

```bash
hostless serve --port 11434 --dev-mode
```

## 2) Add provider API keys

```bash
hostless keys add openai "$OPENAI_API_KEY"
hostless keys add anthropic "$ANTHROPIC_API_KEY"
hostless keys add google "$GEMINI_API_KEY"
```

Verify:

```bash
hostless keys list
```

## 3) Create a scoped bridge token (recommended)

Allow one token to call all 3 providers with selected model patterns:

```bash
hostless token create \
  --name "my-mixed-app" \
  --origin "http://myapp.localhost:4173" \
  --providers "openai,anthropic,google" \
  --models "gpt-4o*,claude-3-*,gemini-2.5-*" \
  --ttl 86400
```

Save the returned `sk_local_...` token as `HOSTLESS_TOKEN`.

## 4) Call different providers by changing only `model`

All calls still go to the same OpenAI-compatible endpoint:

```bash
POST http://localhost:11434/v1/chat/completions
```

### OpenAI

```bash
curl -s http://localhost:11434/v1/chat/completions \
  -H "Authorization: Bearer $HOSTLESS_TOKEN" \
  -H "Content-Type: application/json" \
  -d '{
    "model": "gpt-4o-mini",
    "messages": [{"role":"user","content":"Say hi in 1 sentence."}]
  }'
```

### Anthropic (prefix form)

```bash
curl -s http://localhost:11434/v1/chat/completions \
  -H "Authorization: Bearer $HOSTLESS_TOKEN" \
  -H "Content-Type: application/json" \
  -d '{
    "model": "anthropic/claude-3-5-sonnet-latest",
    "messages": [{"role":"user","content":"Say hi in 1 sentence."}]
  }'
```

### Gemini (prefix form)

```bash
curl -s http://localhost:11434/v1/chat/completions \
  -H "Authorization: Bearer $HOSTLESS_TOKEN" \
  -H "Content-Type: application/json" \
  -d '{
    "model": "google/gemini-2.5-flash",
    "messages": [{"role":"user","content":"Say hi in 1 sentence."}]
  }'
```

## 5) Wrap a local app and auto-provision scoped token

If your app runs on `.localhost`, `hostless run` can register route + token automatically:

```bash
hostless run myapp \
  --providers "openai,anthropic,google" \
  --models "gpt-4o*,claude-3-*,gemini-2.5-*" \
  -- npm run dev
```

Your app is reachable at:

```text
http://myapp.localhost:11434
```

## 6) Useful checks

```bash
curl -s http://localhost:11434/health
hostless token list
hostless route list
```

## 7) Streaming examples (`stream: true`)

Use `-N` with `curl` so chunks print as they arrive:

### OpenAI streaming

```bash
curl -N http://localhost:11434/v1/chat/completions \
  -H "Authorization: Bearer $HOSTLESS_TOKEN" \
  -H "Content-Type: application/json" \
  -d '{
    "model": "gpt-4o-mini",
    "stream": true,
    "messages": [{"role":"user","content":"Count to 5."}]
  }'
```

### Anthropic streaming

```bash
curl -N http://localhost:11434/v1/chat/completions \
  -H "Authorization: Bearer $HOSTLESS_TOKEN" \
  -H "Content-Type: application/json" \
  -d '{
    "model": "anthropic/claude-3-5-sonnet-latest",
    "stream": true,
    "messages": [{"role":"user","content":"Count to 5."}]
  }'
```

### Gemini streaming

```bash
curl -N http://localhost:11434/v1/chat/completions \
  -H "Authorization: Bearer $HOSTLESS_TOKEN" \
  -H "Content-Type: application/json" \
  -d '{
    "model": "google/gemini-2.5-flash",
    "stream": true,
    "messages": [{"role":"user","content":"Count to 5."}]
  }'
```

Streams use OpenAI-compatible SSE chunks and terminate with `data: [DONE]`.

## Notes on model routing behavior

- `anthropic/...` or `claude...` routes to Anthropic.
- `google/...` or `gemini...` routes to Google Gemini.
- `openai/...` and everything else defaults to OpenAI-compatible.
- Request/response shape remains OpenAI-compatible for client apps.