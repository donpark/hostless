use anyhow::{Context, Result};
use std::path::Path;

const HOSTS_PATH: &str = "/etc/hosts";
const MARKER_START: &str = "# hostless-start";
const MARKER_END: &str = "# hostless-end";

fn read_hosts(path: &Path) -> Result<String> {
    std::fs::read_to_string(path).with_context(|| format!("Failed to read {}", path.display()))
}

fn write_hosts(path: &Path, content: &str) -> Result<()> {
    std::fs::write(path, content).with_context(|| format!("Failed to write {}", path.display()))
}

#[allow(dead_code)]
pub fn extract_managed_block(content: &str) -> Vec<String> {
    let start = content.find(MARKER_START);
    let end = content.find(MARKER_END);
    let block = match (start, end) {
        (Some(s), Some(e)) if e > s => &content[s + MARKER_START.len()..e],
        _ => return Vec::new(),
    };

    block
        .lines()
        .map(|line| line.trim().to_string())
        .filter(|line| !line.is_empty())
        .collect()
}

pub fn remove_managed_block(content: &str) -> String {
    let start = content.find(MARKER_START);
    let end = content.find(MARKER_END);

    let without_block = match (start, end) {
        (Some(s), Some(e)) if e > s => {
            let end_idx = e + MARKER_END.len();
            format!("{}{}", &content[..s], &content[end_idx..])
        }
        _ => content.to_string(),
    };

    let normalized = without_block
        .replace("\r\n", "\n")
        .lines()
        .collect::<Vec<_>>()
        .join("\n");

    format!("{}\n", normalized.trim_end())
}

pub fn build_managed_block(hostnames: &[String]) -> String {
    if hostnames.is_empty() {
        return String::new();
    }

    let mut lines = vec![MARKER_START.to_string()];
    for hostname in hostnames {
        lines.push(format!("127.0.0.1 {}", hostname));
    }
    lines.push(MARKER_END.to_string());
    lines.join("\n")
}

pub fn sync_hosts_with_path(path: &Path, hostnames: &[String]) -> Result<()> {
    let content = read_hosts(path)?;
    let cleaned = remove_managed_block(&content);

    let mut deduped = hostnames.to_vec();
    deduped.sort();
    deduped.dedup();

    if deduped.is_empty() {
        return write_hosts(path, &cleaned);
    }

    let block = build_managed_block(&deduped);
    let merged = format!("{}\n{}\n", cleaned.trim_end(), block);
    write_hosts(path, &merged)
}

pub fn clean_hosts_with_path(path: &Path) -> Result<()> {
    let content = read_hosts(path)?;
    let cleaned = remove_managed_block(&content);
    write_hosts(path, &cleaned)
}

pub fn sync_hosts(hostnames: &[String]) -> Result<()> {
    let path = std::env::var("HOSTLESS_HOSTS_PATH")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| std::path::PathBuf::from(HOSTS_PATH));
    sync_hosts_with_path(&path, hostnames)
}

pub fn clean_hosts() -> Result<()> {
    let path = std::env::var("HOSTLESS_HOSTS_PATH")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| std::path::PathBuf::from(HOSTS_PATH));
    clean_hosts_with_path(&path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_hosts_path() -> std::path::PathBuf {
        std::env::temp_dir().join(format!("hostless-hosts-test-{}", rand::random::<u32>()))
    }

    #[test]
    fn test_remove_managed_block() {
        let input = "127.0.0.1 localhost\n# hostless-start\n127.0.0.1 app.localhost\n# hostless-end\n";
        let out = remove_managed_block(input);
        assert!(!out.contains("hostless-start"));
        assert!(out.contains("localhost"));
    }

    #[test]
    fn test_extract_managed_block_variants() {
        assert!(extract_managed_block("127.0.0.1 localhost\n").is_empty());
        assert!(extract_managed_block("# hostless-start\n127.0.0.1 a.localhost\n").is_empty());
        assert!(extract_managed_block("127.0.0.1 a.localhost\n# hostless-end\n").is_empty());

        let content = "127.0.0.1 localhost\n# hostless-start\n 127.0.0.1 a.localhost \n\n127.0.0.1 b.localhost\n# hostless-end\n";
        let extracted = extract_managed_block(content);
        assert_eq!(
            extracted,
            vec![
                "127.0.0.1 a.localhost".to_string(),
                "127.0.0.1 b.localhost".to_string()
            ]
        );
    }

    #[test]
    fn test_build_managed_block() {
        let out = build_managed_block(&["a.localhost".to_string(), "b.localhost".to_string()]);
        assert!(out.contains("# hostless-start"));
        assert!(out.contains("127.0.0.1 a.localhost"));
        assert!(out.contains("127.0.0.1 b.localhost"));
        assert!(out.contains("# hostless-end"));
    }

    #[test]
    fn test_sync_and_clean_hosts_with_path() {
        let path = temp_hosts_path();
        std::fs::write(&path, "127.0.0.1 localhost\n").unwrap();

        let hostnames = vec!["myapp.localhost".to_string(), "api.localhost".to_string()];
        sync_hosts_with_path(&path, &hostnames).unwrap();
        let synced = std::fs::read_to_string(&path).unwrap();
        assert!(synced.contains("# hostless-start"));
        assert!(synced.contains("myapp.localhost"));

        clean_hosts_with_path(&path).unwrap();
        let cleaned = std::fs::read_to_string(&path).unwrap();
        assert!(!cleaned.contains("# hostless-start"));
        assert!(cleaned.contains("localhost"));

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn test_remove_block_normalizes_newlines() {
        let input = "127.0.0.1 localhost\n\n\n# hostless-start\n127.0.0.1 x.localhost\n# hostless-end\n\n\nother\n";
        let out = remove_managed_block(input);
        assert!(!out.contains("hostless-start"));
        assert!(out.contains("localhost"));
        assert!(out.contains("other"));
    }

    #[test]
    fn test_build_and_extract_roundtrip() {
        let hostnames = vec!["a.localhost".to_string(), "b.localhost".to_string()];
        let block = build_managed_block(&hostnames);
        let extracted = extract_managed_block(&block);
        assert_eq!(
            extracted,
            vec![
                "127.0.0.1 a.localhost".to_string(),
                "127.0.0.1 b.localhost".to_string()
            ]
        );
    }
}
