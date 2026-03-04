# CLI Commands

This reference reflects the command surface implemented in `src/main.rs`.

## Top-level commands

```bash
hostless serve [--port 11434] [--tls] [--verbose] [--dev-mode] [--daemon]
hostless run <name> [--port <p>] [--daemon-port <p>] [--providers <csv>] [--models <csv>] [--rate-limit <n>] [--ttl <seconds>] [--no-token] -- <command...>
hostless stop
hostless route <list|add|remove> ...
hostless trust
hostless keys <add|list|remove|migrate> ...
hostless origins <add|list|remove> ...
hostless auth login <provider>
hostless token <create|list|revoke> ...
```

## serve

```bash
hostless serve [--port 11434] [--tls] [--verbose] [--dev-mode] [--daemon]
```

| Flag | Description |
|---|---|
| `--port` | Listen port (default: `11434`) |
| `--tls` | Enable TLS with auto-generated local certs |
| `--verbose` | Enable verbose logging |
| `--dev-mode` | Allow unauthenticated bare localhost/no-origin requests |
| `--daemon` | Run server in background and persist PID/port metadata |

## run (portless-clone)

```bash
hostless run <name> [options] -- <command...>
```

| Option | Description |
|---|---|
| `<name>` | App name used as `<name>.localhost` |
| `--port <port>` | Override assigned app port |
| `--daemon-port <port>` | Hostless daemon port (default: `11434`) |
| `--providers <csv>` | Restrict token to providers (`openai,anthropic,google`) |
| `--models <csv>` | Restrict token to model globs |
| `--rate-limit <n>` | Requests/hour limit for auto-token |
| `--ttl <seconds>` | Token TTL (default: `86400`) |
| `--no-token` | Skip auto-token provisioning |

## stop

```bash
hostless stop
```

Stops the daemon process tracked in `~/.hostless/hostless.pid`.

## route

```bash
hostless route list
hostless route add <name> --port <port> [--daemon-port 11434]
hostless route remove <name> [--daemon-port 11434]
```

Use these commands to manage `.localhost` route mappings without process wrapping.

## trust

```bash
hostless trust
```

Trusts the local hostless certificate authority in the system trust store.

## keys

```bash
hostless keys add <provider> <api_key> [--base-url <url>]
hostless keys list
hostless keys remove <provider>
hostless keys migrate
```

- `add`: Store provider credentials (`openai`, `anthropic`, `google`, `openrouter`, or custom)
- `list`: List configured providers
- `remove`: Delete provider credentials
- `migrate`: Best-effort migrate legacy key storage

## origins

```bash
hostless origins add <origin>
hostless origins list
hostless origins remove <origin>
```

Manage explicitly allowed web origins.

## auth

```bash
hostless auth login <provider>
```

Start OAuth login flow for a provider (currently `openrouter`).

## token (token-swapping)

```bash
hostless token create [--name <name>] [--origin <origin|*>] [--providers <csv>] [--models <csv>] [--rate-limit <n>] [--ttl <seconds>]
hostless token list
hostless token revoke <token-or-prefix>
```

- `create`: Create bridge tokens for CLI/apps with optional origin/provider/model/rate/ttl scopes
- `list`: Show active bridge tokens
- `revoke`: Revoke by full token or prefix

## Make targets (dev workflow)

The project Makefile provides common local workflows around the CLI:

```bash
make build                                  # cargo build + codesign + copy ./hostless
make release                                # cargo build --release + codesign + copy ./hostless
make serve [PORT=11434]                     # run proxy in foreground
make stop                                   # stop daemon tracked by ~/.hostless/hostless.pid
make stop-all                               # stop all running hostless processes
make test-web                               # run plain python test web server
make test-web-wrapped WEB_PORT=4173 PORT=11434
make test-web-status WEB_PORT=4173 PORT=11434
```

Notes:
- `make test-web-wrapped` starts hostless on `PORT` if `/health` is unavailable, clears stale `test-web` routes, and frees `WEB_PORT` listeners before wrapping.
- `make test-web-status` reports daemon health, active routes, listener state on `WEB_PORT`, and running hostless processes.
