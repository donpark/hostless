# CLI Commands

This reference reflects the command surface implemented in `src/main.rs`.

Related docs:

- `docs/process-management.md`
- `docs/auth-and-security.md`
- `docs/reverse-proxy.md`

## Top-level commands

```bash
hostless serve [--port 11434] [--tls] [--verbose] [--dev-mode] [--daemon] [--token-persistence <off|file|keychain>]
hostless proxy <start|stop> ...
hostless run [<name>] [--infer-name] [--name <name>] [--worktree-prefix] [--app-port <p>] [--daemon-port <p>] [--providers <csv>] [--models <csv>] [--rate-limit <n>] [--ttl <seconds>] [--no-token] -- <command...>
hostless <name> <command...>
hostless stop
hostless list [--daemon-port <p>]
hostless route <list|add|remove> ...
hostless alias <list|add|remove> ...
hostless alias <name> <port> [--daemon-port <p>]
hostless alias --remove <name> [--daemon-port <p>]
hostless hosts <sync|clean>
hostless trust
hostless keys <add|list|remove|migrate> ...
hostless origins <add|list|remove> ...
hostless config <list|set-token-persistence> ...
hostless auth login <provider>
hostless token <create|list|revoke> ...
```

## serve

```bash
hostless serve [--port 11434] [--tls] [--verbose] [--dev-mode] [--daemon] [--token-persistence <off|file|keychain>]
```

| Flag | Description |
|---|---|
| `--port` | Listen port (default: `11434`) |
| `--tls` | Enable TLS with auto-generated local certs |
| `--verbose` | Enable verbose logging |
| `--dev-mode` | Allow unauthenticated bare localhost/no-origin requests |
| `--daemon` | Run server in background and persist PID/port metadata |
| `--token-persistence` | Bridge token storage mode: `off` (default), `file` (plaintext), `keychain` (encrypted) |

## run (portless-clone)

```bash
hostless run [<name>] [options] -- <command...>
hostless <name> <command...>
```

| Option | Description |
|---|---|
| `<name>` | App name used as `<name>.localhost` (optional with `--infer-name`) |
| `--infer-name` | Infer app name from package.json, git root, or directory name |
| `--name <name>` | Explicit name override |
| `--worktree-prefix` | Prefix app name with current git worktree branch segment |
| `--app-port <port>` | Override assigned app port (`--port` remains accepted as alias) |
| `--daemon-port <port>` | Hostless daemon port (default: `11434`) |
| `--providers <csv>` | Restrict token to providers (`openai,anthropic,google`) |
| `--models <csv>` | Restrict token to model globs |
| `--rate-limit <n>` | Requests/hour limit for auto-token |
| `--ttl <seconds>` | Token TTL (default: `86400`) |
| `--no-token` | Skip auto-token provisioning |

Environment:

| Variable | Description |
|---|---|
| `HOSTLESS_APP_PORT` | Default app port override for `hostless run` |
| `HOSTLESS_ENABLE_WILDCARD_ROUTES` | Enable wildcard subdomain routing (`tenant.app.localhost` -> `app.localhost`) |

The top-level shorthand `hostless <name> <command...>` is portless-compatible and maps to `hostless run <name> -- <command...>`.

## stop

```bash
hostless stop
```

Stops the daemon process tracked in `~/.hostless/hostless.pid`.

## proxy (portless-compatible)

```bash
hostless proxy start [--port 11434] [--https] [--verbose] [--dev-mode] [--foreground] [--token-persistence <off|file|keychain>]
hostless proxy stop
```

- `proxy start` defaults to daemon/background mode unless `--foreground` is used.
- `--https` maps to hostless TLS mode.
- `--token-persistence` uses the same token storage policy as `hostless serve`.

## list (portless-compatible)

```bash
hostless list [--daemon-port <port>]
```

Equivalent to `hostless route list`.

## route

```bash
hostless route list
hostless route add <name> --port <port> [--daemon-port 11434]
hostless route remove <name> [--daemon-port 11434]
```

Use these commands to manage `.localhost` route mappings without process wrapping.

## alias

```bash
hostless alias list [--daemon-port 11434]
hostless alias add <name> <port> [--daemon-port 11434]
hostless alias remove <name> [--daemon-port 11434]
hostless alias <name> <port> [--daemon-port 11434]
hostless alias --remove <name> [--daemon-port 11434]
```

Static aliases are loopback-only route registrations (`127.0.0.1:<port>`) and do not auto-provision bridge tokens.
The positional and `--remove` forms are portless-compatible shorthands.

## hosts

```bash
hostless hosts sync
hostless hosts clean
```

`hosts sync` writes current persisted route hostnames into a managed `/etc/hosts` block.
`hosts clean` removes only the managed hostless block. Run these with sufficient permissions (typically via `sudo`).

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

## config

```bash
hostless config list
hostless config set-token-persistence <off|file|keychain>
```

- `list`: Show current persisted defaults from `~/.hostless/config.json`
- `set-token-persistence`: Set default token storage policy used by `serve`/`proxy start` when `--token-persistence` is not provided

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
