//! Process wrapping and lifecycle management.
//!
//! Spawns child processes (dev servers, scripts, etc.) wrapped with:
//! - PORT/HOST env vars injected
//! - Framework-specific CLI flag injection (vite --port, next -p, etc.)
//! - Signal forwarding (SIGINT/SIGTERM)
//! - Route auto-registration with the hostless daemon
//! - Token auto-provisioning

use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::path::PathBuf;

use anyhow::{Context, Result};
use fs2::FileExt;
use tracing::{info, warn};

/// Configuration for spawning a wrapped process.
#[derive(Debug, Clone)]
pub struct SpawnConfig {
    /// App name (becomes <name>.localhost)
    pub name: String,
    /// The command to run (passed to /bin/sh -c)
    pub command: String,
    /// Target port for the app. If None, a random port 4000-4999 is assigned.
    pub port: Option<u16>,
    /// The hostless daemon's port (default 11434)
    pub daemon_port: u16,
    /// Whether to auto-provision a bridge token
    pub auto_token: bool,
    /// Provider scope for auto-provisioned token
    pub allowed_providers: Option<Vec<String>>,
    /// Model scope for auto-provisioned token
    pub allowed_models: Option<Vec<String>>,
    /// Rate limit for auto-provisioned token
    pub rate_limit: Option<u64>,
    /// TTL in seconds
    pub ttl: u64,
}

/// Result of registering a route with the daemon.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct RegistrationResult {
    pub hostname: String,
    pub url: String,
    pub token: Option<String>,
}

/// Guard for the daemon startup lock file.
pub struct DaemonStartLock {
    file: File,
}

impl Drop for DaemonStartLock {
    fn drop(&mut self) {
        let _ = self.file.unlock();
    }
}

/// Find a random available port in the 4000-4999 range.
pub fn find_available_port() -> Result<u16> {
    use std::net::TcpListener;

    // Try random ports in 4000-4999
    for _ in 0..100 {
        let port = 4000 + (rand::random::<u16>() % 1000);
        if TcpListener::bind(("127.0.0.1", port)).is_ok() {
            return Ok(port);
        }
    }

    // Fallback: let OS assign
    let listener = TcpListener::bind("127.0.0.1:0")
        .context("Failed to find available port")?;
    let port = listener.local_addr()?.port();
    Ok(port)
}

/// Detect the framework from the command and inject appropriate flags.
///
/// Returns the modified command with --port/--host flags appended.
pub fn inject_framework_flags(command: &str, port: u16) -> String {
    let cmd_lower = command.to_lowercase();

    // Extract the base command (first word)
    let base_cmd = command.split_whitespace().next().unwrap_or("");
    let base_name = std::path::Path::new(base_cmd)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or(base_cmd);

    // Check known frameworks for --port/--host flag conventions
    // Only inject if the user hasn't already specified --port
    if command.contains("--port") || command.contains("-p ") {
        return command.to_string();
    }

    match base_name {
        "vite" | "astro" | "react-router" | "ng" | "nuxt" | "remix" => {
            format!("{} --port {} --host 127.0.0.1", command, port)
        }
        "next" => {
            format!("{} -p {} -H 127.0.0.1", command, port)
        }
        _ => {
            // Check if it's an npm/yarn/pnpm script that wraps a known framework
            if cmd_lower.contains("vite")
                || cmd_lower.contains("astro")
                || cmd_lower.contains("nuxt")
            {
                // For npm run dev with vite/astro, flags go after --
                if base_name == "npm" || base_name == "yarn" || base_name == "pnpm" {
                    format!("{} -- --port {} --host 127.0.0.1", command, port)
                } else {
                    command.to_string()
                }
            } else {
                command.to_string()
            }
        }
    }
}

/// Build the environment variables for the child process.
pub fn build_child_env(
    port: u16,
    token: Option<&str>,
    daemon_port: u16,
    app_name: &str,
) -> HashMap<String, String> {
    let mut env: HashMap<String, String> = std::env::vars().collect();

    // Core env vars
    env.insert("PORT".to_string(), port.to_string());
    env.insert("HOST".to_string(), "127.0.0.1".to_string());

    // Vite needs this to allow .localhost subdomains
    env.insert(
        "__VITE_ADDITIONAL_SERVER_ALLOWED_HOSTS".to_string(),
        ".localhost".to_string(),
    );

    // Hostless-specific env vars
    let hostless_url = format!("http://{}.localhost:{}", app_name, daemon_port);
    env.insert("HOSTLESS_URL".to_string(), hostless_url);
    env.insert(
        "HOSTLESS_API".to_string(),
        format!("http://localhost:{}", daemon_port),
    );

    if let Some(token) = token {
        env.insert("HOSTLESS_TOKEN".to_string(), token.to_string());
    }

    // Prepend node_modules/.bin to PATH if it exists
    if let Ok(cwd) = std::env::current_dir() {
        let nm_bin = cwd.join("node_modules").join(".bin");
        if nm_bin.exists() {
            let current_path = env.get("PATH").cloned().unwrap_or_default();
            env.insert(
                "PATH".to_string(),
                format!("{}:{}", nm_bin.display(), current_path),
            );
        }
    }

    env
}

/// Register a route with the running hostless daemon via HTTP.
pub async fn register_with_daemon(
    config: &SpawnConfig,
    port: u16,
    pid: Option<u32>,
) -> Result<RegistrationResult> {
    let client = reqwest::Client::new();
    let daemon_url = format!("http://localhost:{}", config.daemon_port);
    let admin_token = crate::auth::admin::load_admin_token()?;

    let body = serde_json::json!({
        "name": config.name,
        "port": port,
        "pid": pid,
        "auto_token": config.auto_token,
        "allowed_providers": config.allowed_providers,
        "allowed_models": config.allowed_models,
        "rate_limit": config.rate_limit,
        "ttl": config.ttl,
    });

    let resp = client
        .post(format!("{}/routes/register", daemon_url))
        .header(crate::auth::admin::ADMIN_HEADER, admin_token)
        .json(&body)
        .send()
        .await
        .context("Failed to connect to hostless daemon. Is it running? (hostless serve)")?;

    if !resp.status().is_success() {
        let text = resp.text().await.unwrap_or_default();
        anyhow::bail!("Failed to register route: {}", text);
    }

    let data: serde_json::Value = resp.json().await?;
    let hostname = data["hostname"].as_str().unwrap_or("").to_string();
    let url = data["url"].as_str().unwrap_or("").to_string();
    let token = data["token"]["token"].as_str().map(|s| s.to_string());

    Ok(RegistrationResult {
        hostname,
        url,
        token,
    })
}

/// Deregister a route with the running hostless daemon via HTTP.
pub async fn deregister_with_daemon(daemon_port: u16, name: &str) -> Result<()> {
    let client = reqwest::Client::new();
    let daemon_url = format!("http://localhost:{}", daemon_port);
    let admin_token = match crate::auth::admin::load_admin_token() {
        Ok(token) => token,
        Err(e) => {
            warn!("Failed to load admin token for deregistration: {}", e);
            return Ok(());
        }
    };

    let resp = client
        .post(format!("{}/routes/deregister", daemon_url))
        .header(crate::auth::admin::ADMIN_HEADER, admin_token)
        .json(&serde_json::json!({ "name": name }))
        .send()
        .await;

    match resp {
        Ok(r) if r.status().is_success() => Ok(()),
        Ok(r) => {
            let text = r.text().await.unwrap_or_default();
            warn!("Failed to deregister route: {}", text);
            Ok(()) // Non-fatal
        }
        Err(e) => {
            warn!("Failed to connect to daemon for deregistration: {}", e);
            Ok(()) // Non-fatal — daemon may have stopped
        }
    }
}

/// Check if the hostless daemon is running.
pub async fn is_daemon_running(port: u16) -> bool {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(2))
        .build()
        .unwrap();

    match client
        .get(format!("http://localhost:{}/health", port))
        .send()
        .await
    {
        Ok(r) => r.status().is_success(),
        Err(_) => false,
    }
}

/// Read the daemon port from ~/.hostless/hostless.port
pub fn read_daemon_port() -> Option<u16> {
    let port_file = daemon_port_path().ok()?;
    let content = std::fs::read_to_string(port_file).ok()?;
    content.trim().parse().ok()
}

/// Acquire an exclusive lock used to serialize daemon auto-start attempts.
pub fn acquire_daemon_start_lock() -> Result<DaemonStartLock> {
    let lock_path = daemon_start_lock_path()?;
    let file = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .open(lock_path)
        .context("Failed to open daemon startup lock file")?;

    file.lock_exclusive()
        .context("Failed to acquire daemon startup lock")?;

    Ok(DaemonStartLock { file })
}

/// Start the hostless daemon process in background mode.
pub fn start_daemon_process(port: u16) -> Result<()> {
    let exe = std::env::current_exe()
        .context("Failed to resolve current hostless executable")?;

    std::process::Command::new(exe)
        .arg("serve")
        .arg("--port")
        .arg(port.to_string())
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .context("Failed to spawn hostless daemon process")?;

    Ok(())
}

/// Read the daemon PID from ~/.hostless/hostless.pid
pub fn read_daemon_pid() -> Option<u32> {
    let pid_file = daemon_pid_path().ok()?;
    let content = std::fs::read_to_string(pid_file).ok()?;
    content.trim().parse().ok()
}

/// Write daemon PID to ~/.hostless/hostless.pid
pub fn write_daemon_pid(pid: u32) -> Result<()> {
    let path = daemon_pid_path()?;
    std::fs::write(&path, pid.to_string())
        .context("Failed to write daemon PID file")?;
    Ok(())
}

/// Write daemon port to ~/.hostless/hostless.port
pub fn write_daemon_port(port: u16) -> Result<()> {
    let path = daemon_port_path()?;
    std::fs::write(&path, port.to_string())
        .context("Failed to write daemon port file")?;
    Ok(())
}

/// Remove daemon PID and port files.
pub fn cleanup_daemon_files() {
    if let Ok(pid_path) = daemon_pid_path() {
        let _ = std::fs::remove_file(pid_path);
    }
    if let Ok(port_path) = daemon_port_path() {
        let _ = std::fs::remove_file(port_path);
    }
}

fn daemon_pid_path() -> Result<PathBuf> {
    let dir = crate::config::AppConfig::config_dir()?;
    Ok(dir.join("hostless.pid"))
}

fn daemon_port_path() -> Result<PathBuf> {
    let dir = crate::config::AppConfig::config_dir()?;
    Ok(dir.join("hostless.port"))
}

fn daemon_start_lock_path() -> Result<PathBuf> {
    let dir = crate::config::AppConfig::config_dir()?;
    Ok(dir.join("daemon-start.lock"))
}

/// Spawn and manage a wrapped child process.
///
/// This is the main entry point for `hostless run <name> <command>`.
/// It:
/// 1. Finds an available port (or uses the specified one)
/// 2. Registers a route with the daemon
/// 3. Spawns the child process with injected env vars
/// 4. Forwards signals to the child
/// 5. Waits for the child to exit
/// 6. Deregisters the route on cleanup
pub async fn spawn_and_manage(config: SpawnConfig) -> Result<i32> {
    // 1. Find/assign port
    let port = match config.port {
        Some(p) => p,
        None => find_available_port()?,
    };

    info!(
        app = config.name.as_str(),
        port = port,
        "Assigned port for app"
    );

    // 2. Inject framework flags
    let command = inject_framework_flags(&config.command, port);

    // 3. Register with daemon (get token)
    // Register without PID first, then update after spawn with real PID.
    let registration = register_with_daemon(&config, port, None).await?;

    println!("✓ Registered route: {}", registration.url);
    if let Some(ref token) = registration.token {
        println!("  Token: {}...", &token[..token.len().min(24)]);
    }

    // 4. Build env
    let env = build_child_env(
        port,
        registration.token.as_deref(),
        config.daemon_port,
        &config.name,
    );

    // 5. Spawn the child process
    let mut child = tokio::process::Command::new("/bin/sh")
        .arg("-c")
        .arg(&command)
        .envs(&env)
        .stdin(std::process::Stdio::inherit())
        .stdout(std::process::Stdio::inherit())
        .stderr(std::process::Stdio::inherit())
        .kill_on_drop(false) // We handle cleanup ourselves
        .spawn()
        .context(format!("Failed to spawn command: {}", command))?;

    let child_pid = child.id();
    info!(pid = child_pid.unwrap_or(0), command = command.as_str(), "Spawned child process");

    // Update the route with the actual PID
    // (Re-register with correct PID — the daemon will overwrite)
    if let Some(pid) = child_pid {
        if let Err(e) = register_with_daemon(&config, port, Some(pid)).await {
            warn!(pid = pid, error = %e, "Failed to update route with child PID");
        }
    } else {
        warn!("Child PID unavailable; route will not be tied to process liveness checks");
    }

    // 6. Set up signal forwarding
    let name_for_cleanup = config.name.clone();
    let daemon_port = config.daemon_port;

    // Wait for the child to exit
    let status = child.wait().await.context("Failed to wait for child")?;

    let exit_code = if let Some(code) = status.code() {
        code
    } else {
        // Killed by signal
        #[cfg(unix)]
        {
            use std::os::unix::process::ExitStatusExt;
            if let Some(sig) = status.signal() {
                128 + sig
            } else {
                1
            }
        }
        #[cfg(not(unix))]
        {
            1
        }
    };

    // 7. Cleanup: deregister route
    info!(
        app = name_for_cleanup.as_str(),
        exit_code = exit_code,
        "Child process exited"
    );
    deregister_with_daemon(daemon_port, &name_for_cleanup).await?;
    println!("✓ Route deregistered for {}", name_for_cleanup);

    Ok(exit_code)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_inject_framework_flags_vite() {
        let cmd = inject_framework_flags("vite", 4001);
        assert_eq!(cmd, "vite --port 4001 --host 127.0.0.1");
    }

    #[test]
    fn test_inject_framework_flags_next() {
        let cmd = inject_framework_flags("next dev", 4001);
        assert_eq!(cmd, "next dev -p 4001 -H 127.0.0.1");
    }

    #[test]
    fn test_inject_framework_flags_already_has_port() {
        let cmd = inject_framework_flags("vite --port 3000", 4001);
        assert_eq!(cmd, "vite --port 3000"); // Don't double-inject
    }

    #[test]
    fn test_inject_framework_flags_unknown() {
        let cmd = inject_framework_flags("python -m http.server", 4001);
        assert_eq!(cmd, "python -m http.server"); // Don't inject
    }

    #[test]
    fn test_inject_framework_flags_npm_vite() {
        let cmd = inject_framework_flags("npm run dev", 4001);
        // npm doesn't auto-detect as vite without the word in the command
        assert_eq!(cmd, "npm run dev");
    }

    #[test]
    fn test_build_child_env() {
        let env = build_child_env(4001, Some("sk_local_test"), 11434, "myapp");
        assert_eq!(env.get("PORT").unwrap(), "4001");
        assert_eq!(env.get("HOST").unwrap(), "127.0.0.1");
        assert_eq!(env.get("HOSTLESS_TOKEN").unwrap(), "sk_local_test");
        assert_eq!(
            env.get("HOSTLESS_URL").unwrap(),
            "http://myapp.localhost:11434"
        );
        assert_eq!(
            env.get("HOSTLESS_API").unwrap(),
            "http://localhost:11434"
        );
        assert_eq!(
            env.get("__VITE_ADDITIONAL_SERVER_ALLOWED_HOSTS").unwrap(),
            ".localhost"
        );
    }

    #[test]
    fn test_build_child_env_no_token() {
        let env = build_child_env(4001, None, 11434, "myapp");
        assert!(env.get("HOSTLESS_TOKEN").is_none());
    }
}
