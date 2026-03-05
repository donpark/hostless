# Developer Guide

This guide is for developers working on hostless itself.

Related docs:

- `docs/cli-commands.md`
- `docs/auth-and-security.md`
- `docs/testing.md`

## Scope

This repo documents hostless daemon/proxy behavior, CLI operations, and server-side contracts.
App-side implementation details are intentionally out of scope here.

## Local Setup

From repo root:

```bash
make clean
make build
make serve
```

Add at least one provider key for proxy testing:

```bash
make keys-add PROVIDER=openai KEY=sk-...
```

Verify daemon health:

```bash
make health
```

## Core Development Workflows

### CLI and daemon

```bash
hostless serve --daemon
hostless stop
hostless route list
hostless token list
```

### Process wrapping and route lifecycle

```bash
hostless run myapp -- npm run dev
hostless route list
hostless route remove myapp
```

### Config and token persistence

```bash
hostless config list
hostless config set-token-persistence file
```

## Testing

Fast paths:

```bash
cargo test
cargo test --features internal-testing
cargo test --features internal-testing --test proxy_integration
```

E2E (ignored by default):

```bash
OPENAI_API_KEY=sk-... cargo test --features internal-testing --test openai_e2e -- --ignored --nocapture
```

Critical rule: use ephemeral test state (`AppState::new_ephemeral`, `VaultStore::open_ephemeral`) to avoid keychain and disk side effects.

## Troubleshooting

### `Missing or invalid management authentication`

The daemon and CLI may be out of sync on `~/.hostless/admin.token`.

```bash
hostless stop
hostless serve --daemon
```

### Stale route entries

```bash
hostless route list
hostless route remove <name>
```

### Port conflicts

Use a different app port (`--app-port`) or daemon port (`--port` / `--daemon-port`).

## Related Docs

- `docs/cli-commands.md`
- `docs/auth-and-security.md`
- `docs/reverse-proxy.md`
- `docs/process-management.md`
- `docs/testing.md`
