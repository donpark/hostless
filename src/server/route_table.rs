use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Instant;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;
use tracing::{info, warn};

/// An app route mapping a `.localhost` subdomain to a local port.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct AppRoute {
    /// Hostname (e.g., "myapp.localhost")
    pub hostname: String,
    /// Target port on 127.0.0.1
    pub target_port: u16,
    /// PID of the wrapped process (if managed by hostless)
    pub pid: Option<u32>,
    /// Human-readable app name
    pub app_name: String,
    /// When this route was registered
    pub registered_at: Instant,
    /// Associated bridge token (auto-provisioned)
    pub token: Option<String>,
}

/// Serializable route entry for persistence to routes.json
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PersistedRoute {
    pub hostname: String,
    pub target_port: u16,
    pub pid: Option<u32>,
    pub app_name: String,
    /// Unix timestamp (seconds since epoch)
    pub registered_at: u64,
}

/// Route info for API responses (no secrets)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RouteInfo {
    pub hostname: String,
    pub target_port: u16,
    pub pid: Option<u32>,
    pub app_name: String,
    pub url: String,
}

/// Manages the route table mapping hostnames to local app ports.
pub struct RouteTable {
    routes: RwLock<HashMap<String, AppRoute>>,
    /// Server port (used to construct URLs)
    server_port: u16,
}

impl RouteTable {
    pub fn new(server_port: u16) -> Self {
        Self {
            routes: RwLock::new(HashMap::new()),
            server_port,
        }
    }

    /// Register a new route. Returns the full URL for the app.
    pub async fn register(
        &self,
        app_name: &str,
        target_port: u16,
        pid: Option<u32>,
    ) -> Result<AppRoute> {
        let hostname = format!("{}.localhost", app_name);

        let route = AppRoute {
            hostname: hostname.clone(),
            target_port,
            pid,
            app_name: app_name.to_string(),
            registered_at: Instant::now(),
            token: None,
        };

        let mut routes = self.routes.write().await;

        if let Some(existing) = routes.get(&hostname) {
            // If existing route has a PID, check if it's still alive
            if let Some(old_pid) = existing.pid {
                if is_process_alive(old_pid) {
                    anyhow::bail!(
                        "Route '{}' already exists (PID {}). Remove it first or choose a different name.",
                        hostname, old_pid
                    );
                }
                // Old process is dead, allow overwrite
                warn!(
                    hostname = hostname.as_str(),
                    old_pid = old_pid,
                    "Replacing stale route (process dead)"
                );
            }
        }

        routes.insert(hostname.clone(), route.clone());
        info!(
            hostname = hostname.as_str(),
            target_port = target_port,
            "Registered route"
        );

        // Persist to disk
        if let Err(e) = self.persist_locked(&routes) {
            warn!("Failed to persist routes: {}", e);
        }

        Ok(route)
    }

    /// Set the token for a route (after auto-provisioning).
    pub async fn set_token(&self, hostname: &str, token: String) {
        let mut routes = self.routes.write().await;
        if let Some(route) = routes.get_mut(hostname) {
            route.token = Some(token);
        }
    }

    /// Look up a route by hostname (e.g., "myapp.localhost").
    #[allow(dead_code)]
    pub async fn lookup(&self, hostname: &str) -> Option<AppRoute> {
        let routes = self.routes.read().await;
        routes.get(hostname).cloned()
    }

    /// Look up a route by hostname with optional wildcard fallback.
    /// Exact matches always win. When wildcard is enabled, `a.b.localhost`
    /// may match a registered `b.localhost` route.
    pub async fn lookup_with_wildcard(&self, hostname: &str, wildcard: bool) -> Option<AppRoute> {
        let routes = self.routes.read().await;

        if let Some(exact) = routes.get(hostname) {
            return Some(exact.clone());
        }

        if !wildcard {
            return None;
        }

        routes
            .iter()
            .filter(|(registered, _)| hostname.ends_with(&format!(".{}", registered)))
            .max_by_key(|(registered, _)| registered.len())
            .map(|(_, route)| route.clone())
    }

    /// Remove a route by hostname or app name.
    pub async fn remove(&self, name: &str) -> Option<AppRoute> {
        let mut routes = self.routes.write().await;

        // Try exact hostname first
        let hostname = if name.ends_with(".localhost") {
            name.to_string()
        } else {
            format!("{}.localhost", name)
        };

        let removed = routes.remove(&hostname);

        if removed.is_some() {
            info!(hostname = hostname.as_str(), "Removed route");
            if let Err(e) = self.persist_locked(&routes) {
                warn!("Failed to persist routes: {}", e);
            }
        }

        removed
    }

    /// List all active routes.
    pub async fn list(&self) -> Vec<RouteInfo> {
        let routes = self.routes.read().await;
        routes
            .values()
            .map(|r| RouteInfo {
                hostname: r.hostname.clone(),
                target_port: r.target_port,
                pid: r.pid,
                app_name: r.app_name.clone(),
                url: format!("http://{}:{}", r.hostname, self.server_port),
            })
            .collect()
    }

    /// Remove routes whose PIDs are no longer alive.
    /// Returns (removed_count, Vec<removed_tokens>).
    pub async fn cleanup_stale(&self) -> (usize, Vec<String>) {
        let mut routes = self.routes.write().await;
        let before = routes.len();
        let mut removed_tokens = Vec::new();

        routes.retain(|_hostname, route| {
            if let Some(pid) = route.pid {
                if !is_process_alive(pid) {
                    if let Some(ref token) = route.token {
                        removed_tokens.push(token.clone());
                    }
                    return false;
                }
            }
            true
        });

        let removed = before - routes.len();
        if removed > 0 {
            if let Err(e) = self.persist_locked(&routes) {
                warn!("Failed to persist routes after cleanup: {}", e);
            }
        }

        (removed, removed_tokens)
    }

    /// Persist routes to ~/.hostless/routes.json
    fn persist_locked(&self, routes: &HashMap<String, AppRoute>) -> Result<()> {
        let path = routes_file_path()?;
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();

        let persisted: Vec<PersistedRoute> = routes
            .values()
            .map(|r| PersistedRoute {
                hostname: r.hostname.clone(),
                target_port: r.target_port,
                pid: r.pid,
                app_name: r.app_name.clone(),
                registered_at: now, // Approximate — Instant doesn't serialize
            })
            .collect();

        let data = serde_json::to_string_pretty(&persisted)
            .context("Failed to serialize routes")?;

        // Use file locking for safe concurrent access
        use fs2::FileExt;
        let file = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&path)
            .context("Failed to open routes file")?;
        file.lock_exclusive()
            .context("Failed to lock routes file")?;
        std::fs::write(&path, data).context("Failed to write routes file")?;
        file.unlock().context("Failed to unlock routes file")?;

        Ok(())
    }

    /// Load routes from disk (used on startup to recover state).
    pub async fn load_from_disk(&self) -> Result<usize> {
        let path = routes_file_path()?;
        if !path.exists() {
            return Ok(0);
        }

        let data = std::fs::read_to_string(&path)
            .context("Failed to read routes file")?;
        let persisted: Vec<PersistedRoute> = serde_json::from_str(&data)
            .context("Failed to parse routes file")?;

        let mut routes = self.routes.write().await;
        let mut loaded = 0;

        for pr in persisted {
            // Only load routes with alive PIDs (or no PID)
            if let Some(pid) = pr.pid {
                if !is_process_alive(pid) {
                    continue;
                }
            }

            routes.insert(
                pr.hostname.clone(),
                AppRoute {
                    hostname: pr.hostname,
                    target_port: pr.target_port,
                    pid: pr.pid,
                    app_name: pr.app_name,
                    registered_at: Instant::now(), // Approximate
                    token: None,
                },
            );
            loaded += 1;
        }

        info!("Loaded {} routes from disk", loaded);
        Ok(loaded)
    }
}

/// Get the path to ~/.hostless/routes.json
fn routes_file_path() -> Result<PathBuf> {
    let dir = crate::config::AppConfig::config_dir()?;
    Ok(dir.join("routes.json"))
}

/// Check if a process is alive by sending signal 0.
fn is_process_alive(pid: u32) -> bool {
    if pid == 0 {
        return false;
    }

    use nix::sys::signal;
    use nix::unistd::Pid;

    match signal::kill(Pid::from_raw(pid as i32), None) {
        Ok(_) => true,
        Err(nix::errno::Errno::EPERM) => true, // Process exists but we lack permission
        Err(_) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_register_and_lookup() {
        let table = RouteTable::new(11434);
        let route = table.register("myapp", 4001, Some(12345)).await.unwrap();

        assert_eq!(route.hostname, "myapp.localhost");
        assert_eq!(route.target_port, 4001);

        let found = table.lookup("myapp.localhost").await;
        assert!(found.is_some());
        assert_eq!(found.unwrap().target_port, 4001);
    }

    #[tokio::test]
    async fn test_remove_route() {
        let table = RouteTable::new(11434);
        table.register("myapp", 4001, None).await.unwrap();

        let removed = table.remove("myapp").await;
        assert!(removed.is_some());

        let found = table.lookup("myapp.localhost").await;
        assert!(found.is_none());
    }

    #[tokio::test]
    async fn test_lookup_with_wildcard_match() {
        let table = RouteTable::new(11434);
        table.register("myapp", 4001, None).await.unwrap();

        let found = table
            .lookup_with_wildcard("tenant.myapp.localhost", true)
            .await;
        assert!(found.is_some());
        assert_eq!(found.unwrap().target_port, 4001);
    }

    #[tokio::test]
    async fn test_lookup_with_wildcard_exact_precedence() {
        let table = RouteTable::new(11434);
        table.register("myapp", 4001, None).await.unwrap();
        table.register("tenant.myapp", 4002, None).await.unwrap();

        let found = table
            .lookup_with_wildcard("tenant.myapp.localhost", true)
            .await;
        assert!(found.is_some());
        assert_eq!(found.unwrap().target_port, 4002);
    }

    #[tokio::test]
    async fn test_lookup_with_wildcard_prefers_most_specific_suffix() {
        let table = RouteTable::new(11434);
        table.register("app", 4001, None).await.unwrap();
        table.register("myapp", 4002, None).await.unwrap();

        let found = table
            .lookup_with_wildcard("tenant.myapp.localhost", true)
            .await;
        assert!(found.is_some());
        assert_eq!(found.unwrap().target_port, 4002);
    }

    #[test]
    fn test_pid_zero_is_not_alive() {
        assert!(!is_process_alive(0));
    }

    #[tokio::test]
    async fn test_remove_by_hostname() {
        let table = RouteTable::new(11434);
        table.register("myapp", 4001, None).await.unwrap();

        let removed = table.remove("myapp.localhost").await;
        assert!(removed.is_some());
    }

    #[tokio::test]
    async fn test_list_routes() {
        let table = RouteTable::new(11434);
        table.register("app1", 4001, None).await.unwrap();
        table.register("app2", 4002, None).await.unwrap();

        let list = table.list().await;
        assert_eq!(list.len(), 2);
    }

    #[tokio::test]
    async fn test_cleanup_stale() {
        let table = RouteTable::new(11434);
        // PID 999999999 is almost certainly not alive
        table.register("dead-app", 4001, Some(999_999_999)).await.unwrap();
        table.register("no-pid", 4002, None).await.unwrap();

        let (removed, _tokens) = table.cleanup_stale().await;
        assert_eq!(removed, 1);

        let list = table.list().await;
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].app_name, "no-pid");
    }
}
