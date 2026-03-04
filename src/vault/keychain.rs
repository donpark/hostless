use anyhow::{Context, Result};

const SERVICE_NAME: &str = "hostless";
const ACCOUNT_NAME: &str = "master-key";

/// Try loading an existing master key from OS keychain without creating one
/// and without password fallback prompts.
pub fn try_load_existing_master_key() -> Result<Option<[u8; 32]>> {
    let entry = keyring::Entry::new(SERVICE_NAME, ACCOUNT_NAME)
        .context("Failed to create keychain entry")?;

    match entry.get_password() {
        Ok(stored) => {
            let bytes = hex_decode(&stored)?;
            if bytes.len() != 32 {
                anyhow::bail!("Stored master key has wrong length: {}", bytes.len());
            }
            let mut key = [0u8; 32];
            key.copy_from_slice(&bytes);
            Ok(Some(key))
        }
        Err(keyring::Error::NoEntry) => Ok(None),
        Err(e) => Err(anyhow::anyhow!("Keychain error: {}", e)),
    }
}

#[cfg(test)]
fn hex_encode(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{:02x}", b)).collect()
}

fn hex_decode(hex: &str) -> Result<Vec<u8>> {
    if hex.len() % 2 != 0 {
        anyhow::bail!("Invalid hex string length");
    }
    (0..hex.len())
        .step_by(2)
        .map(|i| {
            u8::from_str_radix(&hex[i..i + 2], 16)
                .context("Invalid hex character")
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_hex_roundtrip() {
        let original = [0xDE, 0xAD, 0xBE, 0xEF, 0x42];
        let encoded = hex_encode(&original);
        let decoded = hex_decode(&encoded).unwrap();
        assert_eq!(decoded, original);
    }
}
