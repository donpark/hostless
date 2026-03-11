use anyhow::{Context, Result};
use oauth2::{
    basic::BasicClient, AuthUrl, AuthorizationCode, ClientId, ClientSecret, CsrfToken,
    PkceCodeChallenge, RedirectUrl, Scope, TokenResponse, TokenUrl,
};
use oauth2::reqwest;
use tracing::info;

use crate::config::AppConfig;

/// Known OAuth provider configurations
struct OAuthProviderConfig {
    auth_url: &'static str,
    token_url: &'static str,
    default_scopes: Vec<&'static str>,
}

fn get_provider_config(provider: &str) -> Option<OAuthProviderConfig> {
    match provider {
        "openrouter" => Some(OAuthProviderConfig {
            auth_url: "https://openrouter.ai/auth/authorize",
            token_url: "https://openrouter.ai/auth/token",
            default_scopes: vec!["openid", "profile"],
        }),
        _ => None,
    }
}

/// Start an interactive OAuth login flow.
///
/// 1. Opens the browser to the provider's authorize endpoint
/// 2. Listens on a local port for the callback
/// 3. Exchanges the code for a token
/// 4. Stores the token in the vault
pub async fn start_oauth_login(provider: &str) -> Result<()> {
    let config = AppConfig::load()?;

    // Check if there's a custom OAuth config for this provider
    let (client_id, client_secret, auth_url, token_url, scopes) =
        if let Some(custom) = config.oauth_clients.get(provider) {
            (
                custom.client_id.clone(),
                custom.client_secret.clone(),
                custom.auth_url.clone(),
                custom.token_url.clone(),
                custom.scopes.clone(),
            )
        } else if let Some(builtin) = get_provider_config(provider) {
            // For built-in providers, we need the user to have set up client credentials
            let client_id = std::env::var(format!(
                "{}_CLIENT_ID",
                provider.to_uppercase()
            ))
            .context(format!(
                "Set {}_CLIENT_ID environment variable or configure OAuth in ~/.hostless/config.json",
                provider.to_uppercase()
            ))?;

            let client_secret = std::env::var(format!(
                "{}_CLIENT_SECRET",
                provider.to_uppercase()
            ))
            .ok();

            (
                client_id,
                client_secret,
                builtin.auth_url.to_string(),
                builtin.token_url.to_string(),
                builtin
                    .default_scopes
                    .iter()
                    .map(|s| s.to_string())
                    .collect(),
            )
        } else {
            anyhow::bail!(
                "Unknown OAuth provider '{}'. Configure it in ~/.hostless/config.json under 'oauth_clients'.",
                provider
            );
        };

    // Find a free port for the callback listener
    let callback_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let callback_port = callback_listener.local_addr()?.port();
    let redirect_uri = format!("http://localhost:{}/callback", callback_port);

    info!("Starting OAuth flow for '{}'", provider);
    info!("Callback listener on port {}", callback_port);

    let mut client = BasicClient::new(ClientId::new(client_id))
        .set_auth_uri(AuthUrl::new(auth_url)?)
        .set_token_uri(TokenUrl::new(token_url)?)
        .set_redirect_uri(RedirectUrl::new(redirect_uri)?);

    if let Some(client_secret) = client_secret {
        client = client.set_client_secret(ClientSecret::new(client_secret));
    }

    // Generate PKCE challenge
    let (pkce_challenge, pkce_verifier) = PkceCodeChallenge::new_random_sha256();

    // Build authorization URL
    let mut auth_request = client
        .authorize_url(CsrfToken::new_random)
        .set_pkce_challenge(pkce_challenge);

    for scope in &scopes {
        auth_request = auth_request.add_scope(Scope::new(scope.clone()));
    }

    let (authorize_url, csrf_state) = auth_request.url();

    // Open browser
    info!("Opening browser for authorization...");
    open::that(authorize_url.as_str()).context("Failed to open browser")?;

    println!("Waiting for authorization callback...");
    println!("(If the browser didn't open, visit: {})", authorize_url);

    // Wait for the callback
    let (code, returned_state) =
        wait_for_callback(callback_listener).await?;

    // Verify CSRF state
    if returned_state != csrf_state.secret().as_str() {
        anyhow::bail!("CSRF state mismatch! Possible attack.");
    }

    info!("Received authorization code, exchanging for token...");

    let http_client = reqwest::ClientBuilder::new()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .context("Failed to build OAuth HTTP client")?;

    // Exchange code for token using the workspace's reqwest client.
    let token_response = client
        .exchange_code(AuthorizationCode::new(code))
        .set_pkce_verifier(pkce_verifier)
        .request_async(&http_client)
        .await
        .map_err(|e| anyhow::anyhow!("Token exchange failed: {}", e))?;

    let access_token = token_response.access_token().secret().clone();

    // Store in vault
    let vault = crate::vault::VaultStore::open().await?;
    vault.add_key(provider, &access_token, None).await?;

    println!("✓ OAuth login successful for '{}'!", provider);
    println!("  Access token stored securely in vault.");

    if let Some(expires_in) = token_response.expires_in() {
        println!("  Token expires in {:?}.", expires_in);
    }

    Ok(())
}

/// Wait for the OAuth callback on the temporary listener.
/// Returns (code, state).
async fn wait_for_callback(
    listener: tokio::net::TcpListener,
) -> Result<(String, String)> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let (mut stream, _) = listener
        .accept()
        .await
        .context("Failed to accept callback connection")?;

    let mut buf = vec![0u8; 4096];
    let n = stream
        .read(&mut buf)
        .await
        .context("Failed to read callback request")?;

    let request = String::from_utf8_lossy(&buf[..n]);

    // Parse the GET request to extract query parameters
    let first_line = request
        .lines()
        .next()
        .context("Empty callback request")?;

    let path = first_line
        .split_whitespace()
        .nth(1)
        .context("Invalid HTTP request")?;

    let url = url::Url::parse(&format!("http://localhost{}", path))
        .context("Failed to parse callback URL")?;

    let code = url
        .query_pairs()
        .find(|(k, _)| k == "code")
        .map(|(_, v)| v.to_string())
        .context("Missing 'code' in callback")?;

    let state = url
        .query_pairs()
        .find(|(k, _)| k == "state")
        .map(|(_, v)| v.to_string())
        .unwrap_or_default();

    // Send a response to the browser
    let response = "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\n\r\n\
        <!DOCTYPE html><html><head><title>Hostless</title></head>\
        <body style='font-family:system-ui;text-align:center;padding:2em'>\
        <h1>✓ Authorization Successful</h1>\
        <p>You can close this window and return to the terminal.</p>\
        </body></html>";

    stream
        .write_all(response.as_bytes())
        .await
        .context("Failed to send callback response")?;

    Ok((code, state))
}
