use aes_gcm::{
    aead::{Aead, KeyInit},
    Aes256Gcm, Nonce,
};
use anyhow::{Context, Result};
use base64::{engine::general_purpose::STANDARD as BASE64, Engine};
use rand::RngCore;

/// AES-256-GCM encryption for vault data.
///
/// Encrypted format: base64(nonce[12] || ciphertext || tag[16])

const NONCE_LEN: usize = 12;

/// Encrypt plaintext with a 256-bit key. Returns base64-encoded (nonce || ciphertext || tag).
#[allow(dead_code)]
pub fn encrypt(key: &[u8; 32], plaintext: &[u8]) -> Result<String> {
    let cipher = Aes256Gcm::new(key.into());

    let mut nonce_bytes = [0u8; NONCE_LEN];
    rand::thread_rng().fill_bytes(&mut nonce_bytes);
    let nonce = Nonce::from_slice(&nonce_bytes);

    let ciphertext = cipher
        .encrypt(nonce, plaintext)
        .map_err(|e| anyhow::anyhow!("Encryption failed: {}", e))?;

    // Concatenate nonce + ciphertext (which includes the GCM tag)
    let mut blob = Vec::with_capacity(NONCE_LEN + ciphertext.len());
    blob.extend_from_slice(&nonce_bytes);
    blob.extend_from_slice(&ciphertext);

    Ok(BASE64.encode(&blob))
}

/// Decrypt base64-encoded (nonce || ciphertext || tag) with a 256-bit key.
pub fn decrypt(key: &[u8; 32], encoded: &str) -> Result<Vec<u8>> {
    let blob = BASE64
        .decode(encoded)
        .context("Failed to decode base64 vault data")?;

    if blob.len() < NONCE_LEN + 16 {
        anyhow::bail!("Encrypted data too short");
    }

    let (nonce_bytes, ciphertext) = blob.split_at(NONCE_LEN);
    let nonce = Nonce::from_slice(nonce_bytes);
    let cipher = Aes256Gcm::new(key.into());

    let plaintext = cipher
        .decrypt(nonce, ciphertext)
        .map_err(|e| anyhow::anyhow!("Decryption failed (wrong key?): {}", e))?;

    Ok(plaintext)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_encrypt_decrypt_roundtrip() {
        let key = [42u8; 32];
        let plaintext = b"sk-test-1234567890abcdef";

        let encrypted = encrypt(&key, plaintext).unwrap();
        let decrypted = decrypt(&key, &encrypted).unwrap();

        assert_eq!(decrypted, plaintext);
    }

    #[test]
    fn test_wrong_key_fails() {
        let key1 = [42u8; 32];
        let key2 = [99u8; 32];
        let plaintext = b"secret-key";

        let encrypted = encrypt(&key1, plaintext).unwrap();
        assert!(decrypt(&key2, &encrypted).is_err());
    }

    #[test]
    fn test_tampered_data_fails() {
        let key = [42u8; 32];
        let plaintext = b"secret-key";

        let encrypted = encrypt(&key, plaintext).unwrap();
        let mut blob = BASE64.decode(&encrypted).unwrap();
        // Tamper with a byte
        if let Some(byte) = blob.last_mut() {
            *byte ^= 0xff;
        }
        let tampered = BASE64.encode(&blob);
        assert!(decrypt(&key, &tampered).is_err());
    }
}
