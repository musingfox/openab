use anyhow::{anyhow, Result};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

const REFRESH_SKEW_SECONDS: u64 = 120;

// OpenAI/Codex OAuth constants (public client, same as official Codex CLI)
const CODEX_CLIENT_ID: &str = "app_scp_codex_prod_001";
const CODEX_DEVICE_AUTH_URL: &str = "https://auth.openai.com/oauth/device/code";
const CODEX_TOKEN_URL: &str = "https://auth.openai.com/oauth/token";
const CODEX_SCOPES: &str = "openid profile email offline_access";

/// Stored OAuth credentials.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TokenStore {
    pub access_token: String,
    pub refresh_token: String,
    pub expires_at: u64,
    pub token_endpoint: String,
    pub provider: String,
}

/// Path to the auth file: ~/.openab/agent/auth.json
fn auth_path() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
    PathBuf::from(home)
        .join(".openab")
        .join("agent")
        .join("auth.json")
}

/// Load stored tokens from disk.
pub fn load_tokens() -> Result<TokenStore> {
    let path = auth_path();
    let data = std::fs::read_to_string(&path).map_err(|_| {
        anyhow!(
            "No credentials found at {}. Run `openab-agent auth codex-oauth` first.",
            path.display()
        )
    })?;
    serde_json::from_str(&data).map_err(|e| anyhow!("Invalid auth.json: {e}"))
}

/// Save tokens to disk atomically with 0600 permissions.
fn save_tokens(store: &TokenStore) -> Result<()> {
    let path = auth_path();
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let data = serde_json::to_string_pretty(store)?;

    #[cfg(unix)]
    {
        use std::fs::OpenOptions;
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let mut file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&path)?;
        file.write_all(data.as_bytes())?;
    }
    #[cfg(not(unix))]
    {
        std::fs::write(&path, &data)?;
    }
    Ok(())
}

/// Check if token is expired (with skew).
fn is_expired(store: &TokenStore) -> bool {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    now + REFRESH_SKEW_SECONDS >= store.expires_at
}

/// Get a valid access token, refreshing if needed.
pub async fn get_valid_token() -> Result<String> {
    let mut store = load_tokens()?;
    if is_expired(&store) {
        store = refresh_token(&store).await?;
        save_tokens(&store)?;
    }
    Ok(store.access_token)
}

/// Refresh the access token using the refresh_token grant.
async fn refresh_token(store: &TokenStore) -> Result<TokenStore> {
    let client = reqwest::Client::new();
    let resp = client
        .post(&store.token_endpoint)
        .form(&[
            ("grant_type", "refresh_token"),
            ("refresh_token", store.refresh_token.as_str()),
            ("client_id", CODEX_CLIENT_ID),
        ])
        .send()
        .await?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return Err(anyhow!(
            "Token refresh failed (HTTP {status}): {body}. Run `openab-agent auth codex-oauth` again."
        ));
    }

    let payload: serde_json::Value = resp.json().await?;
    let access_token = payload["access_token"]
        .as_str()
        .ok_or_else(|| anyhow!("No access_token in refresh response"))?;
    let new_refresh = payload["refresh_token"]
        .as_str()
        .unwrap_or(&store.refresh_token);
    let expires_in = payload["expires_in"].as_u64().unwrap_or(3600);
    let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();

    Ok(TokenStore {
        access_token: access_token.to_string(),
        refresh_token: new_refresh.to_string(),
        expires_at: now + expires_in,
        token_endpoint: store.token_endpoint.clone(),
        provider: store.provider.clone(),
    })
}

/// Run the OpenAI/Codex device flow login.
pub async fn login_codex_device_flow() -> Result<()> {
    println!("Starting OpenAI Codex device-code login...\n");

    let client = reqwest::Client::new();

    // Step 1: Request device code
    let resp = client
        .post(CODEX_DEVICE_AUTH_URL)
        .form(&[("client_id", CODEX_CLIENT_ID), ("scope", CODEX_SCOPES)])
        .send()
        .await?;

    if !resp.status().is_success() {
        let body = resp.text().await.unwrap_or_default();
        return Err(anyhow!("Device authorization request failed: {body}"));
    }

    let device_resp: serde_json::Value = resp.json().await?;
    let device_code = device_resp["device_code"]
        .as_str()
        .ok_or_else(|| anyhow!("No device_code in response"))?;
    let user_code = device_resp["user_code"]
        .as_str()
        .ok_or_else(|| anyhow!("No user_code in response"))?;
    let verification_uri = device_resp["verification_uri"]
        .as_str()
        .or_else(|| device_resp["verification_url"].as_str())
        .unwrap_or("https://auth.openai.com/activate");
    let interval = device_resp["interval"].as_u64().unwrap_or(5);

    println!("  Go to:      {}", verification_uri);
    println!("  Enter code: {}\n", user_code);
    println!("Waiting for authorization...");

    // Step 2: Poll for token
    loop {
        tokio::time::sleep(tokio::time::Duration::from_secs(interval)).await;

        let resp = client
            .post(CODEX_TOKEN_URL)
            .form(&[
                ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
                ("client_id", CODEX_CLIENT_ID),
                ("device_code", device_code),
            ])
            .send()
            .await?;

        let status = resp.status();
        let payload: serde_json::Value = resp.json().await?;

        if status.is_success() {
            let access_token = payload["access_token"]
                .as_str()
                .ok_or_else(|| anyhow!("No access_token"))?;
            let refresh_token = payload["refresh_token"]
                .as_str()
                .ok_or_else(|| anyhow!("No refresh_token"))?;
            let expires_in = payload["expires_in"].as_u64().unwrap_or(3600);
            let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();

            let store = TokenStore {
                access_token: access_token.to_string(),
                refresh_token: refresh_token.to_string(),
                expires_at: now + expires_in,
                token_endpoint: CODEX_TOKEN_URL.to_string(),
                provider: "codex".to_string(),
            };
            save_tokens(&store)?;
            println!("\n✅ Login successful! Token saved to {:?}", auth_path());
            return Ok(());
        }

        match payload["error"].as_str().unwrap_or_default() {
            "authorization_pending" | "slow_down" => continue,
            "expired_token" => return Err(anyhow!("Device code expired. Please try again.")),
            "access_denied" => return Err(anyhow!("Authorization denied by user.")),
            e => return Err(anyhow!("Device-code error: {e} — {payload}")),
        }
    }
}

/// Show current auth status.
pub fn show_status() {
    match load_tokens() {
        Ok(store) => {
            let now = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs();
            let expired = now + REFRESH_SKEW_SECONDS >= store.expires_at;
            let masked = if store.access_token.len() > 12 {
                format!(
                    "{}...{}",
                    &store.access_token[..8],
                    &store.access_token[store.access_token.len() - 4..]
                )
            } else {
                "****".to_string()
            };
            println!("Provider:  {}", store.provider);
            println!("Token:     {}", masked);
            println!(
                "Expires:   {} ({})",
                store.expires_at,
                if expired {
                    "EXPIRED — will refresh on next use"
                } else {
                    "valid"
                }
            );
            println!("File:      {:?}", auth_path());
        }
        Err(e) => {
            println!("Not authenticated: {e}");
            println!("\nRun: openab-agent auth codex-oauth");
        }
    }
}
