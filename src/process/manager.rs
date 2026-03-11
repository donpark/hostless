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
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result};
use fs2::FileExt;
#[cfg(unix)]
use nix::sys::signal::{self, Signal};
#[cfg(unix)]
use nix::unistd::Pid;
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
    /// The hostless daemon's port (default 48282)
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

/// Sanitize a string for use as a `.localhost` hostname label.
/// Keeps lowercase alphanumeric and hyphens, collapses repeated hyphens,
/// and trims leading/trailing hyphens.
pub fn sanitize_for_hostname(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    let mut previous_was_hyphen = false;

    for ch in name.chars() {
        let lower = ch.to_ascii_lowercase();
        if lower.is_ascii_alphanumeric() {
            out.push(lower);
            previous_was_hyphen = false;
        } else if !previous_was_hyphen {
            out.push('-');
            previous_was_hyphen = true;
        }
    }

    out.trim_matches('-').to_string()
}

/// Infer a project name from package.json, git root, or directory basename.
pub fn infer_project_name(start_dir: Option<&Path>) -> Result<String> {
    let cwd = match start_dir {
        Some(dir) => dir.to_path_buf(),
        None => std::env::current_dir().context("Failed to resolve current directory")?,
    };

    if let Some(pkg_name) = find_package_json_name(&cwd) {
        let sanitized = sanitize_for_hostname(&pkg_name);
        if !sanitized.is_empty() {
            return Ok(sanitized);
        }
    }

    if let Some(git_root) = find_git_root(&cwd) {
        let sanitized = sanitize_for_hostname(
            git_root
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or_default(),
        );
        if !sanitized.is_empty() {
            return Ok(sanitized);
        }
    }

    let fallback = sanitize_for_hostname(
        cwd.file_name()
            .and_then(|n| n.to_str())
            .unwrap_or_default(),
    );
    if fallback.is_empty() {
        anyhow::bail!("Could not infer a valid app name from current directory")
    }
    Ok(fallback)
}

/// Detect an optional worktree prefix from the current git branch.
/// Returns None for single-worktree repos and default branches.
pub fn detect_worktree_prefix(start_dir: Option<&Path>) -> Option<String> {
    let cwd = start_dir
        .map(|p| p.to_path_buf())
        .or_else(|| std::env::current_dir().ok())?;

    let list_output = Command::new("git")
        .args(["worktree", "list", "--porcelain"])
        .current_dir(&cwd)
        .output()
        .ok()?;
    if !list_output.status.success() {
        return None;
    }

    let list_stdout = String::from_utf8_lossy(&list_output.stdout);
    let worktree_count = list_stdout
        .lines()
        .filter(|line| line.starts_with("worktree "))
        .count();
    if worktree_count <= 1 {
        return None;
    }

    let branch_output = Command::new("git")
        .args(["rev-parse", "--abbrev-ref", "HEAD"])
        .current_dir(&cwd)
        .output()
        .ok()?;
    if !branch_output.status.success() {
        return None;
    }

    let branch = String::from_utf8_lossy(&branch_output.stdout).trim().to_string();
    if branch == "HEAD" || branch == "main" || branch == "master" {
        return None;
    }

    let segment = branch.split('/').next_back().unwrap_or_default();
    let sanitized = sanitize_for_hostname(segment);
    if sanitized.is_empty() {
        None
    } else {
        Some(sanitized)
    }
}

fn find_package_json_name(start_dir: &Path) -> Option<String> {
    let mut dir = start_dir.to_path_buf();
    loop {
        let package_path = dir.join("package.json");
        if let Ok(raw) = std::fs::read_to_string(&package_path) {
            if let Ok(value) = serde_json::from_str::<serde_json::Value>(&raw) {
                if let Some(name) = value.get("name").and_then(|v| v.as_str()) {
                    let stripped = name
                        .split('/')
                        .next_back()
                        .unwrap_or(name)
                        .to_string();
                    if !stripped.is_empty() {
                        return Some(stripped);
                    }
                }
            }
        }

        let parent = dir.parent()?.to_path_buf();
        if parent == dir {
            return None;
        }
        dir = parent;
    }
}

fn find_git_root(start_dir: &Path) -> Option<PathBuf> {
    let output = Command::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .current_dir(start_dir)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let root = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if root.is_empty() {
        None
    } else {
        Some(PathBuf::from(root))
    }
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
        "vite" | "astro" | "react-router" | "ng" | "nuxt" | "remix" | "react-native" => {
            format!("{} --port {} --host 127.0.0.1", command, port)
        }
        "expo" => {
            format!("{} --port {} --host localhost", command, port)
        }
        "next" => {
            format!("{} -p {} -H 127.0.0.1", command, port)
        }
        _ => {
            // Check if it's an npm/yarn/pnpm script that wraps a known framework
            if cmd_lower.contains("vite")
                || cmd_lower.contains("astro")
                || cmd_lower.contains("nuxt")
                || cmd_lower.contains("expo")
                || cmd_lower.contains("react-native")
            {
                // For npm run dev with vite/astro, flags go after --
                if base_name == "npm" || base_name == "yarn" || base_name == "pnpm" {
                    let host = if cmd_lower.contains("expo") {
                        "localhost"
                    } else {
                        "127.0.0.1"
                    };
                    format!("{} -- --port {} --host {}", command, port, host)
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

fn requires_shell(command: &str) -> bool {
    let shell_ops = ['|', '&', ';', '<', '>', '`', '$', '(', ')', '\n'];
    command.chars().any(|c| shell_ops.contains(&c))
}

fn prepare_child_command(command: &str) -> Result<tokio::process::Command> {
    if requires_shell(command) {
        let mut cmd = tokio::process::Command::new("/bin/sh");
        cmd.arg("-c").arg(command);
        return Ok(cmd);
    }

    let parts = shell_words::split(command)
        .context("Failed to parse wrapped command")?;
    let (program, args) = parts
        .split_first()
        .context("Wrapped command cannot be empty")?;

    let mut cmd = tokio::process::Command::new(program);
    cmd.args(args);
    Ok(cmd)
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
    let client = match reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(2))
        .build()
    {
        Ok(client) => client,
        Err(e) => {
            warn!(error = %e, "Failed to build daemon health check client");
            return false;
        }
    };

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
    let content = read_locked_file_to_string(&port_file).ok()?;
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
    start_daemon_process_with_options(port, false, false, false, None)
}

/// Start the hostless daemon process in background mode with explicit serve flags.
pub fn start_daemon_process_with_options(
    port: u16,
    tls: bool,
    dev_mode: bool,
    verbose: bool,
    token_persistence: Option<&str>,
) -> Result<()> {
    let exe = std::env::current_exe()
        .context("Failed to resolve current hostless executable")?;

    let mut cmd = std::process::Command::new(exe);
    cmd.arg("serve").arg("--port").arg(port.to_string());

    if tls {
        cmd.arg("--tls");
    }
    if dev_mode {
        cmd.arg("--dev-mode");
    }
    if verbose {
        cmd.arg("--verbose");
    }
    if let Some(mode) = token_persistence {
        cmd.arg("--token-persistence").arg(mode);
    }

    // Ensure the detached child does not recurse into daemon mode.
    cmd.arg("--daemonized");

    // Keep daemon output available for diagnostics.
    let log_path = daemon_log_path()?;
    let stdout = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .context("Failed to open daemon log file")?;
    let stderr = stdout
        .try_clone()
        .context("Failed to clone daemon log file handle")?;

    cmd
        .stdin(std::process::Stdio::null())
        .stdout(stdout)
        .stderr(stderr)
        .spawn()
        .context("Failed to spawn hostless daemon process")?;

    Ok(())
}

/// Read the daemon PID from ~/.hostless/hostless.pid
pub fn read_daemon_pid() -> Option<u32> {
    let pid_file = daemon_pid_path().ok()?;
    let content = read_locked_file_to_string(&pid_file).ok()?;
    content.trim().parse().ok()
}

/// Write daemon PID to ~/.hostless/hostless.pid
pub fn write_daemon_pid(pid: u32) -> Result<()> {
    let path = daemon_pid_path()?;
    write_locked_file(&path, &pid.to_string())
        .context("Failed to write daemon PID file")?;
    Ok(())
}

/// Write daemon port to ~/.hostless/hostless.port
pub fn write_daemon_port(port: u16) -> Result<()> {
    let path = daemon_port_path()?;
    write_locked_file(&path, &port.to_string())
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

fn daemon_log_path() -> Result<PathBuf> {
    let dir = crate::config::AppConfig::config_dir()?;
    Ok(dir.join("hostless.log"))
}

fn read_locked_file_to_string(path: &Path) -> Result<String> {
    let mut file = OpenOptions::new()
        .read(true)
        .open(path)
        .context("Failed to open file for reading")?;
    file.lock_shared().context("Failed to lock file for reading")?;

    let mut buf = String::new();
    file.read_to_string(&mut buf)
        .context("Failed to read file contents")?;
    file.unlock().context("Failed to unlock file after read")?;
    Ok(buf)
}

fn write_locked_file(path: &Path, content: &str) -> Result<()> {
    let mut file = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .open(path)
        .context("Failed to open file for writing")?;
    file.lock_exclusive().context("Failed to lock file for writing")?;

    file.set_len(0).context("Failed to truncate file")?;
    file.seek(SeekFrom::Start(0))
        .context("Failed to seek to file start")?;
    file.write_all(content.as_bytes())
        .context("Failed to write file contents")?;
    file.sync_all().context("Failed to flush file contents")?;
    file.unlock().context("Failed to unlock file after write")?;
    Ok(())
}

#[cfg(unix)]
fn forward_interrupt_to_child(child_pid: u32) {
    if let Err(e) = signal::kill(Pid::from_raw(child_pid as i32), Signal::SIGINT) {
        warn!(pid = child_pid, error = %e, "Failed to forward SIGINT to child process");
    }
}

#[cfg(not(unix))]
fn forward_interrupt_to_child(child: &mut tokio::process::Child) {
    if let Err(e) = child.start_kill() {
        warn!(error = %e, "Failed to terminate child process after Ctrl+C");
    }
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
    if requires_shell(&command) {
        warn!(
            app = config.name.as_str(),
            "Running wrapped command through shell due to shell operators"
        );
    }
    let mut child_cmd = prepare_child_command(&command)?;
    let mut child = child_cmd
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

    // Wait for the child to exit, forwarding Ctrl+C to keep wrapped process lifecycle sane.
    let status = loop {
        tokio::select! {
            wait_result = child.wait() => {
                break wait_result.context("Failed to wait for child")?;
            }
            ctrl_c = tokio::signal::ctrl_c() => {
                if let Err(e) = ctrl_c {
                    warn!(error = %e, "Failed to listen for Ctrl+C while waiting on child process");
                    continue;
                }

                match child.id() {
                    Some(pid) => {
                        info!(pid = pid, "Forwarding Ctrl+C to child process");
                        #[cfg(unix)]
                        {
                            forward_interrupt_to_child(pid);
                        }
                        #[cfg(not(unix))]
                        {
                            forward_interrupt_to_child(&mut child);
                        }
                    }
                    None => {
                        warn!("Received Ctrl+C but child PID is unavailable");
                    }
                }
            }
        }
    };

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
    use std::fs;
    use std::path::PathBuf;
    use std::process::Command;

    fn temp_dir(prefix: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("{}-{}", prefix, rand::random::<u32>()));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn git_available() -> bool {
        Command::new("git")
            .arg("--version")
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }

    fn run_git(cwd: &Path, args: &[&str]) -> bool {
        Command::new("git")
            .current_dir(cwd)
            .args(args)
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }

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
    fn test_inject_framework_flags_expo() {
        let cmd = inject_framework_flags("expo start", 4001);
        assert_eq!(cmd, "expo start --port 4001 --host localhost");
    }

    #[test]
    fn test_inject_framework_flags_react_native() {
        let cmd = inject_framework_flags("react-native start", 4001);
        assert_eq!(cmd, "react-native start --port 4001 --host 127.0.0.1");
    }

    #[test]
    fn test_inject_framework_flags_npm_expo_script() {
        let cmd = inject_framework_flags("npm run expo", 4001);
        assert_eq!(cmd, "npm run expo -- --port 4001 --host localhost");
    }

    #[test]
    fn test_inject_framework_flags_npm_vite() {
        let cmd = inject_framework_flags("npm run dev", 4001);
        // npm doesn't auto-detect as vite without the word in the command
        assert_eq!(cmd, "npm run dev");
    }

    #[test]
    fn test_requires_shell_detects_shell_operators() {
        assert!(requires_shell("echo hi | cat"));
        assert!(requires_shell("echo hi && echo there"));
        assert!(requires_shell("echo $HOME"));
        assert!(!requires_shell("npm run dev -- --port 4001"));
    }

    #[test]
    fn test_prepare_child_command_direct_path() {
        let cmd = prepare_child_command("npm run dev").unwrap();
        let program = cmd.as_std().get_program().to_string_lossy().to_string();
        assert_eq!(program, "npm");
    }

    #[test]
    fn test_prepare_child_command_shell_fallback() {
        let cmd = prepare_child_command("echo hi | cat").unwrap();
        let program = cmd.as_std().get_program().to_string_lossy().to_string();
        assert_eq!(program, "/bin/sh");
    }

    #[test]
    fn test_build_child_env() {
        let env = build_child_env(4001, Some("sk_local_test"), 48282, "myapp");
        assert_eq!(env.get("PORT").unwrap(), "4001");
        assert_eq!(env.get("HOST").unwrap(), "127.0.0.1");
        assert_eq!(env.get("HOSTLESS_TOKEN").unwrap(), "sk_local_test");
        assert_eq!(
            env.get("HOSTLESS_URL").unwrap(),
            "http://myapp.localhost:48282"
        );
        assert_eq!(
            env.get("HOSTLESS_API").unwrap(),
            "http://localhost:48282"
        );
        assert_eq!(
            env.get("__VITE_ADDITIONAL_SERVER_ALLOWED_HOSTS").unwrap(),
            ".localhost"
        );
    }

    #[test]
    fn test_locked_file_roundtrip() {
        let dir = temp_dir("hostless-manager-lock-io");
        let path = dir.join("state.txt");

        write_locked_file(&path, "12345").unwrap();
        let value = read_locked_file_to_string(&path).unwrap();
        assert_eq!(value, "12345");

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn test_write_locked_file_overwrites_previous_content() {
        let dir = temp_dir("hostless-manager-lock-overwrite");
        let path = dir.join("state.txt");

        write_locked_file(&path, "123456789").unwrap();
        write_locked_file(&path, "42").unwrap();
        let value = read_locked_file_to_string(&path).unwrap();
        assert_eq!(value, "42");

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn test_build_child_env_no_token() {
        let env = build_child_env(4001, None, 48282, "myapp");
        assert!(env.get("HOSTLESS_TOKEN").is_none());
    }

    #[test]
    fn test_sanitize_for_hostname() {
        assert_eq!(sanitize_for_hostname("My_App"), "my-app");
        assert_eq!(sanitize_for_hostname("feature/auth"), "feature-auth");
        assert_eq!(sanitize_for_hostname("---@"), "");
        assert_eq!(sanitize_for_hostname("my--app"), "my-app");
        assert_eq!(sanitize_for_hostname("-myapp-"), "myapp");
        assert_eq!(sanitize_for_hostname("my-app-123"), "my-app-123");
    }

    #[test]
    fn test_infer_project_name_from_package_json() {
        let tmp = temp_dir("hostless-test");
        fs::write(tmp.join("package.json"), r#"{"name":"@org/My_App"}"#).unwrap();

        let inferred = infer_project_name(Some(&tmp)).unwrap();
        assert_eq!(inferred, "my-app");

        let _ = fs::remove_dir_all(tmp);
    }

    #[test]
    fn test_infer_project_name_fallback_to_directory() {
        let tmp = temp_dir("hostless-fallback");

        let inferred = infer_project_name(Some(&tmp)).unwrap();
        assert!(inferred.starts_with("hostless-fallback"));

        let _ = fs::remove_dir_all(tmp);
    }

    #[test]
    fn test_infer_project_name_walks_up_to_parent_package_json() {
        let tmp = temp_dir("hostless-parent-pkg");
        fs::write(tmp.join("package.json"), r#"{"name":"parent-app"}"#).unwrap();
        let nested = tmp.join("src").join("components");
        fs::create_dir_all(&nested).unwrap();

        let inferred = infer_project_name(Some(&nested)).unwrap();
        assert_eq!(inferred, "parent-app");

        let _ = fs::remove_dir_all(tmp);
    }

    #[test]
    fn test_infer_project_name_skips_invalid_package_name() {
        let tmp = temp_dir("hostless-invalid-pkg");
        fs::write(tmp.join("package.json"), r#"{"name":"@@@"}"#).unwrap();

        let inferred = infer_project_name(Some(&tmp)).unwrap();
        assert_ne!(inferred, "");
        assert_ne!(inferred, "@@@");

        let _ = fs::remove_dir_all(tmp);
    }

    #[test]
    fn test_detect_worktree_prefix_returns_none_without_git() {
        let tmp = temp_dir("hostless-no-git");
        let prefix = detect_worktree_prefix(Some(&tmp));
        assert!(prefix.is_none());
        let _ = fs::remove_dir_all(tmp);
    }

    #[test]
    fn test_detect_worktree_prefix_linked_worktree_non_default_branch() {
        if !git_available() {
            return;
        }

        let repo = temp_dir("hostless-worktree-repo");
        if !run_git(&repo, &["init"]) {
            let _ = fs::remove_dir_all(repo);
            return;
        }
        run_git(&repo, &["branch", "-M", "main"]);
        fs::write(repo.join("README.md"), "test").unwrap();
        run_git(&repo, &["add", "."]);
        run_git(&repo, &["-c", "user.name=Test", "-c", "user.email=t@t", "commit", "-m", "init"]);
        run_git(&repo, &["branch", "feature/auth"]);

        let wt = temp_dir("hostless-worktree-linked");
        if !run_git(&repo, &["worktree", "add", wt.to_string_lossy().as_ref(), "feature/auth"]) {
            let _ = fs::remove_dir_all(repo);
            let _ = fs::remove_dir_all(wt);
            return;
        }

        let prefix = detect_worktree_prefix(Some(&wt));
        assert_eq!(prefix.as_deref(), Some("auth"));

        let _ = fs::remove_dir_all(repo);
        let _ = fs::remove_dir_all(wt);
    }

    #[test]
    fn test_detect_worktree_prefix_main_checkout_none() {
        if !git_available() {
            return;
        }

        let repo = temp_dir("hostless-main-worktree-repo");
        if !run_git(&repo, &["init"]) {
            let _ = fs::remove_dir_all(repo);
            return;
        }
        run_git(&repo, &["branch", "-M", "main"]);
        fs::write(repo.join("README.md"), "test").unwrap();
        run_git(&repo, &["add", "."]);
        run_git(&repo, &["-c", "user.name=Test", "-c", "user.email=t@t", "commit", "-m", "init"]);
        run_git(&repo, &["branch", "feature-auth"]);

        let wt = temp_dir("hostless-main-worktree-linked");
        run_git(&repo, &["worktree", "add", wt.to_string_lossy().as_ref(), "feature-auth"]);

        let prefix = detect_worktree_prefix(Some(&repo));
        assert!(prefix.is_none());

        let _ = fs::remove_dir_all(repo);
        let _ = fs::remove_dir_all(wt);
    }
}
