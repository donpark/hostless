# Process Management & Daemon Mode

Hostless can wrap child processes (dev servers, scripts, etc.) and manage their lifecycle, including automatic route registration, port assignment, and token provisioning.

Related docs:

- `docs/cli-commands.md`
- `docs/reverse-proxy.md`
- `docs/auth-and-security.md`

## Overview

```
$ hostless run myapp -- npm run dev

  1. Find available port (4000-4999)
  2. Register route: myapp.localhost → 127.0.0.1:4XXX
  3. Auto-provision bridge token scoped to http://myapp.localhost:11434
  4. Inject PORT, HOST, HOSTLESS_TOKEN, HOSTLESS_URL env vars
  5. Inject framework flags (--port, --host for vite/next/etc.)
  6. Spawn child directly when possible (shell fallback only for shell syntax)
  7. Wait for child exit
  8. Deregister route + revoke token
```

## Process Manager

**File**: `src/process/manager.rs`

### SpawnConfig

| Field | Type | Default | Description |
|---|---|---|---|
| `name` | `String` | optional | App name (becomes `<name>.localhost`) |
| `command` | `String` | required | Command to execute (direct spawn preferred; shell fallback for shell syntax) |
| `port` | `Option<u16>` | random 4000-4999 | Override target port (`--app-port` / `HOSTLESS_APP_PORT`) |
| `daemon_port` | `u16` | 11434 | Hostless server port |
| `auto_token` | `bool` | true | Whether to provision a bridge token |
| `allowed_providers` | `Option<Vec<String>>` | None (all) | Provider scope for token |
| `allowed_models` | `Option<Vec<String>>` | None (all) | Model scope for token |
| `rate_limit` | `Option<u64>` | None | Requests per hour |
| `ttl` | `u64` | 86400 | Token TTL in seconds |

### Port Allocation

`find_available_port()` tries random ports in `4000-4999` (up to 100 attempts). Falls back to OS-assigned port via `TcpListener::bind("127.0.0.1:0")`.

### Framework Flag Injection

`inject_framework_flags(command, port)` detects the framework from the command and appends appropriate flags:

| Framework | Flags appended |
|---|---|
| `vite` | `--port <port> --host 127.0.0.1` |
| `astro` | `--port <port> --host 127.0.0.1` |
| `react-router` | `--port <port> --host 127.0.0.1` |
| `ng` (Angular) | `--port <port> --host 127.0.0.1` |
| `nuxt` | `--port <port> --host 127.0.0.1` |
| `remix` | `--port <port> --host 127.0.0.1` |
| `next` | `-p <port> -H 127.0.0.1` |
| `react-native` | `--port <port> --host 127.0.0.1` |
| `expo` | `--port <port> --host localhost` |
| `npm`/`yarn`/`pnpm` + vite/astro/nuxt in command | `-- --port <port> --host 127.0.0.1` |
| Unknown (node, python, etc.) | No injection |

For wrapper commands containing `expo`, host injection uses `localhost`.

**Skip conditions**: If the command already contains `--port` or `-p `, no flags are injected.

### Environment Variables

`build_child_env(port, token, daemon_port, app_name)` builds the child process environment by inheriting all current env vars and adding:

| Variable | Value | Description |
|---|---|---|
| `PORT` | `<port>` | The assigned port |
| `HOST` | `127.0.0.1` | Bind address |
| `HOSTLESS_TOKEN` | `sk_local_...` | Bridge token (if auto_token=true) |
| `HOSTLESS_URL` | `http://<name>.localhost:<daemon_port>` | The app's public URL |
| `HOSTLESS_API` | `http://localhost:<daemon_port>` | Hostless management API |
| `__VITE_ADDITIONAL_SERVER_ALLOWED_HOSTS` | `.localhost` | Allows Vite to serve `.localhost` subdomains |

Also prepends `node_modules/.bin` to `PATH` if it exists in CWD.

### Command Execution Safety

`spawn_and_manage()` executes commands in two modes:

- **Direct mode (preferred)**: parses command words and spawns without a shell.
- **Shell fallback**: if shell operators are detected (`|`, `&`, `;`, `<`, `>`, `` ` ``, `$`, parentheses, newline), runs via `/bin/sh -c`.

This preserves advanced shell workflows while reducing accidental shell-injection surface for ordinary commands.

### Daemon Communication

| Function | Purpose |
|---|---|
| `register_with_daemon(config, port, pid)` | POST `/routes/register` to running daemon |
| `deregister_with_daemon(daemon_port, name)` | POST `/routes/deregister` (non-fatal on failure) |
| `is_daemon_running(port)` | GET `/health` with 2s timeout |
| `read_daemon_port()` | Read `~/.hostless/hostless.port` |
| `read_daemon_pid()` | Read `~/.hostless/hostless.pid` |
| `write_daemon_port(port)` / `write_daemon_pid(pid)` | Write daemon state files |
| `cleanup_daemon_files()` | Remove PID and port files |

Daemon PID/port file reads and writes use file locking (`fs2`) to avoid concurrent state corruption.

## Daemon Mode

**File**: `src/main.rs` (Serve command)

`hostless serve --daemon` backgrounds the server process:

1. Spawns a detached daemon process via `start_daemon_process_with_options(...)`
2. Writes PID to `~/.hostless/hostless.pid`
3. Writes port to `~/.hostless/hostless.port`
4. On foreground exit (`hostless stop`), a `DaemonCleanupGuard` removes these files

### Stop Command

`hostless stop`:
1. Reads PID from `~/.hostless/hostless.pid`
2. Sends `SIGTERM` via `nix::sys::signal::kill()`
3. Waits up to 5 seconds for process to exit (polls every 200ms with signal 0)
4. Cleans up PID/port files

### `HOSTLESS=0` Bypass

When `HOSTLESS=0` is set in the environment, `hostless run` executes the command directly without any wrapping. This allows nested invocations to opt out.

## Name Inference & Worktrees

`hostless run` supports optional name automation:

- `--infer-name`: infer from `package.json` name, then git root directory, then current directory.
- `--name <name>`: explicit name override.
- `--worktree-prefix`: prepend inferred/explicit name with current git worktree branch segment (non-main/master).

## File Layout on Disk

| Path | Content |
|---|---|
| `~/.hostless/hostless.pid` | Daemon process ID (text) |
| `~/.hostless/hostless.port` | Daemon listen port (text) |
| `~/.hostless/routes.json` | Persisted route table (JSON array of `PersistedRoute`) |

## Dependencies

| Crate | Version | Purpose |
|---|---|---|
| `nix` | 0.29 | Signal sending (`SIGTERM`), PID liveness checks |
| `fs2` | 0.4 | File locking for daemon state files |
| `shell-words` | 1.x | Safe command tokenization for direct command spawn |
