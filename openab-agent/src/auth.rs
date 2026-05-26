use anyhow::{anyhow, Result};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

const REFRESH_SKEW_SECONDS: u64 = 120;

// OpenAI/Codex OAuth constants (public client, same as official Codex CLI)
// Configurable via env var if user has their own OAuth app registration
const CODEX_DEVICE_AUTH_URL: &str = "https://auth.openai.com/api/accounts/deviceauth/usercode";
const CODEX_TOKEN_URL: &str = "https://auth.openai.com/api/accounts/deviceauth/token";
const CODEX_SCOPES: &str = "openid offline_access";
const CODEX_AUDIENCE: &str = "https://api.openai.com/v1";

fn codex_client_id() -> String {
    std::env::var("OPENAB_AGENT_OAUTH_CLIENT_ID")
        .unwrap_or_else(|_| "app_EMoamEEZ73f0CkXaXp7hrann".to_string())
}

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

/// Force-refresh the token regardless of expiry (for 401 recovery).
pub async fn force_refresh() -> Result<String> {
    let store = load_tokens()?;
    let new_store = refresh_token(&store).await?;
    save_tokens(&new_store)?;
    Ok(new_store.access_token)
}

/// Refresh the access token using the refresh_token grant.
async fn refresh_token(store: &TokenStore) -> Result<TokenStore> {
    let client_id = codex_client_id();
    let client = reqwest::Client::new();
    let resp = client
        .post(&store.token_endpoint)
        .form(&[
            ("grant_type", "refresh_token"),
            ("refresh_token", store.refresh_token.as_str()),
            ("client_id", client_id.as_str()),
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
    let client_id = codex_client_id();
    let resp = client
        .post(CODEX_DEVICE_AUTH_URL)
        .header("Content-Type", "application/json").json(&serde_json::json!({
            "client_id": client_id
        }))
        .send()
        .await?;

    if !resp.status().is_success() {
        let body = resp.text().await.unwrap_or_default();
        return Err(anyhow!("Device authorization request failed: {body}"));
    }

    let device_resp: serde_json::Value = resp.json().await?;
    let device_code = device_resp["device_auth_id"]
        .as_str()
        .ok_or_else(|| anyhow!("No device_auth_id in response"))?;
    let user_code = device_resp["user_code"]
        .as_str()
        .ok_or_else(|| anyhow!("No user_code in response"))?;
    let verification_uri = "https://auth.openai.com/codex/device";
    let interval = device_resp["interval"].as_str().and_then(|s| s.parse::<u64>().ok()).or_else(|| device_resp["interval"].as_u64()).unwrap_or(5).max(5);

    println!("  Go to:      {}", verification_uri);
    println!("  Enter code: {}\n", user_code);
    println!("Waiting for authorization...");

    // Step 2: Poll for token (F4: 10 minute wall-clock timeout)
    let deadline = tokio::time::Instant::now() + tokio::time::Duration::from_secs(600);
    let mut poll_interval = interval;

    loop {
        if tokio::time::Instant::now() >= deadline {
            return Err(anyhow!(
                "Device flow timed out after 10 minutes. Please try again."
            ));
        }

        tokio::time::sleep(tokio::time::Duration::from_secs(poll_interval)).await;

        let resp = client
            .post(CODEX_TOKEN_URL)
            .json(&serde_json::json!({
                "client_id": client_id,
                "device_auth_id": device_code,
                "user_code": user_code
            }))
            .send()
            .await?;

        let status = resp.status();
        let payload: serde_json::Value = resp.json().await?;

        if status.is_success() {
            // Device auth returns authorization_code + code_verifier
            let auth_code = payload["authorization_code"]
                .as_str()
                .ok_or_else(|| anyhow!("No authorization_code in device auth response: {payload}"))?;
            let code_verifier = payload["code_verifier"]
                .as_str()
                .ok_or_else(|| anyhow!("No code_verifier in device auth response: {payload}"))?;

            // Step 3: Exchange authorization_code for tokens at /oauth/token
            let token_resp = client
                .post("https://auth.openai.com/oauth/token")
                .form(&[
                    ("grant_type", "authorization_code"),
                    ("client_id", client_id.as_str()),
                    ("code", auth_code),
                    ("code_verifier", code_verifier),
                    ("redirect_uri", "https://auth.openai.com/deviceauth/callback"),
                ])
                .send()
                .await?;

            if !token_resp.status().is_success() {
                let body = token_resp.text().await.unwrap_or_default();
                return Err(anyhow!("Token exchange failed: {body}"));
            }

            let token_payload: serde_json::Value = token_resp.json().await?;
            let access_token = token_payload["access_token"]
                .as_str()
                .ok_or_else(|| anyhow!("No access_token in token response: {token_payload}"))?;
            let refresh_token = token_payload["refresh_token"]
                .as_str()
                .ok_or_else(|| anyhow!("No refresh_token in token response"))?;
            let expires_in = token_payload["expires_in"].as_u64().unwrap_or(3600);
            let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();

            let store = TokenStore {
                access_token: access_token.to_string(),
                refresh_token: refresh_token.to_string(),
                expires_at: now + expires_in,
                token_endpoint: "https://auth.openai.com/oauth/token".to_string(),
                provider: "codex".to_string(),
            };
            save_tokens(&store)?;
            println!("\n✅ Login successful! Token saved to {:?}", auth_path());
            return Ok(());
        }

        // OpenAI returns nested error: {"error": {"code": "...", "message": "..."}}
            let error_code = payload["error"]["code"].as_str()
                .or_else(|| payload["error"].as_str())
                .unwrap_or_default();
            match error_code {
                "authorization_pending" | "deviceauth_authorization_pending" => continue,
                "slow_down" => {
                    poll_interval += 5;
                    continue;
                }
                "expired_token" | "deviceauth_expired" => return Err(anyhow!("Device code expired. Please try again.")),
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

#[cfg(test)]
mod tests {
    use super::*;

    fn make_store(expires_at: u64) -> TokenStore {
        TokenStore {
            access_token: "test_access_token_value".to_string(),
            refresh_token: "test_refresh".to_string(),
            expires_at,
            token_endpoint: "https://example.com/token".to_string(),
            provider: "codex".to_string(),
        }
    }

    #[test]
    fn test_is_expired_future_token() {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let store = make_store(now + 3600);
        assert!(!is_expired(&store));
    }

    #[test]
    fn test_is_expired_past_token() {
        let store = make_store(0);
        assert!(is_expired(&store));
    }

    #[test]
    fn test_is_expired_within_skew() {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        // Token expires in 60s, but skew is 120s → should be considered expired
        let store = make_store(now + 60);
        assert!(is_expired(&store));
    }

    #[test]
    fn test_auth_path() {
        let path = auth_path();
        assert!(path.to_string_lossy().contains(".openab/agent/auth.json"));
    }

    #[test]
    fn test_codex_client_id_default() {
        // When env var is not set, should return default
        unsafe { std::env::remove_var("OPENAB_AGENT_OAUTH_CLIENT_ID") };
        assert_eq!(codex_client_id(), "app_EMoamEEZ73f0CkXaXp7hrann");
    }

    #[test]
    fn test_codex_client_id_override() {
        unsafe { std::env::set_var("OPENAB_AGENT_OAUTH_CLIENT_ID", "custom_id") };
        assert_eq!(codex_client_id(), "custom_id");
        unsafe { std::env::remove_var("OPENAB_AGENT_OAUTH_CLIENT_ID") };
    }
}
