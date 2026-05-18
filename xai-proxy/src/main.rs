use anyhow::{anyhow, Context, Result};
use axum::{body::Body, extract::State, http::Request, response::Response, routing::any, Router};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use clap::{Parser, Subcommand};
use hyper_util::{client::legacy::Client, rt::TokioExecutor};
use rand::RngCore;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    net::SocketAddr,
    path::PathBuf,
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::TcpListener,
    sync::RwLock,
};
use tracing::{error, info};

// === Constants (borrowed from Hermes) ===

const XAI_OAUTH_DISCOVERY_URL: &str = "https://auth.x.ai/.well-known/openid-configuration";
const XAI_OAUTH_CLIENT_ID: &str = "b1a00492-073a-47ea-816f-4c329264a828";
const XAI_OAUTH_SCOPE: &str = "openid profile email offline_access grok-cli:access api:access";
const XAI_OAUTH_REDIRECT_PORT: u16 = 56121;
const XAI_API_BASE: &str = "https://api.x.ai";
const REFRESH_SKEW_SECONDS: u64 = 120;

// === CLI ===

#[derive(Parser)]
#[command(name = "xai-proxy", about = "xAI OAuth proxy sidecar")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Authenticate with xAI via browser OAuth (PKCE)
    Login,
    /// Authenticate with xAI via device-code flow (headless/K8s/ECS)
    LoginDevice,
    /// Start the proxy server
    Serve {
        /// Listen port
        #[arg(short, long, default_value = "9090")]
        port: u16,
        /// Listen address
        #[arg(long, default_value = "127.0.0.1")]
        bind: String,
    },
}

// === Token Storage ===

#[derive(Debug, Clone, Serialize, Deserialize)]
struct TokenStore {
    access_token: String,
    refresh_token: String,
    #[serde(default)]
    expires_at: u64, // unix timestamp
    #[serde(default)]
    token_endpoint: String,
}

fn token_path() -> PathBuf {
    if let Ok(p) = std::env::var("XAI_PROXY_TOKEN_PATH") {
        let path = PathBuf::from(p);
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).ok();
        }
        return path;
    }
    let dir = dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".xai-proxy");
    std::fs::create_dir_all(&dir).ok();
    dir.join("tokens.json")
}

fn load_tokens() -> Result<TokenStore> {
    let path = token_path();
    let data = std::fs::read_to_string(&path)
        .with_context(|| format!("No token file at {}. Run `xai-proxy login` first.", path.display()))?;
    serde_json::from_str(&data).context("Invalid token file")
}

fn save_tokens(store: &TokenStore) -> Result<()> {
    let path = token_path();
    let data = serde_json::to_string_pretty(store)?;
    std::fs::write(&path, data)?;
    // chmod 600
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

// === OIDC Discovery ===

#[derive(Deserialize)]
struct OidcDiscovery {
    authorization_endpoint: String,
    token_endpoint: String,
    #[serde(default)]
    device_authorization_endpoint: String,
}

async fn discover_endpoints() -> Result<OidcDiscovery> {
    let client = reqwest::Client::new();
    let resp = client
        .get(XAI_OAUTH_DISCOVERY_URL)
        .send()
        .await?
        .error_for_status()?;
    resp.json().await.context("Failed to parse OIDC discovery")
}

// === PKCE ===

fn pkce_verifier() -> String {
    let mut buf = [0u8; 64];
    rand::thread_rng().fill_bytes(&mut buf);
    URL_SAFE_NO_PAD.encode(buf)
}

fn pkce_challenge(verifier: &str) -> String {
    let digest = Sha256::digest(verifier.as_bytes());
    URL_SAFE_NO_PAD.encode(digest)
}

// === OAuth Login ===

async fn do_login() -> Result<()> {
    info!("Starting xAI OAuth PKCE login...");
    let discovery = discover_endpoints().await?;

    let code_verifier = pkce_verifier();
    let code_challenge = pkce_challenge(&code_verifier);
    let state = uuid::Uuid::new_v4().to_string();
    let nonce = uuid::Uuid::new_v4().to_string();

    let redirect_uri = format!("http://127.0.0.1:{}/callback", XAI_OAUTH_REDIRECT_PORT);

    let authorize_url = format!(
        "{}?response_type=code&client_id={}&redirect_uri={}&scope={}&code_challenge={}&code_challenge_method=S256&state={}&nonce={}&plan=generic&referrer=xai-proxy",
        discovery.authorization_endpoint,
        urlencoding::encode(XAI_OAUTH_CLIENT_ID),
        urlencoding::encode(&redirect_uri),
        urlencoding::encode(XAI_OAUTH_SCOPE),
        urlencoding::encode(&code_challenge),
        urlencoding::encode(&state),
        urlencoding::encode(&nonce),
    );

    // Start local callback server
    let listener = TcpListener::bind(format!("127.0.0.1:{}", XAI_OAUTH_REDIRECT_PORT))
        .await
        .context("Failed to bind callback port 56121")?;

    println!("\nOpen this URL to authorize:\n");
    println!("  {}\n", authorize_url);

    // Try to open browser
    if open::that(&authorize_url).is_ok() {
        println!("Browser opened. Waiting for callback...");
    } else {
        println!("Could not open browser. Please open the URL above manually.");
    }

    // Wait for callback
    let (mut stream, _) = listener.accept().await?;
    let mut reader = BufReader::new(&mut stream);
    let mut request_line = String::new();
    reader.read_line(&mut request_line).await?;

    // Parse GET /callback?code=...&state=... HTTP/1.1
    let path = request_line
        .split_whitespace()
        .nth(1)
        .ok_or_else(|| anyhow!("Invalid HTTP request"))?;

    let url = url::Url::parse(&format!("http://localhost{}", path))?;
    let params: std::collections::HashMap<_, _> = url.query_pairs().collect();

    // Drain remaining headers
    loop {
        let mut line = String::new();
        reader.read_line(&mut line).await?;
        if line.trim().is_empty() {
            break;
        }
    }

    // Send response
    let body = "<html><body><h1>xAI authorization received.</h1>You can close this tab.</body></html>";
    let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        body.len(),
        body
    );
    stream.write_all(response.as_bytes()).await?;

    // Validate state
    let received_state = params.get("state").ok_or_else(|| anyhow!("No state in callback"))?;
    if received_state.as_ref() != state {
        return Err(anyhow!("State mismatch — possible CSRF"));
    }

    let code = params
        .get("code")
        .ok_or_else(|| anyhow!("No code in callback. Error: {:?}", params.get("error")))?;

    // Exchange code for tokens
    info!("Exchanging authorization code for tokens...");
    let client = reqwest::Client::new();
    let resp = client
        .post(&discovery.token_endpoint)
        .header("Content-Type", "application/x-www-form-urlencoded")
        .form(&[
            ("grant_type", "authorization_code"),
            ("code", code.as_ref()),
            ("redirect_uri", &redirect_uri),
            ("client_id", XAI_OAUTH_CLIENT_ID),
            ("code_verifier", &code_verifier),
            ("code_challenge", &code_challenge),
            ("code_challenge_method", "S256"),
        ])
        .send()
        .await?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return Err(anyhow!("Token exchange failed (HTTP {}): {}", status, body));
    }

    let token_resp: serde_json::Value = resp.json().await?;
    let access_token = token_resp["access_token"]
        .as_str()
        .ok_or_else(|| anyhow!("No access_token in response"))?;
    let refresh_token = token_resp["refresh_token"]
        .as_str()
        .ok_or_else(|| anyhow!("No refresh_token in response"))?;
    let expires_in = token_resp["expires_in"].as_u64().unwrap_or(3600);

    let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
    let store = TokenStore {
        access_token: access_token.to_string(),
        refresh_token: refresh_token.to_string(),
        expires_at: now + expires_in,
        token_endpoint: discovery.token_endpoint,
    };
    save_tokens(&store)?;

    println!("\n✅ Login successful! Token saved to {:?}", token_path());
    println!("   Run `xai-proxy serve` to start the proxy.");
    Ok(())
}

// === Device-Code Login (headless) ===

async fn do_login_device() -> Result<()> {
    info!("Starting xAI device-code login...");
    let discovery = discover_endpoints().await?;

    let device_endpoint = if discovery.device_authorization_endpoint.is_empty() {
        "https://auth.x.ai/oauth2/device/code".to_string()
    } else {
        discovery.device_authorization_endpoint
    };

    let client = reqwest::Client::new();
    let resp = client
        .post(&device_endpoint)
        .form(&[
            ("client_id", XAI_OAUTH_CLIENT_ID),
            ("scope", XAI_OAUTH_SCOPE),
        ])
        .send()
        .await?;

    if !resp.status().is_success() {
        let body = resp.text().await.unwrap_or_default();
        return Err(anyhow!("Device authorization failed: {}", body));
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
        .unwrap_or("https://auth.x.ai/oauth2/device");
    let interval = device_resp["interval"].as_u64().unwrap_or(5);

    println!("\n  Go to:     {}", verification_uri);
    println!("  Enter code: {}\n", user_code);
    println!("Waiting for authorization...");

    loop {
        tokio::time::sleep(tokio::time::Duration::from_secs(interval)).await;

        let resp = client
            .post(&discovery.token_endpoint)
            .form(&[
                ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
                ("client_id", XAI_OAUTH_CLIENT_ID),
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
                token_endpoint: discovery.token_endpoint,
            };
            save_tokens(&store)?;
            println!("\n✅ Login successful! Token saved to {:?}", token_path());
            println!("   Run `xai-proxy serve` to start the proxy.");
            return Ok(());
        }

        let error = payload["error"].as_str().unwrap_or_default();
        match error {
            "authorization_pending" | "slow_down" => continue,
            "expired_token" => return Err(anyhow!("Device code expired. Try again.")),
            "access_denied" => return Err(anyhow!("Authorization denied by user.")),
            _ => return Err(anyhow!("Device-code error: {} — {:?}", error, payload)),
        }
    }
}

// === Token Refresh ===

async fn refresh_token(store: &TokenStore) -> Result<TokenStore> {
    let client = reqwest::Client::new();
    let resp = client
        .post(&store.token_endpoint)
        .form(&[
            ("grant_type", "refresh_token"),
            ("refresh_token", &store.refresh_token),
            ("client_id", XAI_OAUTH_CLIENT_ID),
        ])
        .send()
        .await?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return Err(anyhow!("Token refresh failed (HTTP {}): {}", status, body));
    }

    let token_resp: serde_json::Value = resp.json().await?;
    let access_token = token_resp["access_token"]
        .as_str()
        .ok_or_else(|| anyhow!("No access_token in refresh response"))?;
    let refresh_token = token_resp["refresh_token"]
        .as_str()
        .unwrap_or(&store.refresh_token);
    let expires_in = token_resp["expires_in"].as_u64().unwrap_or(3600);

    let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
    Ok(TokenStore {
        access_token: access_token.to_string(),
        refresh_token: refresh_token.to_string(),
        expires_at: now + expires_in,
        token_endpoint: store.token_endpoint.clone(),
    })
}

// === Proxy State ===

struct ProxyState {
    tokens: RwLock<TokenStore>,
    http_client: Client<hyper_rustls::HttpsConnector<hyper_util::client::legacy::connect::HttpConnector>, Body>,
}

impl ProxyState {
    async fn get_valid_token(&self) -> Result<String> {
        let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
        {
            let tokens = self.tokens.read().await;
            if tokens.expires_at > now + REFRESH_SKEW_SECONDS {
                return Ok(tokens.access_token.clone());
            }
        }
        // Need refresh
        let mut tokens = self.tokens.write().await;
        // Double-check after acquiring write lock
        let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
        if tokens.expires_at > now + REFRESH_SKEW_SECONDS {
            return Ok(tokens.access_token.clone());
        }
        info!("Refreshing xAI OAuth token...");
        let new_tokens = refresh_token(&tokens).await?;
        save_tokens(&new_tokens)?;
        *tokens = new_tokens;
        Ok(tokens.access_token.clone())
    }
}

// === Proxy Handler ===

async fn proxy_handler(
    State(state): State<Arc<ProxyState>>,
    mut req: Request<Body>,
) -> Response<Body> {
    let token = match state.get_valid_token().await {
        Ok(t) => t,
        Err(e) => {
            error!("Failed to get token: {}", e);
            return Response::builder()
                .status(502)
                .body(Body::from(format!("Token error: {}", e)))
                .unwrap();
        }
    };

    // Rewrite URI to api.x.ai
    let path_and_query = req
        .uri()
        .path_and_query()
        .map(|pq| pq.as_str())
        .unwrap_or("/");
    let target_uri = format!("{}{}", XAI_API_BASE, path_and_query);

    *req.uri_mut() = target_uri.parse().unwrap();

    // Inject auth header
    req.headers_mut().insert(
        hyper::header::AUTHORIZATION,
        format!("Bearer {}", token).parse().unwrap(),
    );
    req.headers_mut().insert(
        hyper::header::HOST,
        "api.x.ai".parse().unwrap(),
    );

    // Forward
    match state.http_client.request(req).await {
        Ok(resp) => {
            let (parts, body) = resp.into_parts();
            Response::from_parts(parts, Body::new(body))
        }
        Err(e) => {
            error!("Upstream error: {}", e);
            Response::builder()
                .status(502)
                .body(Body::from(format!("Upstream error: {}", e)))
                .unwrap()
        }
    }
}

// === Serve ===

async fn do_serve(bind: &str, port: u16) -> Result<()> {
    let store = load_tokens()?;

    // Check if token needs immediate refresh
    let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
    let store = if store.expires_at <= now + REFRESH_SKEW_SECONDS {
        info!("Token expired, refreshing...");
        let new_store = refresh_token(&store).await?;
        save_tokens(&new_store)?;
        new_store
    } else {
        store
    };

    // Build HTTPS client for upstream
    let https = hyper_rustls::HttpsConnectorBuilder::new()
        .with_native_roots()?
        .https_or_http()
        .enable_http1()
        .enable_http2()
        .build();
    let http_client = Client::builder(TokioExecutor::new()).build(https);

    let state = Arc::new(ProxyState {
        tokens: RwLock::new(store),
        http_client,
    });

    let app = Router::new()
        .route("/{*path}", any(proxy_handler))
        .route("/", any(proxy_handler))
        .with_state(state);

    let addr: SocketAddr = format!("{}:{}", bind, port).parse()?;
    let listener = TcpListener::bind(addr).await?;
    info!("xai-proxy listening on http://{}", addr);
    println!("xai-proxy listening on http://{}", addr);
    println!("Set your client's base URL to: http://{}/v1", addr);

    axum::serve(listener, app).await?;
    Ok(())
}

// === Main ===

#[tokio::main]
async fn main() -> Result<()> {
    rustls::crypto::ring::default_provider()
        .install_default()
        .expect("Failed to install rustls crypto provider");

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "xai_proxy=info".into()),
        )
        .init();

    let cli = Cli::parse();
    match cli.command {
        Commands::Login => do_login().await,
        Commands::LoginDevice => do_login_device().await,
        Commands::Serve { port, bind } => do_serve(&bind, port).await,
    }
}
