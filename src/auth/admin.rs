use std::path::PathBuf;

use anyhow::{Context, Result};
use rand::RngCore;

pub const ADMIN_HEADER: &str = "x-hostless-admin";

fn admin_token_path() -> Result<PathBuf> {
    Ok(crate::config::AppConfig::config_dir()?.join("admin.token"))
}

pub fn load_or_create_admin_token() -> Result<String> {
    let path = admin_token_path()?;

    if path.exists() {
        let token = std::fs::read_to_string(&path)
            .context("Failed to read admin token file")?
            .trim()
            .to_string();
        if !token.is_empty() {
            return Ok(token);
        }
    }

    let mut bytes = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut bytes);
    let token = bytes
        .iter()
        .map(|b| format!("{:02x}", b))
        .collect::<String>();

    match std::fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&path)
    {
        Ok(mut file) => {
            use std::io::Write;
            file.write_all(token.as_bytes())
                .context("Failed to write admin token file")?;
            file.flush().context("Failed to flush admin token file")?;
        }
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            let existing = std::fs::read_to_string(&path)
                .context("Failed to read existing admin token file")?
                .trim()
                .to_string();
            if existing.is_empty() {
                anyhow::bail!(
                    "Admin token file exists but is empty. Restart the daemon and retry."
                );
            }
            return Ok(existing);
        }
        Err(e) => {
            return Err(e).context("Failed to create admin token file");
        }
    }

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let perms = std::fs::Permissions::from_mode(0o600);
        std::fs::set_permissions(&path, perms)
            .context("Failed to set admin token file permissions")?;
    }

    Ok(token)
}

pub fn load_admin_token() -> Result<String> {
    let path = admin_token_path()?;
    if !path.exists() {
        anyhow::bail!(
            "Admin token file not found. Start the daemon with 'hostless serve' first."
        );
    }

    let token = std::fs::read_to_string(&path)
        .context("Failed to read admin token file")?
        .trim()
        .to_string();

    if token.is_empty() {
        anyhow::bail!(
            "Admin token file is empty. Restart the daemon with 'hostless serve'."
        );
    }

    Ok(token)
}
