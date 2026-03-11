use anyhow::{Context, Result};
use clap::{Parser, Subcommand, ValueEnum};
#[cfg(target_os = "linux")]
use std::path::Path;
use std::net::SocketAddr;
use std::time::{Duration, Instant};
use tracing::info;

mod auth;
mod config;
mod hosts;
mod process;
mod providers;
mod server;
mod tls;
mod vault;

#[derive(Clone, Copy, Debug, ValueEnum)]
enum TokenPersistenceArg {
    Off,
    File,
    Keychain,
}

impl TokenPersistenceArg {
    fn as_cli_value(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::File => "file",
            Self::Keychain => "keychain",
        }
    }
}

impl From<TokenPersistenceArg> for config::TokenPersistenceMode {
    fn from(value: TokenPersistenceArg) -> Self {
        match value {
            TokenPersistenceArg::Off => config::TokenPersistenceMode::Off,
            TokenPersistenceArg::File => config::TokenPersistenceMode::File,
            TokenPersistenceArg::Keychain => config::TokenPersistenceMode::Keychain,
        }
    }
}

#[derive(Parser)]
#[command(name = "hostless", version, about = "Local AI proxy that manages LLM API keys securely")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Start the proxy server
    Serve {
        /// Port to listen on
        #[arg(short, long, default_value = "48282")]
        port: u16,

        /// Enable TLS with auto-generated local certificates
        #[arg(long)]
        tls: bool,

        /// Verbose logging
        #[arg(short, long)]
        verbose: bool,

        /// Dev mode: allow unauthenticated access from bare localhost/127.0.0.1
        /// and requests with no Origin header (CLI/curl). Without this flag,
        /// ALL clients must present a valid bridge token.
        #[arg(long)]
        dev_mode: bool,

        /// Run as a background daemon
        #[arg(long)]
        daemon: bool,

        /// Bridge token persistence mode (default from config, fallback: off)
        #[arg(long, value_enum)]
        token_persistence: Option<TokenPersistenceArg>,

        /// Internal flag used by daemon launcher to avoid re-daemonizing.
        #[arg(long, hide = true)]
        daemonized: bool,
    },

    /// Run a command through the hostless proxy (assigns <name>.localhost subdomain)
    Run {
        /// App name (becomes <name>.localhost)
        /// If omitted, use --infer-name or --name.
        name: Option<String>,

        /// Infer app name from package.json, git root, or current directory
        #[arg(long)]
        infer_name: bool,

        /// Optional explicit app name override (useful with reserved words)
        #[arg(long = "name")]
        name_override: Option<String>,

        /// Prefix inferred/explicit app name with git worktree branch segment
        #[arg(long)]
        worktree_prefix: bool,

        /// Command to run (use -- to separate from hostless args)
        #[arg(trailing_var_arg = true, num_args = 1..)]
        command: Vec<String>,

        /// Override the port assigned to the app
        #[arg(long = "app-port", alias = "port")]
        app_port: Option<u16>,

        /// Hostless daemon port (default: auto-detect or 48282)
        #[arg(long)]
        daemon_port: Option<u16>,

        /// Restrict to specific providers (comma-separated: openai,anthropic,google)
        #[arg(long, value_delimiter = ',')]
        providers: Option<Vec<String>>,

        /// Restrict to specific models (glob patterns, comma-separated)
        #[arg(long, value_delimiter = ',')]
        models: Option<Vec<String>>,

        /// Rate limit in requests per hour
        #[arg(long)]
        rate_limit: Option<u64>,

        /// Token TTL in seconds (default: 86400 = 24h)
        #[arg(long, default_value = "86400")]
        ttl: u64,

        /// Skip auto-token provisioning
        #[arg(long)]
        no_token: bool,
    },

    /// Stop the hostless daemon
    Stop,

    /// Portless-compatible proxy controls
    Proxy {
        #[command(subcommand)]
        action: ProxyAction,
    },

    /// Portless-compatible route listing alias
    List {
        /// Hostless daemon port override
        #[arg(long)]
        daemon_port: Option<u16>,
    },

    /// Manage app routes
    Route {
        #[command(subcommand)]
        action: RouteAction,
    },

    /// Manage static loopback aliases (for services not spawned by hostless)
    Alias {
        #[command(subcommand)]
        action: Option<AliasAction>,

        /// Portless-compatible remove form: hostless alias --remove <name>
        #[arg(long)]
        remove: Option<String>,

        /// Portless-compatible add form: hostless alias <name> <port>
        name: Option<String>,

        /// Portless-compatible add form: hostless alias <name> <port>
        port: Option<u16>,

        /// Hostless daemon port (default: 48282)
        #[arg(long, default_value = "48282")]
        daemon_port: u16,
    },

    /// Trust the hostless CA certificate in the system store
    Trust,

    /// Manage /etc/hosts entries for current hostless routes
    Hosts {
        #[command(subcommand)]
        action: HostsAction,
    },

    /// Manage API keys
    Keys {
        #[command(subcommand)]
        action: KeysAction,
    },

    /// Manage allowed origins
    Origins {
        #[command(subcommand)]
        action: OriginsAction,
    },

    /// Manage hostless configuration values
    Config {
        #[command(subcommand)]
        action: ConfigAction,
    },

    /// Start OAuth login with a provider
    Auth {
        #[command(subcommand)]
        action: AuthAction,
    },

    /// Manage bridge tokens for apps and CLI clients
    Token {
        #[command(subcommand)]
        action: TokenAction,
    },

    /// Portless-compatible shorthand: hostless <name> <command...>
    #[command(external_subcommand)]
    External(Vec<String>),

}

#[derive(Subcommand)]
enum ProxyAction {
    /// Start the proxy daemon (portless-compatible)
    Start {
        /// Port to listen on
        #[arg(short, long, default_value = "48282")]
        port: u16,

        /// Enable TLS with auto-generated local certificates
        #[arg(long = "https")]
        https: bool,

        /// Verbose logging
        #[arg(short, long)]
        verbose: bool,

        /// Dev mode: allow unauthenticated bare localhost/no-origin requests
        #[arg(long)]
        dev_mode: bool,

        /// Run in foreground (default is daemon/background)
        #[arg(long)]
        foreground: bool,

        /// Bridge token persistence mode (default from config, fallback: off)
        #[arg(long, value_enum)]
        token_persistence: Option<TokenPersistenceArg>,
    },
    /// Stop the proxy daemon (portless-compatible)
    Stop,
}

#[derive(Subcommand)]
enum KeysAction {
    /// Add an API key for a provider
    Add {
        /// Provider name (openai, anthropic, google, openrouter, or custom)
        provider: String,
        /// The API key
        api_key: String,
        /// Optional custom base URL for the provider
        #[arg(long)]
        base_url: Option<String>,
    },
    /// List stored providers
    List,
    /// Remove a provider's API key
    Remove {
        /// Provider name
        provider: String,
    },
    /// Migrate legacy encrypted keys.vault into plaintext keys.env (best effort)
    Migrate,
}

#[derive(Subcommand)]
enum OriginsAction {
    /// List allowed origins
    List,
    /// Remove an allowed origin
    Remove {
        /// Origin URL (e.g., https://myapp.com)
        origin: String,
    },
    /// Add an allowed origin manually
    Add {
        /// Origin URL (e.g., https://myapp.com)
        origin: String,
    },
}

#[derive(Subcommand)]
enum RouteAction {
    /// List active routes
    List,
    /// Add a route manually (register an app without process wrapping)
    Add {
        /// App name (becomes <name>.localhost)
        name: String,
        /// Target port on 127.0.0.1
        #[arg(long)]
        port: u16,
        /// Hostless daemon port (default: 48282)
        #[arg(long, default_value = "48282")]
        daemon_port: u16,
    },
    /// Remove a route
    Remove {
        /// App name or hostname (e.g., "myapp" or "myapp.localhost")
        name: String,
        /// Hostless daemon port (default: 48282)
        #[arg(long, default_value = "48282")]
        daemon_port: u16,
    },
}

#[derive(Subcommand)]
enum AliasAction {
    /// List static aliases (same output as route list)
    List {
        /// Hostless daemon port (default: 48282)
        #[arg(long, default_value = "48282")]
        daemon_port: u16,
    },
    /// Add a static alias to a loopback port
    Add {
        /// Alias name (becomes <name>.localhost)
        name: String,
        /// Target local port on 127.0.0.1
        port: u16,
        /// Hostless daemon port (default: 48282)
        #[arg(long, default_value = "48282")]
        daemon_port: u16,
    },
    /// Remove a static alias
    Remove {
        /// Alias name or hostname
        name: String,
        /// Hostless daemon port (default: 48282)
        #[arg(long, default_value = "48282")]
        daemon_port: u16,
    },
}

#[derive(Subcommand)]
enum HostsAction {
    /// Sync managed hostless entries into /etc/hosts
    Sync,
    /// Remove managed hostless entries from /etc/hosts
    Clean,
}

#[derive(Subcommand)]
enum AuthAction {
    /// Start OAuth login with a provider
    Login {
        /// Provider name (openrouter)
        provider: String,
    },
}

#[derive(Subcommand)]
enum ConfigAction {
    /// Show current configuration
    List,
    /// Set bridge token persistence policy in config.json
    SetTokenPersistence {
        /// Persistence mode used by default when serve/proxy start omits --token-persistence
        #[arg(value_enum)]
        mode: TokenPersistenceArg,
    },
}

#[derive(Subcommand)]
enum TokenAction {
    /// Create a new bridge token for CLI or app use
    Create {
        /// Human-readable app name (e.g., "my-cli-tool")
        #[arg(long)]
        name: Option<String>,

        /// Origin to bind this token to (e.g., "http://myapp.localhost:1355").
        /// Use "*" for CLI tokens that don't send an Origin header.
        #[arg(long, default_value = "*")]
        origin: String,

        /// Restrict to specific providers (comma-separated: openai,anthropic,google)
        #[arg(long, value_delimiter = ',')]
        providers: Option<Vec<String>>,

        /// Restrict to specific models (glob patterns, comma-separated: gpt-4o*,claude-3-haiku*)
        #[arg(long, value_delimiter = ',')]
        models: Option<Vec<String>>,

        /// Rate limit in requests per hour
        #[arg(long)]
        rate_limit: Option<u64>,

        /// Token time-to-live in seconds (default: 86400 = 24 hours)
        #[arg(long, default_value = "86400")]
        ttl: u64,
    },
    /// List all active tokens
    List,
    /// Revoke a token by prefix
    Revoke {
        /// Token string or prefix (e.g., "sk_local_abc...")
        token: String,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Commands::Serve {
            port,
            tls,
            verbose,
            dev_mode,
            daemon,
            token_persistence,
            daemonized,
        } => {
            init_tracing(verbose);
            if dev_mode {
                info!("⚠️  Dev mode enabled: bare localhost and no-origin requests bypass auth");
            }

            // Write daemon state files
            process::manager::write_daemon_port(port).ok();
            process::manager::write_daemon_pid(std::process::id()).ok();

            if daemon && !daemonized {
                // Daemon mode: spawn a detached child process and return.
                // Avoid forking from inside the Tokio runtime.
                println!("Starting hostless daemon on port {}...", port);
                process::manager::start_daemon_process_with_options(
                    port,
                    tls,
                    dev_mode,
                    verbose,
                    token_persistence.map(|v| v.as_cli_value()),
                )?;
                return Ok(());
            } else {
                // Foreground mode (existing behavior)
                let app_state = server::AppState::new(
                    port,
                    dev_mode,
                    token_persistence.map(Into::into),
                )
                .await?;
                let addr = SocketAddr::from(([127, 0, 0, 1], port));

                // Clean up daemon files on exit
                let _cleanup_guard = DaemonCleanupGuard;

                if tls {
                    info!("Starting Hostless proxy with TLS on https://localhost:{}", port);
                    tls::serve_tls(app_state, addr).await?;
                } else {
                    info!("Starting Hostless proxy on http://localhost:{}", port);
                    let app = server::create_router(app_state);
                    let listener = tokio::net::TcpListener::bind(addr).await?;
                    info!("Listening on {}", addr);
                    axum::serve(listener, app)
                        .with_graceful_shutdown(shutdown_signal())
                        .await?;
                }
            }
        }

        Commands::Run {
            name,
            infer_name,
            name_override,
            worktree_prefix,
            command,
            app_port,
            daemon_port,
            providers,
            models,
            rate_limit,
            ttl,
            no_token,
        } => {
            run_wrapped_command(
                name,
                infer_name,
                name_override,
                worktree_prefix,
                command,
                app_port,
                daemon_port,
                providers,
                models,
                rate_limit,
                ttl,
                no_token,
            )
            .await?;
        }

        Commands::Stop => {
            // Read PID from file and send SIGTERM
            match process::manager::read_daemon_pid() {
                Some(pid) => {
                    use nix::sys::signal::{self, Signal};
                    use nix::unistd::Pid;

                    println!("Stopping hostless daemon (PID {})...", pid);
                    match signal::kill(Pid::from_raw(pid as i32), Signal::SIGTERM) {
                        Ok(_) => {
                            // Wait up to 5s for process to exit
                            for _ in 0..50 {
                                std::thread::sleep(std::time::Duration::from_millis(100));
                                if signal::kill(Pid::from_raw(pid as i32), None).is_err() {
                                    break;
                                }
                            }
                            process::manager::cleanup_daemon_files();
                            println!("✓ Daemon stopped");
                        }
                        Err(nix::errno::Errno::ESRCH) => {
                            process::manager::cleanup_daemon_files();
                            println!("Daemon was not running (stale PID file cleaned up)");
                        }
                        Err(e) => {
                            anyhow::bail!("Failed to stop daemon: {}", e);
                        }
                    }
                }
                None => {
                    println!("No daemon PID file found. Is the daemon running?");
                }
            }
        }

        Commands::Proxy { action } => match action {
            ProxyAction::Start {
                port,
                https,
                verbose,
                dev_mode,
                foreground,
                token_persistence,
            } => {
                // Portless semantics default to daemon mode unless --foreground is set.
                handle_serve(
                    port,
                    https,
                    verbose,
                    dev_mode,
                    !foreground,
                    false,
                    token_persistence.map(Into::into),
                )
                .await?;
            }
            ProxyAction::Stop => {
                handle_stop()?;
            }
        },

        Commands::List { daemon_port } => {
            let effective_port = daemon_port
                .or_else(process::manager::read_daemon_port)
                .unwrap_or(48282);
            print_routes(effective_port).await?;
        }

        Commands::Route { action } => {
            handle_route(action).await?;
        }

        Commands::Alias {
            action,
            remove,
            name,
            port,
            daemon_port,
        } => {
            if let Some(action) = action {
                handle_alias(action).await?;
            } else if let Some(name) = remove {
                handle_alias(AliasAction::Remove { name, daemon_port }).await?;
            } else if let (Some(name), Some(port)) = (name, port) {
                handle_alias(AliasAction::Add {
                    name,
                    port,
                    daemon_port,
                })
                .await?;
            } else {
                anyhow::bail!(
                    "Invalid alias usage. Use one of:\n  hostless alias list\n  hostless alias add <name> <port>\n  hostless alias remove <name>\n  hostless alias <name> <port>\n  hostless alias --remove <name>"
                );
            }
        }

        Commands::Trust => {
            handle_trust()?;
        }

        Commands::Hosts { action } => {
            handle_hosts(action)?;
        }

        Commands::Keys { action } => {
            init_tracing(false);
            handle_keys(action).await?;
        }

        Commands::Origins { action } => {
            init_tracing(false);
            handle_origins(action).await?;
        }

        Commands::Config { action } => {
            init_tracing(false);
            handle_config(action)?;
        }

        Commands::Auth { action } => {
            init_tracing(false);
            handle_auth(action).await?;
        }

        Commands::Token { action } => {
            init_tracing(false);
            handle_token(action).await?;
        }

        Commands::External(args) => {
            if args.len() < 2 {
                anyhow::bail!(
                    "Unknown command '{}'.\nFor app shorthand use: hostless <name> <command...>",
                    args.first().cloned().unwrap_or_default()
                );
            }
            let name = Some(args[0].clone());
            let command = args[1..].to_vec();
            run_wrapped_command(
                name,
                false,
                None,
                false,
                command,
                None,
                None,
                None,
                None,
                None,
                86400,
                false,
            )
            .await?;
        }

    }

    Ok(())
}

async fn run_wrapped_command(
    name: Option<String>,
    infer_name: bool,
    name_override: Option<String>,
    worktree_prefix: bool,
    command: Vec<String>,
    app_port: Option<u16>,
    daemon_port: Option<u16>,
    providers: Option<Vec<String>>,
    models: Option<Vec<String>>,
    rate_limit: Option<u64>,
    ttl: u64,
    no_token: bool,
) -> Result<()> {
    init_tracing(false);

    if command.is_empty() {
        anyhow::bail!("No command provided. Usage: hostless run <name> -- <command>");
    }

    // Check HOSTLESS=0 bypass
    if std::env::var("HOSTLESS").map(|v| v == "0").unwrap_or(false) {
        println!("HOSTLESS=0 set, running command directly without proxy");
        let cmd_str = command.join(" ");
        let status = std::process::Command::new("/bin/sh")
            .arg("-c")
            .arg(&cmd_str)
            .status()
            .context("Failed to run command")?;
        std::process::exit(status.code().unwrap_or(1));
    }

    let mut effective_name = if let Some(name) = name_override {
        name
    } else if let Some(name) = name {
        name
    } else if infer_name {
        process::manager::infer_project_name(None)?
    } else {
        anyhow::bail!(
            "No app name provided. Use 'hostless run <name> -- <cmd>' or '--infer-name'."
        );
    };

    effective_name = process::manager::sanitize_for_hostname(&effective_name);
    if effective_name.is_empty() {
        anyhow::bail!("App name is invalid after hostname sanitization");
    }

    if worktree_prefix {
        if let Some(prefix) = process::manager::detect_worktree_prefix(None) {
            effective_name = format!("{}-{}", prefix, effective_name);
        }
    }

    // Determine daemon port (explicit --daemon-port wins; else auto-detect from file; else default)
    let effective_daemon_port = daemon_port
        .or_else(process::manager::read_daemon_port)
        .unwrap_or(48282);

    ensure_daemon_ready_for_run(&effective_name, effective_daemon_port).await?;

    let cmd_str = command.join(" ");

    let env_app_port = match std::env::var("HOSTLESS_APP_PORT") {
        Ok(raw) => {
            let parsed = raw.parse::<u16>().with_context(|| {
                format!("Invalid HOSTLESS_APP_PORT value '{}': expected 1-65535", raw)
            })?;
            Some(parsed)
        }
        Err(_) => None,
    };

    let effective_app_port = app_port.or(env_app_port);

    info!(
        event = "launch-wrapped-app",
        app = effective_name.as_str(),
        daemon_port = effective_daemon_port,
        "Launching wrapped app"
    );

    let config = process::manager::SpawnConfig {
        name: effective_name,
        command: cmd_str,
        port: effective_app_port,
        daemon_port: effective_daemon_port,
        auto_token: !no_token,
        allowed_providers: providers,
        allowed_models: models,
        rate_limit,
        ttl,
    };

    let exit_code = process::manager::spawn_and_manage(config).await?;
    std::process::exit(exit_code);
}

async fn handle_serve(
    port: u16,
    tls: bool,
    verbose: bool,
    dev_mode: bool,
    daemon: bool,
    daemonized: bool,
    token_persistence: Option<config::TokenPersistenceMode>,
) -> Result<()> {
    init_tracing(verbose);
    if dev_mode {
        info!("⚠️  Dev mode enabled: bare localhost and no-origin requests bypass auth");
    }

    process::manager::write_daemon_port(port).ok();
    process::manager::write_daemon_pid(std::process::id()).ok();

    if daemon && !daemonized {
        println!("Starting hostless daemon on port {}...", port);
        process::manager::start_daemon_process_with_options(
            port,
            tls,
            dev_mode,
            verbose,
            token_persistence.map(|m| m.as_str()),
        )?;
        return Ok(());
    }

    let app_state = server::AppState::new(port, dev_mode, token_persistence).await?;
    let addr = SocketAddr::from(([127, 0, 0, 1], port));

    let _cleanup_guard = DaemonCleanupGuard;

    if tls {
        info!("Starting Hostless proxy with TLS on https://localhost:{}", port);
        tls::serve_tls(app_state, addr).await?;
    } else {
        info!("Starting Hostless proxy on http://localhost:{}", port);
        let app = server::create_router(app_state);
        let listener = tokio::net::TcpListener::bind(addr).await?;
        info!("Listening on {}", addr);
        axum::serve(listener, app)
            .with_graceful_shutdown(shutdown_signal())
            .await?;
    }

    Ok(())
}

fn handle_stop() -> Result<()> {
    match process::manager::read_daemon_pid() {
        Some(pid) => {
            use nix::sys::signal::{self, Signal};
            use nix::unistd::Pid;

            println!("Stopping hostless daemon (PID {})...", pid);
            match signal::kill(Pid::from_raw(pid as i32), Signal::SIGTERM) {
                Ok(_) => {
                    for _ in 0..50 {
                        std::thread::sleep(std::time::Duration::from_millis(100));
                        if signal::kill(Pid::from_raw(pid as i32), None).is_err() {
                            break;
                        }
                    }
                    process::manager::cleanup_daemon_files();
                    println!("✓ Daemon stopped");
                }
                Err(nix::errno::Errno::ESRCH) => {
                    process::manager::cleanup_daemon_files();
                    println!("Daemon was not running (stale PID file cleaned up)");
                }
                Err(e) => {
                    anyhow::bail!("Failed to stop daemon: {}", e);
                }
            }
        }
        None => {
            println!("No daemon PID file found. Is the daemon running?");
        }
    }

    Ok(())
}

async fn print_routes(daemon_port: u16) -> Result<()> {
    let client = reqwest::Client::new();
    let admin_token = auth::admin::load_admin_token()?;
    let resp = client
        .get(format!("http://localhost:{}/routes", daemon_port))
        .header(auth::admin::ADMIN_HEADER, &admin_token)
        .send()
        .await;

    match resp {
        Ok(r) if r.status().is_success() => {
            let data: serde_json::Value = r.json().await?;
            let routes = data.get("routes").and_then(|r| r.as_array());
            match routes {
                Some(routes) if !routes.is_empty() => {
                    println!("Active routes:");
                    for route in routes {
                        let name = route.get("app_name").and_then(|v| v.as_str()).unwrap_or("?");
                        let hostname = route.get("hostname").and_then(|v| v.as_str()).unwrap_or("?");
                        let port = route.get("target_port").and_then(|v| v.as_u64()).unwrap_or(0);
                        let pid = route.get("pid").and_then(|v| v.as_u64());
                        let url = route.get("url").and_then(|v| v.as_str()).unwrap_or("?");

                        println!("  • {} → 127.0.0.1:{}", hostname, port);
                        println!("    Name: {}", name);
                        println!("    URL:  {}", url);
                        if let Some(pid) = pid {
                            println!("    PID:  {}", pid);
                        }
                    }
                }
                _ => println!("No active routes."),
            }
        }
        _ => anyhow::bail!("Hostless daemon is not running. Start it with: hostless serve"),
    }

    Ok(())
}

async fn ensure_daemon_ready_for_run(app_name: &str, daemon_port: u16) -> Result<()> {
    if process::manager::is_daemon_running(daemon_port).await {
        info!(
            event = "ready",
            app = app_name,
            daemon_port = daemon_port,
            source = "already-running",
            "Hostless daemon is ready"
        );
        return Ok(());
    }

    info!(
        event = "detected-not-running",
        app = app_name,
        daemon_port = daemon_port,
        "Hostless daemon not detected"
    );

    let _start_lock = process::manager::acquire_daemon_start_lock()?;

    if process::manager::is_daemon_running(daemon_port).await {
        info!(
            event = "ready",
            app = app_name,
            daemon_port = daemon_port,
            source = "already-running-after-lock",
            "Hostless daemon is ready"
        );
        return Ok(());
    }

    info!(
        event = "starting-daemon",
        app = app_name,
        daemon_port = daemon_port,
        "Starting hostless daemon"
    );

    let start_error = process::manager::start_daemon_process(daemon_port)
        .err()
        .map(|e| e.to_string());

    let max_wait = Duration::from_secs(15);
    let begin = Instant::now();
    let mut attempt: u32 = 0;
    let mut delay = Duration::from_millis(100);

    while begin.elapsed() <= max_wait {
        attempt += 1;
        if process::manager::is_daemon_running(daemon_port).await {
            info!(
                event = "ready",
                app = app_name,
                daemon_port = daemon_port,
                attempt = attempt,
                elapsed_ms = begin.elapsed().as_millis() as u64,
                source = "auto-start",
                "Hostless daemon is ready"
            );
            return Ok(());
        }

        tokio::time::sleep(delay).await;
        delay = std::cmp::min(delay * 2, Duration::from_secs(1));
    }

    let startup_cause = start_error
        .map(|e| format!(" Startup error: {}", e))
        .unwrap_or_default();

    info!(
        event = "startup-failed",
        app = app_name,
        daemon_port = daemon_port,
        elapsed_ms = begin.elapsed().as_millis() as u64,
        "Hostless daemon failed to become ready"
    );

    anyhow::bail!(
        "Hostless daemon failed to start on port {} within 15s. \
Start it manually with:\n  hostless serve --daemon --port {}{}",
        daemon_port,
        daemon_port,
        startup_cause
    );
}

async fn handle_keys(action: KeysAction) -> Result<()> {
    let vault = vault::VaultStore::open().await?;

    match action {
        KeysAction::Add {
            provider,
            api_key,
            base_url,
        } => {
            vault.add_key(&provider, &api_key, base_url.as_deref()).await?;
            println!("✓ Stored API key for '{}'", provider);
        }
        KeysAction::List => {
            let providers = vault.list_providers().await?;
            if providers.is_empty() {
                println!("No API keys stored. Use 'hostless keys add <provider> <key>' to add one.");
            } else {
                println!("Stored providers:");
                for p in providers {
                    println!(
                        "  • {}{}",
                        p.name,
                        p.base_url
                            .map(|u| format!(" ({})", u))
                            .unwrap_or_default()
                    );
                }
            }
        }
        KeysAction::Remove { provider } => {
            vault.remove_key(&provider).await?;
            println!("✓ Removed API key for '{}'", provider);
        }
        KeysAction::Migrate => {
            let migrated = vault.migrate_legacy_json_vault().await?;
            if migrated == 0 {
                println!("No legacy keys migrated.");
            } else {
                println!("✓ Migrated {} key(s) from keys.vault to keys.env", migrated);
            }
        }
    }

    Ok(())
}

async fn handle_origins(action: OriginsAction) -> Result<()> {
    let mut cfg = config::AppConfig::load()?;

    match action {
        OriginsAction::List => {
            if cfg.allowed_origins.is_empty() {
                println!("No allowed origins. Origins are added when webapps complete the handshake.");
            } else {
                println!("Allowed origins:");
                for origin in &cfg.allowed_origins {
                    println!("  • {}", origin);
                }
            }
        }
        OriginsAction::Remove { origin } => {
            cfg.allowed_origins.retain(|o| o != &origin);
            cfg.save()?;
            println!("✓ Removed origin '{}'", origin);
        }
        OriginsAction::Add { origin } => {
            if !cfg.allowed_origins.contains(&origin) {
                cfg.allowed_origins.push(origin.clone());
                cfg.save()?;
            }
            println!("✓ Added origin '{}'", origin);
        }
    }

    Ok(())
}

async fn handle_auth(action: AuthAction) -> Result<()> {
    match action {
        AuthAction::Login { provider } => {
            auth::oauth::start_oauth_login(&provider).await?;
        }
    }

    Ok(())
}

fn handle_config(action: ConfigAction) -> Result<()> {
    let mut cfg = config::AppConfig::load()?;

    match action {
        ConfigAction::List => {
            println!("Current configuration:");
            println!("  token_persistence: {}", cfg.token_persistence.as_str());
        }
        ConfigAction::SetTokenPersistence { mode } => {
            cfg.token_persistence = mode.into();
            cfg.save()?;
            println!(
                "✓ Set token persistence default to '{}'",
                cfg.token_persistence.as_str()
            );
            println!(
                "  This applies to future 'hostless serve' and 'hostless proxy start' runs unless overridden with --token-persistence."
            );
        }
    }

    Ok(())
}

fn active_daemon_port() -> u16 {
    process::manager::read_daemon_port().unwrap_or(48282)
}

async fn handle_token(action: TokenAction) -> Result<()> {
    let admin_token = auth::admin::load_admin_token()?;
    let daemon_port = active_daemon_port();
    let proxy_url = format!("http://localhost:{}", daemon_port);

    match action {
        TokenAction::Create {
            name,
            origin,
            providers,
            models,
            rate_limit,
            ttl,
        } => {
            // We need a running server to issue tokens, so call the server's API
            // For CLI-created tokens, we talk to the running proxy
            let client = reqwest::Client::new();

            // Check if server is running
            match client.get(format!("{}/health", proxy_url)).send().await {
                Ok(r) if r.status().is_success() => {}
                _ => {
                    anyhow::bail!(
                        "Hostless proxy is not running. Start it with: hostless serve\n\
                         Tokens can only be created while the server is running."
                    );
                }
            }

            // Use the dedicated CLI token endpoint (no dialog needed)
            let body = serde_json::json!({
                "origin": origin,
                "name": name,
                "allowed_providers": providers,
                "allowed_models": models,
                "rate_limit": rate_limit,
                "ttl": ttl,
            });

            let resp = client
                .post(format!("{}/auth/token", proxy_url))
                .header(auth::admin::ADMIN_HEADER, &admin_token)
                .json(&body)
                .send()
                .await?;

            if resp.status().is_success() {
                let data: serde_json::Value = resp.json().await?;
                let token = data.get("token").and_then(|t| t.as_str()).unwrap_or("unknown");
                println!("✓ Bridge token created");
                if let Some(ref n) = name {
                    println!("  App name:   {}", n);
                }
                println!("  Origin:     {}", origin);
                if let Some(ref p) = providers {
                    println!("  Providers:  {}", p.join(", "));
                }
                if let Some(ref m) = models {
                    println!("  Models:     {}", m.join(", "));
                }
                if let Some(rl) = rate_limit {
                    println!("  Rate limit: {} req/hr", rl);
                }
                println!("  TTL:        {}s", ttl);
                println!("  Token:      {}", token);
                println!("\n  Use: Authorization: Bearer {}", token);
            } else {
                let text = resp.text().await.unwrap_or_default();
                anyhow::bail!("Failed to create token: {}", text);
            }
        }
        TokenAction::List => {
            let client = reqwest::Client::new();
            let resp = client
                .get(format!("{}/auth/tokens", proxy_url))
                .header(auth::admin::ADMIN_HEADER, &admin_token)
                .send()
                .await;

            match resp {
                Ok(r) if r.status().is_success() => {
                    let data: serde_json::Value = r.json().await?;
                    let tokens = data.get("tokens").and_then(|t| t.as_array());
                    match tokens {
                        Some(tokens) if !tokens.is_empty() => {
                            println!("Active bridge tokens:");
                            for t in tokens {
                                let prefix = t.get("token_prefix").and_then(|v| v.as_str()).unwrap_or("???");
                                let origin = t.get("origin").and_then(|v| v.as_str()).unwrap_or("?");
                                let app = t.get("app_name").and_then(|v| v.as_str());
                                let expires = t.get("expires_in_secs").and_then(|v| v.as_u64()).unwrap_or(0);
                                let providers = t.get("allowed_providers").and_then(|v| v.as_array());
                                let models = t.get("allowed_models").and_then(|v| v.as_array());

                                println!("  • {}", prefix);
                                if let Some(name) = app {
                                    println!("    Name:      {}", name);
                                }
                                println!("    Origin:    {}", origin);
                                println!("    Expires:   {}s", expires);
                                if let Some(p) = providers {
                                    let names: Vec<&str> = p.iter().filter_map(|v| v.as_str()).collect();
                                    println!("    Providers: {}", names.join(", "));
                                }
                                if let Some(m) = models {
                                    let names: Vec<&str> = m.iter().filter_map(|v| v.as_str()).collect();
                                    println!("    Models:    {}", names.join(", "));
                                }
                            }
                        }
                        _ => {
                            println!("No active bridge tokens.");
                        }
                    }
                }
                _ => {
                    anyhow::bail!("Hostless proxy is not running. Start it with: hostless serve");
                }
            }
        }
        TokenAction::Revoke { token } => {
            let client = reqwest::Client::new();
            let resp = client
                .post(format!("{}/auth/revoke", proxy_url))
                .header(auth::admin::ADMIN_HEADER, &admin_token)
                .json(&serde_json::json!({ "token": token }))
                .send()
                .await;

            match resp {
                Ok(r) if r.status().is_success() => {
                    println!("✓ Token revoked");
                }
                Ok(r) => {
                    let text = r.text().await.unwrap_or_default();
                    anyhow::bail!("Failed to revoke token: {}", text);
                }
                Err(_) => {
                    anyhow::bail!("Hostless proxy is not running. Start it with: hostless serve");
                }
            }
        }
    }

    Ok(())
}

async fn handle_route(action: RouteAction) -> Result<()> {
    let client = reqwest::Client::new();
    let admin_token = auth::admin::load_admin_token()?;

    match action {
        RouteAction::List => {
            let daemon_port = process::manager::read_daemon_port().unwrap_or(48282);
            print_routes(daemon_port).await?;
        }
        RouteAction::Add {
            name,
            port,
            daemon_port,
        } => {
            let effective_port = process::manager::read_daemon_port().unwrap_or(daemon_port);
            let body = serde_json::json!({
                "name": name,
                "port": port,
                "auto_token": true,
            });

            let resp = client
                .post(format!("http://localhost:{}/routes/register", effective_port))
                .header(auth::admin::ADMIN_HEADER, &admin_token)
                .json(&body)
                .send()
                .await;

            match resp {
                Ok(r) if r.status().is_success() => {
                    let data: serde_json::Value = r.json().await?;
                    let url = data["url"].as_str().unwrap_or("?");
                    println!("✓ Route registered: {}", url);
                    if let Some(token) = data["token"]["token"].as_str() {
                        println!("  Token: {}", token);
                    }
                }
                Ok(r) => {
                    let text = r.text().await.unwrap_or_default();
                    anyhow::bail!("Failed to register route: {}", text);
                }
                Err(_) => {
                    anyhow::bail!("Hostless daemon is not running. Start it with: hostless serve");
                }
            }
        }
        RouteAction::Remove { name, daemon_port } => {
            let effective_port = process::manager::read_daemon_port().unwrap_or(daemon_port);
            let resp = client
                .post(format!(
                    "http://localhost:{}/routes/deregister",
                    effective_port
                ))
                .header(auth::admin::ADMIN_HEADER, &admin_token)
                .json(&serde_json::json!({ "name": name }))
                .send()
                .await;

            match resp {
                Ok(r) if r.status().is_success() => {
                    println!("✓ Route removed for '{}'", name);
                }
                Ok(r) => {
                    let text = r.text().await.unwrap_or_default();
                    anyhow::bail!("Failed to remove route: {}", text);
                }
                Err(_) => {
                    anyhow::bail!("Hostless daemon is not running. Start it with: hostless serve");
                }
            }
        }
    }

    Ok(())
}

async fn handle_alias(action: AliasAction) -> Result<()> {
    let client = reqwest::Client::new();
    let admin_token = auth::admin::load_admin_token()?;

    match action {
        AliasAction::List { daemon_port } => {
            let effective_port = process::manager::read_daemon_port().unwrap_or(daemon_port);
            let resp = client
                .get(format!("http://localhost:{}/routes", effective_port))
                .header(auth::admin::ADMIN_HEADER, &admin_token)
                .send()
                .await;

            match resp {
                Ok(r) if r.status().is_success() => {
                    let data: serde_json::Value = r.json().await?;
                    let routes = data.get("routes").and_then(|r| r.as_array());
                    match routes {
                        Some(routes) if !routes.is_empty() => {
                            println!("Aliases/routes:");
                            for route in routes {
                                let hostname = route
                                    .get("hostname")
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("?");
                                let port = route
                                    .get("target_port")
                                    .and_then(|v| v.as_u64())
                                    .unwrap_or(0);
                                println!("  • {} -> 127.0.0.1:{}", hostname, port);
                            }
                        }
                        _ => println!("No aliases/routes."),
                    }
                }
                _ => anyhow::bail!("Hostless daemon is not running. Start it with: hostless serve"),
            }
        }
        AliasAction::Add {
            name,
            port,
            daemon_port,
        } => {
            let effective_port = process::manager::read_daemon_port().unwrap_or(daemon_port);
            let body = serde_json::json!({
                "name": name,
                "port": port,
                "auto_token": false,
            });

            let resp = client
                .post(format!("http://localhost:{}/routes/register", effective_port))
                .header(auth::admin::ADMIN_HEADER, &admin_token)
                .json(&body)
                .send()
                .await;

            match resp {
                Ok(r) if r.status().is_success() => {
                    let data: serde_json::Value = r.json().await?;
                    let url = data["url"].as_str().unwrap_or("?");
                    println!("✓ Alias registered: {}", url);
                }
                Ok(r) => {
                    let text = r.text().await.unwrap_or_default();
                    anyhow::bail!("Failed to register alias: {}", text);
                }
                Err(_) => anyhow::bail!("Hostless daemon is not running. Start it with: hostless serve"),
            }
        }
        AliasAction::Remove { name, daemon_port } => {
            let effective_port = process::manager::read_daemon_port().unwrap_or(daemon_port);
            let resp = client
                .post(format!("http://localhost:{}/routes/deregister", effective_port))
                .header(auth::admin::ADMIN_HEADER, &admin_token)
                .json(&serde_json::json!({ "name": name }))
                .send()
                .await;

            match resp {
                Ok(r) if r.status().is_success() => {
                    println!("✓ Alias removed for '{}'", name);
                }
                Ok(r) => {
                    let text = r.text().await.unwrap_or_default();
                    anyhow::bail!("Failed to remove alias: {}", text);
                }
                Err(_) => anyhow::bail!("Hostless daemon is not running. Start it with: hostless serve"),
            }
        }
    }

    Ok(())
}

fn handle_trust() -> Result<()> {
    let config_dir = config::AppConfig::config_dir()?;
    let ca_path = config_dir.join("ca.pem");

    if !ca_path.exists() {
        anyhow::bail!(
            "No CA certificate found at {}. Start the server with --tls first to generate one.",
            ca_path.display()
        );
    }

    #[cfg(target_os = "macos")]
    {
        println!("Adding CA certificate to macOS login keychain...");
        println!("(You may be prompted for your login password)");
        let status = std::process::Command::new("security")
            .args([
                "add-trusted-cert",
                "-d",
                "-r",
                "trustRoot",
                "-k",
            ])
            .arg(dirs::home_dir().unwrap().join("Library/Keychains/login.keychain-db"))
            .arg(&ca_path)
            .status()
            .context("Failed to run 'security' command")?;

        if status.success() {
            println!("✓ CA certificate trusted");
        } else {
            anyhow::bail!("Failed to trust CA certificate (exit code: {:?})", status.code());
        }
    }

    #[cfg(target_os = "linux")]
    {
        let trust = detect_linux_trust_config();
        let dest = format!("{}/hostless-ca.crt", trust.cert_dir);

        println!("Detected Linux trust setup: {}", trust.label);
        println!("Copying CA certificate to {}...", dest);
        println!("This requires sudo access.");

        let mkdir_status = std::process::Command::new("sudo")
            .args(["mkdir", "-p", trust.cert_dir])
            .status()
            .context("Failed to create certificate trust directory")?;

        if !mkdir_status.success() {
            anyhow::bail!("Failed to create trust directory {}", trust.cert_dir);
        }

        let status = std::process::Command::new("sudo")
            .args(["cp", &ca_path.to_string_lossy(), &dest])
            .status()
            .context("Failed to copy CA certificate")?;

        if !status.success() {
            anyhow::bail!("Failed to copy CA certificate");
        }

        let status = std::process::Command::new("sudo")
            .arg(trust.update_command)
            .status()
            .with_context(|| format!("Failed to run {}", trust.update_command))?;

        if status.success() {
            println!("✓ CA certificate trusted");
        } else {
            anyhow::bail!("Failed to update CA certificates");
        }
    }

    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        println!("CA certificate is at: {}", ca_path.display());
        println!("Please trust it manually in your OS certificate store.");
    }

    Ok(())
}

fn handle_hosts(action: HostsAction) -> Result<()> {
    match action {
        HostsAction::Sync => {
            let hostnames = load_route_hostnames_from_disk()?;
            hosts::sync_hosts(&hostnames)?;
            println!("✓ Synced {} hostless hostnames into /etc/hosts", hostnames.len());
        }
        HostsAction::Clean => {
            hosts::clean_hosts()?;
            println!("✓ Removed hostless-managed /etc/hosts entries");
        }
    }

    Ok(())
}

fn load_route_hostnames_from_disk() -> Result<Vec<String>> {
    let path = config::AppConfig::config_dir()?.join("routes.json");
    if !path.exists() {
        return Ok(Vec::new());
    }

    let data = std::fs::read_to_string(&path)
        .with_context(|| format!("Failed to read {}", path.display()))?;

    let routes: Vec<serde_json::Value> = serde_json::from_str(&data)
        .with_context(|| format!("Failed to parse {}", path.display()))?;

    let mut hostnames: Vec<String> = routes
        .into_iter()
        .filter_map(|r| r.get("hostname").and_then(|h| h.as_str()).map(|s| s.to_string()))
        .collect();
    hostnames.sort();
    hostnames.dedup();
    Ok(hostnames)
}

#[cfg(target_os = "linux")]
#[derive(Clone, Copy)]
struct LinuxTrustConfig {
    label: &'static str,
    cert_dir: &'static str,
    update_command: &'static str,
}

#[cfg(target_os = "linux")]
fn linux_trust_configs() -> [LinuxTrustConfig; 4] {
    [
        LinuxTrustConfig {
            label: "debian/ubuntu",
            cert_dir: "/usr/local/share/ca-certificates",
            update_command: "update-ca-certificates",
        },
        LinuxTrustConfig {
            label: "arch",
            cert_dir: "/etc/ca-certificates/trust-source/anchors",
            update_command: "update-ca-trust",
        },
        LinuxTrustConfig {
            label: "fedora/rhel/centos",
            cert_dir: "/etc/pki/ca-trust/source/anchors",
            update_command: "update-ca-trust",
        },
        LinuxTrustConfig {
            label: "opensuse",
            cert_dir: "/etc/pki/trust/anchors",
            update_command: "update-ca-certificates",
        },
    ]
}

#[cfg(target_os = "linux")]
fn command_exists(cmd: &str) -> bool {
    std::process::Command::new("which")
        .arg(cmd)
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

#[cfg(target_os = "linux")]
fn detect_linux_trust_config() -> LinuxTrustConfig {
    let configs = linux_trust_configs();

    if let Ok(os_release) = std::fs::read_to_string("/etc/os-release") {
        let data = os_release.to_lowercase();
        if data.contains("arch") {
            return configs[1];
        }
        if data.contains("fedora") || data.contains("rhel") || data.contains("centos") {
            return configs[2];
        }
        if data.contains("suse") {
            return configs[3];
        }
        if data.contains("debian") || data.contains("ubuntu") {
            return configs[0];
        }
    }

    // Fallback: probe available trust commands + expected trust directory roots.
    for config in configs {
        let parent_exists = Path::new(config.cert_dir)
            .parent()
            .map(|parent| parent.exists())
            .unwrap_or(false);
        if command_exists(config.update_command) && parent_exists {
            return config;
        }
    }

    // Safe fallback for unknown distros.
    configs[0]
}

/// Guard that cleans up daemon files when dropped (foreground mode).
struct DaemonCleanupGuard;

impl Drop for DaemonCleanupGuard {
    fn drop(&mut self) {
        process::manager::cleanup_daemon_files();
    }
}

fn init_tracing(verbose: bool) {
    let filter = if verbose {
        "hostless=debug,tower_http=debug"
    } else {
        "hostless=info"
    };
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| filter.into()),
        )
        .try_init();
}

async fn shutdown_signal() {
    tokio::signal::ctrl_c()
        .await
        .expect("Failed to install CTRL+C handler");
    info!("Shutdown signal received, gracefully stopping...");
}
