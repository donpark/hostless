use std::net::SocketAddr;
use std::sync::Arc;

use anyhow::{Context, Result};
use tracing::info;

use crate::server::{self, AppState};

/// Serve with auto-generated TLS certificates on localhost.
pub async fn serve_tls(state: Arc<AppState>, addr: SocketAddr) -> Result<()> {
    let (cert_pem, key_pem) = generate_local_certs()?;

    let rustls_config = axum_server::tls_rustls::RustlsConfig::from_pem(
        cert_pem.into_bytes(),
        key_pem.into_bytes(),
    )
    .await
    .context("Failed to create TLS config")?;

    let app = server::create_router(state);

    info!("TLS certificates generated for localhost");
    info!("Note: You may need to trust the CA certificate for HTTPS to work in browsers.");

    axum_server::bind_rustls(addr, rustls_config)
        .serve(app.into_make_service())
        .await
        .context("TLS server failed")?;

    Ok(())
}

/// Generate self-signed TLS certificates for localhost using rcgen.
fn generate_local_certs() -> Result<(String, String)> {
    use rcgen::{CertificateParams, DnType, KeyPair, SanType};

    let mut params = CertificateParams::default();
    params
        .distinguished_name
        .push(DnType::CommonName, "Hostless Local CA");
    params
        .distinguished_name
        .push(DnType::OrganizationName, "Hostless");

    params.subject_alt_names = vec![
        SanType::DnsName("localhost".try_into().unwrap()),
        SanType::IpAddress(std::net::IpAddr::V4(std::net::Ipv4Addr::new(127, 0, 0, 1))),
    ];

    // Valid for 1 year
    params.not_before = rcgen::date_time_ymd(2024, 1, 1);
    params.not_after = rcgen::date_time_ymd(2027, 12, 31);

    let key_pair = KeyPair::generate().context("Failed to generate key pair")?;
    let cert = params
        .self_signed(&key_pair)
        .context("Failed to generate self-signed certificate")?;

    let cert_pem = cert.pem();
    let key_pem = key_pair.serialize_pem();

    // Optionally save to config dir for inspection
    let config_dir = crate::config::AppConfig::config_dir()?;
    let cert_path = config_dir.join("localhost.crt");
    let key_path = config_dir.join("localhost.key");

    std::fs::write(&cert_path, &cert_pem).ok();
    std::fs::write(&key_path, &key_pem).ok();

    info!("TLS cert saved to: {}", cert_path.display());

    Ok((cert_pem, key_pem))
}
