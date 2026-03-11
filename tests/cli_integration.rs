//! CLI integration tests extracted from proxy integration coverage.
//! These tests focus on end-to-end command behavior for run/alias/hosts flows.

use std::path::{Path, PathBuf};

use hostless::process::manager::find_available_port;

fn create_temp_home_dir() -> PathBuf {
    let path = std::env::temp_dir().join(format!("hostless-test-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&path).unwrap();
    path
}

fn resolve_hostless_bin() -> PathBuf {
    if let Ok(bin) = std::env::var("CARGO_BIN_EXE_hostless") {
        return PathBuf::from(bin);
    }

    let current_exe = std::env::current_exe().unwrap();
    let target_debug = current_exe
        .parent()
        .and_then(|p| p.parent())
        .expect("test binary should be under target/debug/deps");
    let fallback = target_debug.join("hostless");
    assert!(
        fallback.exists(),
        "hostless binary not found at {}",
        fallback.display()
    );
    fallback
}

async fn run_cli(bin: &Path, home: &Path, args: &[&str]) -> std::process::Output {
    let config_dir = home.join(".hostless");
    tokio::process::Command::new(bin)
        .env("HOME", home)
        .env("HOSTLESS_CONFIG_DIR", &config_dir)
        .args(args)
        .output()
        .await
        .unwrap()
}

async fn run_cli_with_env(
    bin: &Path,
    home: &Path,
    args: &[&str],
    extra_env: &[(&str, &str)],
) -> std::process::Output {
    let config_dir = home.join(".hostless");
    let mut cmd = tokio::process::Command::new(bin);
    cmd.env("HOME", home)
        .env("HOSTLESS_CONFIG_DIR", &config_dir)
        .args(args);
    for (k, v) in extra_env {
        cmd.env(k, v);
    }
    cmd.output().await.unwrap()
}

async fn wait_for_health(port: u16) {
    let client = reqwest::Client::new();
    for _ in 0..150 {
        if let Ok(resp) = client.get(format!("http://localhost:{}/health", port)).send().await {
            if resp.status().is_success() {
                return;
            }
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    panic!("daemon did not become healthy on port {}", port);
}

async fn wait_for_file(path: &Path, timeout_ms: u64) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(timeout_ms);
    while std::time::Instant::now() < deadline {
        if path.exists() {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    panic!("timed out waiting for file: {}", path.display());
}

/// Concurrent `hostless run` invocations on the same daemon port should both succeed,
/// even when the daemon is initially down.
#[tokio::test]
async fn test_run_concurrent_autostart_is_idempotent() {
    let bin = resolve_hostless_bin();
    let home = create_temp_home_dir();
    let daemon_port = find_available_port().unwrap();
    let daemon_port_arg = daemon_port.to_string();
    let args_a = [
        "run",
        "concurrent-a",
        "--daemon-port",
        daemon_port_arg.as_str(),
        "--",
        "true",
    ];
    let args_b = [
        "run",
        "concurrent-b",
        "--daemon-port",
        daemon_port_arg.as_str(),
        "--",
        "true",
    ];

    let run_a = run_cli(&bin, &home, &args_a);
    let run_b = run_cli(&bin, &home, &args_b);

    let (out_a, out_b) = tokio::join!(run_a, run_b);

    let _ = run_cli(&bin, &home, &["stop"]).await;
    let _ = std::fs::remove_dir_all(&home);

    assert!(
        out_a.status.success(),
        "first run failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&out_a.stdout),
        String::from_utf8_lossy(&out_a.stderr)
    );
    assert!(
        out_b.status.success(),
        "second run failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&out_b.stdout),
        String::from_utf8_lossy(&out_b.stderr)
    );
}

#[tokio::test]
async fn test_run_accepts_infer_name_flag_smoke() {
    let bin = resolve_hostless_bin();
    let home = create_temp_home_dir();
    let daemon_port = find_available_port().unwrap();

    let out = run_cli(
        &bin,
        &home,
        &[
            "run",
            "smoke-infer",
            "--infer-name",
            "--daemon-port",
            &daemon_port.to_string(),
            "--",
            "true",
        ],
    )
    .await;

    let _ = run_cli(&bin, &home, &["stop"]).await;
    let _ = std::fs::remove_dir_all(&home);

    assert!(
        out.status.success(),
        "infer-name smoke run failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}

#[tokio::test]
async fn test_alias_add_list_remove() {
    let bin = resolve_hostless_bin();
    let home = create_temp_home_dir();
    let daemon_port = find_available_port().unwrap();

    let serve = run_cli(
        &bin,
        &home,
        &["proxy", "start", "--port", &daemon_port.to_string()],
    )
    .await;
    assert!(
        serve.status.success(),
        "proxy start failed: {}",
        String::from_utf8_lossy(&serve.stderr)
    );
    wait_for_health(daemon_port).await;

    let add = run_cli(
        &bin,
        &home,
        &["alias", "dockersvc", "4011", "--daemon-port", &daemon_port.to_string()],
    )
    .await;
    assert!(
        add.status.success(),
        "alias add (compat) failed: {}",
        String::from_utf8_lossy(&add.stderr)
    );

    let list = run_cli(
        &bin,
        &home,
        &["list", "--daemon-port", &daemon_port.to_string()],
    )
    .await;
    assert!(
        list.status.success(),
        "list (compat) failed: {}",
        String::from_utf8_lossy(&list.stderr)
    );
    let list_out = String::from_utf8_lossy(&list.stdout);
    assert!(list_out.contains("dockersvc.localhost"));

    let remove = run_cli(
        &bin,
        &home,
        &["alias", "--remove", "dockersvc", "--daemon-port", &daemon_port.to_string()],
    )
    .await;
    assert!(
        remove.status.success(),
        "alias remove (compat) failed: {}",
        String::from_utf8_lossy(&remove.stderr)
    );

    let _ = run_cli(&bin, &home, &["proxy", "stop"]).await;
    let _ = std::fs::remove_dir_all(&home);
}

#[tokio::test]
async fn test_hosts_sync_and_clean_cli_with_override_path() {
    let bin = resolve_hostless_bin();
    let home = create_temp_home_dir();
    let config_dir = home.join(".hostless");
    std::fs::create_dir_all(&config_dir).unwrap();

    let routes = serde_json::json!([
        {
            "hostname": "myapp.localhost",
            "target_port": 4001,
            "pid": null,
            "app_name": "myapp",
            "registered_at": 0
        },
        {
            "hostname": "api.localhost",
            "target_port": 4002,
            "pid": null,
            "app_name": "api",
            "registered_at": 0
        }
    ]);
    std::fs::write(config_dir.join("routes.json"), serde_json::to_string_pretty(&routes).unwrap()).unwrap();

    let hosts_path = std::env::temp_dir().join(format!("hostless-hosts-cli-{}", uuid::Uuid::new_v4()));
    std::fs::write(&hosts_path, "127.0.0.1 localhost\n").unwrap();
    let hosts_path_owned = hosts_path.to_string_lossy().to_string();

    let sync = run_cli_with_env(
        &bin,
        &home,
        &["hosts", "sync"],
        &[("HOSTLESS_HOSTS_PATH", hosts_path_owned.as_str())],
    )
    .await;
    assert!(sync.status.success(), "hosts sync failed: {}", String::from_utf8_lossy(&sync.stderr));
    let synced = std::fs::read_to_string(&hosts_path).unwrap();
    assert!(synced.contains("# hostless-start"));
    assert!(synced.contains("myapp.localhost"));
    assert!(synced.contains("api.localhost"));

    let clean = run_cli_with_env(
        &bin,
        &home,
        &["hosts", "clean"],
        &[("HOSTLESS_HOSTS_PATH", hosts_path_owned.as_str())],
    )
    .await;
    assert!(clean.status.success(), "hosts clean failed: {}", String::from_utf8_lossy(&clean.stderr));
    let cleaned = std::fs::read_to_string(&hosts_path).unwrap();
    assert!(!cleaned.contains("# hostless-start"));
    assert!(cleaned.contains("localhost"));

    let _ = std::fs::remove_file(&hosts_path);
    let _ = std::fs::remove_dir_all(&home);
}

#[tokio::test]
async fn test_proxy_start_stop_compat() {
    let bin = resolve_hostless_bin();
    let home = create_temp_home_dir();
    let daemon_port = find_available_port().unwrap();

    let start = run_cli(
        &bin,
        &home,
        &["proxy", "start", "--port", &daemon_port.to_string()],
    )
    .await;
    assert!(
        start.status.success(),
        "proxy start failed: {}",
        String::from_utf8_lossy(&start.stderr)
    );
    wait_for_health(daemon_port).await;

    let stop = run_cli(&bin, &home, &["proxy", "stop"]).await;
    assert!(
        stop.status.success(),
        "proxy stop failed: {}",
        String::from_utf8_lossy(&stop.stderr)
    );

    let _ = std::fs::remove_dir_all(&home);
}

#[tokio::test]
async fn test_top_level_shorthand_run_with_bypass() {
    let bin = resolve_hostless_bin();
    let home = create_temp_home_dir();

    let out = run_cli_with_env(&bin, &home, &["myapp", "true"], &[("HOSTLESS", "0")]).await;
    let stdout = String::from_utf8_lossy(&out.stdout);

    let _ = std::fs::remove_dir_all(&home);

    assert!(
        out.status.success(),
        "top-level shorthand run failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(stdout.contains("HOSTLESS=0 set"));
}

#[tokio::test]
async fn test_config_token_persistence_roundtrip() {
    let bin = resolve_hostless_bin();
    let home = create_temp_home_dir();

    let set_file = run_cli(
        &bin,
        &home,
        &["config", "set-token-persistence", "file"],
    )
    .await;
    assert!(
        set_file.status.success(),
        "config set-token-persistence file failed: {}",
        String::from_utf8_lossy(&set_file.stderr)
    );

    let list_file = run_cli(&bin, &home, &["config", "list"]).await;
    assert!(
        list_file.status.success(),
        "config list after set failed: {}",
        String::from_utf8_lossy(&list_file.stderr)
    );
    let list_file_stdout = String::from_utf8_lossy(&list_file.stdout);
    assert!(
        list_file_stdout.contains("token_persistence: file"),
        "unexpected config list output: {}",
        list_file_stdout
    );

    let set_off = run_cli(
        &bin,
        &home,
        &["config", "set-token-persistence", "off"],
    )
    .await;
    assert!(
        set_off.status.success(),
        "config set-token-persistence off failed: {}",
        String::from_utf8_lossy(&set_off.stderr)
    );

    let list_off = run_cli(&bin, &home, &["config", "list"]).await;
    assert!(
        list_off.status.success(),
        "config list after reset failed: {}",
        String::from_utf8_lossy(&list_off.stderr)
    );
    let list_off_stdout = String::from_utf8_lossy(&list_off.stdout);
    assert!(
        list_off_stdout.contains("token_persistence: off"),
        "unexpected config list output after reset: {}",
        list_off_stdout
    );

    let _ = std::fs::remove_dir_all(&home);
}

#[tokio::test]
async fn test_proxy_start_honors_config_token_persistence_default() {
    let bin = resolve_hostless_bin();
    let home = create_temp_home_dir();
    let daemon_port = find_available_port().unwrap();

    let set_file = run_cli(
        &bin,
        &home,
        &["config", "set-token-persistence", "file"],
    )
    .await;
    assert!(
        set_file.status.success(),
        "config set-token-persistence file failed: {}",
        String::from_utf8_lossy(&set_file.stderr)
    );

    let start = run_cli(
        &bin,
        &home,
        &["proxy", "start", "--port", &daemon_port.to_string()],
    )
    .await;
    assert!(
        start.status.success(),
        "proxy start failed: {}",
        String::from_utf8_lossy(&start.stderr)
    );
    wait_for_health(daemon_port).await;

    let admin_token_path = home.join(".hostless").join("admin.token");
    wait_for_file(&admin_token_path, 5_000).await;
    let admin_token = std::fs::read_to_string(admin_token_path)
        .unwrap()
        .trim()
        .to_string();

    let client = reqwest::Client::new();
    let create_resp = client
        .post(format!("http://localhost:{}/auth/token", daemon_port))
        .header("x-hostless-admin", admin_token)
        .json(&serde_json::json!({
            "origin": "*",
            "name": "proxy-config-default",
            "ttl": 3600
        }))
        .send()
        .await
        .unwrap();
    assert!(
        create_resp.status().is_success(),
        "token create via daemon API failed: {}",
        create_resp.status()
    );

    // Give persistence a moment to flush to disk.
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    let tokens_path = home.join(".hostless").join("tokens.json");
    assert!(
        tokens_path.exists(),
        "expected tokens.json to exist when proxy uses config default 'file' mode"
    );

    let _ = run_cli(&bin, &home, &["proxy", "stop"]).await;
    let _ = std::fs::remove_dir_all(&home);
}

#[tokio::test]
async fn test_proxy_start_cli_override_beats_config_default() {
    let bin = resolve_hostless_bin();
    let home = create_temp_home_dir();
    let daemon_port = find_available_port().unwrap();

    let set_file = run_cli(
        &bin,
        &home,
        &["config", "set-token-persistence", "file"],
    )
    .await;
    assert!(
        set_file.status.success(),
        "config set-token-persistence file failed: {}",
        String::from_utf8_lossy(&set_file.stderr)
    );

    let start = run_cli(
        &bin,
        &home,
        &[
            "proxy",
            "start",
            "--port",
            &daemon_port.to_string(),
            "--token-persistence",
            "off",
        ],
    )
    .await;
    assert!(
        start.status.success(),
        "proxy start failed: {}",
        String::from_utf8_lossy(&start.stderr)
    );
    wait_for_health(daemon_port).await;

    let admin_token_path = home.join(".hostless").join("admin.token");
    wait_for_file(&admin_token_path, 5_000).await;
    let admin_token = std::fs::read_to_string(admin_token_path)
        .unwrap()
        .trim()
        .to_string();

    let client = reqwest::Client::new();
    let create_resp = client
        .post(format!("http://localhost:{}/auth/token", daemon_port))
        .header("x-hostless-admin", admin_token)
        .json(&serde_json::json!({
            "origin": "*",
            "name": "proxy-cli-override",
            "ttl": 3600
        }))
        .send()
        .await
        .unwrap();
    assert!(
        create_resp.status().is_success(),
        "token create via daemon API failed: {}",
        create_resp.status()
    );

    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    let tokens_path = home.join(".hostless").join("tokens.json");
    assert!(
        !tokens_path.exists(),
        "did not expect tokens.json when proxy start override uses 'off' mode"
    );

    let _ = run_cli(&bin, &home, &["proxy", "stop"]).await;
    let _ = std::fs::remove_dir_all(&home);
}

#[tokio::test]
async fn test_token_cli_uses_active_daemon_port() {
    let bin = resolve_hostless_bin();
    let home = create_temp_home_dir();
    let daemon_port = find_available_port().unwrap();

    let start = run_cli(
        &bin,
        &home,
        &["proxy", "start", "--port", &daemon_port.to_string()],
    )
    .await;
    assert!(
        start.status.success(),
        "proxy start failed: {}",
        String::from_utf8_lossy(&start.stderr)
    );
    wait_for_health(daemon_port).await;

    let create = run_cli(
        &bin,
        &home,
        &["token", "create", "--name", "cli-port-check", "--ttl", "3600"],
    )
    .await;
    assert!(
        create.status.success(),
        "token create failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&create.stdout),
        String::from_utf8_lossy(&create.stderr)
    );

    let create_stdout = String::from_utf8_lossy(&create.stdout);
    let token_line = create_stdout
        .lines()
        .find(|line| line.trim_start().starts_with("Token:"))
        .expect("token output should include Token line");
    let token = token_line
        .split_once(':')
        .map(|(_, value)| value.trim().to_string())
        .expect("Token line should contain ':'");
    assert!(token.starts_with("sk_local_"));

    let list = run_cli(&bin, &home, &["token", "list"]).await;
    assert!(
        list.status.success(),
        "token list failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&list.stdout),
        String::from_utf8_lossy(&list.stderr)
    );

    let list_stdout = String::from_utf8_lossy(&list.stdout);
    assert!(
        list_stdout.contains("cli-port-check"),
        "token list output should include created token name\nstdout:\n{}",
        list_stdout
    );

    let revoke = run_cli(&bin, &home, &["token", "revoke", &token]).await;
    assert!(
        revoke.status.success(),
        "token revoke failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&revoke.stdout),
        String::from_utf8_lossy(&revoke.stderr)
    );

    let list_after_revoke = run_cli(&bin, &home, &["token", "list"]).await;
    assert!(
        list_after_revoke.status.success(),
        "token list after revoke failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&list_after_revoke.stdout),
        String::from_utf8_lossy(&list_after_revoke.stderr)
    );
    let list_after_revoke_stdout = String::from_utf8_lossy(&list_after_revoke.stdout);
    assert!(
        !list_after_revoke_stdout.contains("cli-port-check"),
        "revoked token should no longer appear in token list\nstdout:\n{}",
        list_after_revoke_stdout
    );

    let _ = run_cli(&bin, &home, &["proxy", "stop"]).await;
    let _ = std::fs::remove_dir_all(&home);
}
