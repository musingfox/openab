pub mod adapters;
pub mod media;
pub mod schema;
pub mod store;

use axum::{
    extract::State,
    response::IntoResponse,
    routing::{get, post},
    Router,
};
use futures_util::{SinkExt, StreamExt};
use schema::GatewayReply;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::{broadcast, Mutex, Semaphore};
use tracing::{info, warn};

// --- Reply token cache for LINE hybrid Reply/Push dispatch ---

/// Cache entry for LINE reply tokens: (replyToken, insertion_time).
pub type ReplyTokenCache = Arc<std::sync::Mutex<HashMap<String, (String, Instant)>>>;

/// Maximum age (in seconds) before a cached reply token is considered expired.
pub const REPLY_TOKEN_TTL_SECS: u64 = 50;

/// Maximum number of cached reply tokens.
pub const REPLY_TOKEN_CACHE_MAX: usize = 10_000;

/// Maximum number of post-ack LINE webhook payloads processed concurrently.
pub const LINE_WEBHOOK_CONCURRENCY_MAX: usize = 8;

// --- App state (shared across all adapters) ---

pub struct AppState {
    /// Telegram bot token (None if Telegram disabled)
    pub telegram_bot_token: Option<String>,
    /// Telegram webhook secret token for request validation
    pub telegram_secret_token: Option<String>,
    /// LINE channel secret for signature validation
    pub line_channel_secret: Option<String>,
    /// LINE channel access token for reply API
    pub line_access_token: Option<String>,
    /// Teams adapter (None if Teams disabled)
    pub teams: Option<adapters::teams::TeamsAdapter>,
    /// service_url cache for Teams reply routing
    pub teams_service_urls: Mutex<HashMap<String, (String, Instant)>>,
    /// Feishu adapter (None if Feishu disabled)
    pub feishu: Option<adapters::feishu::FeishuAdapter>,
    /// Google Chat adapter (None if Google Chat disabled)
    pub google_chat: Option<adapters::googlechat::GoogleChatAdapter>,
    pub wecom: Option<adapters::wecom::WecomAdapter>,
    /// WebSocket authentication token
    pub ws_token: Option<String>,
    /// Broadcast channel: gateway → OAB (events from all platforms)
    pub event_tx: broadcast::Sender<String>,
    /// Cache: event_id → (LINE replyToken, timestamp).
    pub reply_token_cache: ReplyTokenCache,
    /// Limits concurrent post-ack LINE webhook processing
    pub line_webhook_semaphore: Arc<Semaphore>,
    /// Shared HTTP client for media downloads and API calls
    pub client: reqwest::Client,
    /// Per-connection sender handles for the native adapter
    pub native_senders: adapters::native::NativeSenders,
}

// --- WebSocket handler (OAB connects here) ---

async fn ws_handler(
    State(state): State<Arc<AppState>>,
    query: axum::extract::Query<HashMap<String, String>>,
    ws: axum::extract::WebSocketUpgrade,
) -> axum::response::Response {
    if let Some(ref expected) = state.ws_token {
        let provided = query.get("token").map(|s| s.as_str());
        if provided != Some(expected.as_str()) {
            warn!("WebSocket rejected: invalid or missing token");
            return axum::http::StatusCode::UNAUTHORIZED.into_response();
        }
    }
    ws.on_upgrade(move |socket| handle_oab_connection(state, socket))
}

async fn handle_oab_connection(state: Arc<AppState>, socket: axum::extract::ws::WebSocket) {
    use axum::extract::ws::Message;

    let (mut ws_tx, mut ws_rx) = socket.split();
    let mut event_rx = state.event_tx.subscribe();

    info!("OAB client connected via WebSocket");

    // Forward gateway events → OAB
    let send_task = tokio::spawn(async move {
        loop {
            tokio::select! {
                Ok(event_json) = event_rx.recv() => {
                    if ws_tx.send(Message::Text(event_json.into())).await.is_err() {
                        break;
                    }
                }
            }
        }
    });

    // Receive OAB replies → route to correct platform
    let state_for_recv = state.clone();
    // Track per-message reaction state (Telegram replaces all reactions atomically)
    let reaction_state: Arc<Mutex<HashMap<String, Vec<String>>>> =
        Arc::new(Mutex::new(HashMap::new()));
    let recv_task = tokio::spawn(async move {
        let client = reqwest::Client::new();
        while let Some(Ok(msg)) = ws_rx.next().await {
            if let Message::Text(text) = msg {
                match serde_json::from_str::<GatewayReply>(&text) {
                    Ok(reply) => {
                        info!(
                            platform = %reply.platform,
                            channel = %reply.channel.id,
                            command = ?reply.command.as_deref(),
                            "OAB → gateway reply"
                        );
                        match reply.platform.as_str() {
                            "telegram" => {
                                if let Some(ref token) = state_for_recv.telegram_bot_token {
                                    adapters::telegram::handle_reply(
                                        &reply,
                                        token,
                                        &client,
                                        &state_for_recv.event_tx,
                                        &reaction_state,
                                    )
                                    .await;
                                } else {
                                    warn!("reply for telegram but adapter not configured");
                                }
                            }
                            "line" => {
                                if let Some(ref access_token) = state_for_recv.line_access_token {
                                    adapters::line::dispatch_line_reply(
                                        &client,
                                        access_token,
                                        &state_for_recv.reply_token_cache,
                                        &reply,
                                        adapters::line::LINE_API_BASE,
                                    )
                                    .await;
                                } else {
                                    warn!("reply for line but adapter not configured");
                                }
                            }
                            "teams" => {
                                if let Some(ref teams) = state_for_recv.teams {
                                    adapters::teams::handle_reply(
                                        &reply,
                                        teams,
                                        &state_for_recv.teams_service_urls,
                                    )
                                    .await;
                                } else {
                                    warn!("reply for teams but adapter not configured");
                                }
                            }
                            "feishu" => {
                                if let Some(ref feishu) = state_for_recv.feishu {
                                    adapters::feishu::handle_reply(
                                        &reply,
                                        feishu,
                                        &state_for_recv.event_tx,
                                    )
                                    .await;
                                } else {
                                    warn!("reply for feishu but adapter not configured");
                                }
                            }
                            "googlechat" => {
                                if let Some(ref gc) = state_for_recv.google_chat {
                                    gc.handle_reply(&reply, &state_for_recv.event_tx).await;
                                } else {
                                    warn!("reply for googlechat but adapter not configured");
                                }
                            }
                            "wecom" => {
                                if let Some(ref wecom) = state_for_recv.wecom {
                                    wecom.handle_reply(&reply, &state_for_recv.event_tx).await;
                                } else {
                                    warn!("reply for wecom but adapter not configured");
                                }
                            }
                            "native" => {
                                adapters::native::dispatch_reply(
                                    &state_for_recv.native_senders,
                                    &reply,
                                )
                                .await;
                            }
                            other => warn!(platform = other, "unknown reply platform"),
                        }
                    }
                    Err(e) => warn!("invalid reply from OAB: {e}"),
                }
            }
        }
    });

    tokio::select! {
        _ = send_task => {},
        _ = recv_task => {},
    }
    info!("OAB client disconnected");
}

async fn health() -> &'static str {
    "ok"
}

/// Opaque handle returned by `build_app`.
///
/// Holds background-task guards (e.g. the feishu WebSocket shutdown sender)
/// that must remain alive for the lifetime of the server.  Drop this only after
/// `axum::serve` returns.
pub struct AppHandle {
    /// Keeping this alive signals the feishu WS task to keep running.
    /// Dropping it triggers shutdown via the watch channel.
    pub feishu_shutdown_tx: Option<tokio::sync::watch::Sender<bool>>,
}

/// Build the full gateway application.
///
/// Assembles AppState (reading env vars), mounts all adapter routes, wires the
/// OAB WebSocket handler and the native adapter, spawns background tasks
/// (feishu WebSocket, eviction loop), and returns the ready-to-serve `Router`
/// plus an `AppHandle` that must be kept alive until the server exits.
///
/// The caller is responsible for binding a `TcpListener` and calling
/// `axum::serve`.
pub async fn build_app() -> (Router, AppHandle) {
    let ws_token = std::env::var("GATEWAY_WS_TOKEN").ok();

    if ws_token.is_none() {
        warn!("GATEWAY_WS_TOKEN not set — WebSocket connections are NOT authenticated (insecure)");
    }

    let (event_tx, _) = broadcast::channel::<String>(256);
    let reply_token_cache: ReplyTokenCache = Arc::new(std::sync::Mutex::new(HashMap::new()));
    let native_senders: adapters::native::NativeSenders =
        Arc::new(Mutex::new(HashMap::new()));

    let mut app: Router<Arc<AppState>> = Router::new()
        .route("/ws", get(ws_handler))
        .route("/health", get(health));

    // Native adapter — always mounted
    app = app.merge(adapters::native::router());

    // Telegram adapter
    let telegram_bot_token = std::env::var("TELEGRAM_BOT_TOKEN").ok();
    let telegram_secret_token = std::env::var("TELEGRAM_SECRET_TOKEN").ok();
    if telegram_bot_token.is_some() {
        let webhook_path =
            std::env::var("TELEGRAM_WEBHOOK_PATH").unwrap_or_else(|_| "/webhook/telegram".into());
        if telegram_secret_token.is_none() {
            warn!("TELEGRAM_SECRET_TOKEN not set — webhook requests are NOT validated (insecure)");
        }
        info!(path = %webhook_path, "telegram adapter enabled");
        app = app.route(&webhook_path, post(adapters::telegram::webhook));
    }

    // LINE adapter
    let line_channel_secret = std::env::var("LINE_CHANNEL_SECRET").ok();
    let line_access_token = std::env::var("LINE_CHANNEL_ACCESS_TOKEN").ok();
    info!("line adapter enabled");
    app = app.route("/webhook/line", post(adapters::line::webhook));

    // Teams adapter
    let teams = adapters::teams::TeamsConfig::from_env().map(|config| {
        info!("teams adapter enabled");
        adapters::teams::TeamsAdapter::new(config)
    });
    if teams.is_some() {
        let webhook_path =
            std::env::var("TEAMS_WEBHOOK_PATH").unwrap_or_else(|_| "/webhook/teams".into());
        info!(path = %webhook_path, "teams webhook registered");
        app = app.route(&webhook_path, post(adapters::teams::webhook));
    }

    // Feishu adapter
    let feishu_config = adapters::feishu::FeishuConfig::from_env();
    if let Some(ref config) = feishu_config {
        match config.connection_mode {
            adapters::feishu::ConnectionMode::Websocket => {
                info!("feishu adapter enabled (websocket) — will connect after state init");
            }
            adapters::feishu::ConnectionMode::Webhook => {
                let path = config.webhook_path.clone();
                info!(path = %path, "feishu adapter enabled (webhook)");
                app = app.route(&path, post(adapters::feishu::webhook));
            }
        }
    }
    let feishu = feishu_config.map(adapters::feishu::FeishuAdapter::new);

    // Google Chat adapter
    let google_chat_enabled = std::env::var("GOOGLE_CHAT_ENABLED")
        .map(|v| v == "true" || v == "1")
        .unwrap_or(false);
    let google_chat = if google_chat_enabled {
        let token_cache = std::env::var("GOOGLE_CHAT_SA_KEY_JSON")
            .ok()
            .or_else(|| {
                std::env::var("GOOGLE_CHAT_SA_KEY_FILE")
                    .ok()
                    .and_then(|path| std::fs::read_to_string(&path).ok())
            })
            .and_then(|json| {
                adapters::googlechat::GoogleChatTokenCache::new(&json)
                    .map_err(|e| warn!("googlechat SA key error: {e}"))
                    .ok()
            });
        let access_token = std::env::var("GOOGLE_CHAT_ACCESS_TOKEN").ok();
        let jwt_verifier = std::env::var("GOOGLE_CHAT_AUDIENCE").ok().map(|aud| {
            info!("googlechat webhook JWT verification enabled (audience={aud})");
            adapters::googlechat::GoogleChatJwtVerifier::new(aud)
        });

        let webhook_path = std::env::var("GOOGLE_CHAT_WEBHOOK_PATH")
            .unwrap_or_else(|_| "/webhook/googlechat".into());
        info!(path = %webhook_path, "googlechat adapter enabled");
        app = app.route(&webhook_path, post(adapters::googlechat::webhook));

        if token_cache.is_some() {
            info!("googlechat service account configured — token auto-refresh enabled");
        } else if access_token.is_some() {
            warn!("googlechat using static access token — will expire in ~1 hour");
        } else {
            warn!("GOOGLE_CHAT_ACCESS_TOKEN / GOOGLE_CHAT_SA_KEY_JSON not set — replies will be logged but not sent");
        }
        if jwt_verifier.is_none() {
            warn!(
                "GOOGLE_CHAT_AUDIENCE not set — webhook requests are NOT authenticated (insecure)"
            );
        }

        Some(adapters::googlechat::GoogleChatAdapter::new(
            token_cache,
            access_token,
            jwt_verifier,
        ))
    } else {
        None
    };

    // WeCom adapter
    let wecom = adapters::wecom::WecomConfig::from_env().map(|config| {
        let path = config.webhook_path.clone();
        info!(path = %path, "wecom adapter enabled");
        adapters::wecom::WecomAdapter::new(config)
    });
    if let Some(ref w) = wecom {
        app = app
            .route(
                &w.config.webhook_path,
                axum::routing::get(adapters::wecom::verify),
            )
            .route(&w.config.webhook_path, post(adapters::wecom::webhook));
    }

    if telegram_bot_token.is_none()
        && line_access_token.is_none()
        && teams.is_none()
        && feishu.is_none()
        && google_chat.is_none()
        && wecom.is_none()
    {
        warn!("no adapters configured — set TELEGRAM_BOT_TOKEN, LINE_CHANNEL_ACCESS_TOKEN, TEAMS_APP_ID + TEAMS_APP_SECRET, FEISHU_APP_ID + FEISHU_APP_SECRET, GOOGLE_CHAT_ENABLED=true, and/or WECOM_CORP_ID + WECOM_SECRET + WECOM_TOKEN + WECOM_ENCODING_AES_KEY + WECOM_AGENT_ID");
    }

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .expect("HTTP client must build");

    let state = Arc::new(AppState {
        telegram_bot_token,
        telegram_secret_token,
        line_channel_secret,
        line_access_token,
        teams,
        teams_service_urls: Mutex::new(HashMap::new()),
        feishu,
        google_chat,
        wecom,
        ws_token,
        event_tx,
        reply_token_cache,
        line_webhook_semaphore: Arc::new(Semaphore::new(LINE_WEBHOOK_CONCURRENCY_MAX)),
        client,
        native_senders,
    });

    // Background task: sweep expired reply tokens every REPLY_TOKEN_TTL_SECS
    {
        let cache_state = state.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(std::time::Duration::from_secs(REPLY_TOKEN_TTL_SECS)).await;
                let mut cache = cache_state
                    .reply_token_cache
                    .lock()
                    .unwrap_or_else(|e| e.into_inner());
                let before = cache.len();
                cache.retain(|_, (_, t)| t.elapsed().as_secs() < REPLY_TOKEN_TTL_SECS);
                let after = cache.len();
                if before != after {
                    info!(
                        removed = before - after,
                        remaining = after,
                        "reply token cache sweep"
                    );
                }
            }
        });
    }

    // Periodic cleanup of stale Teams service_url entries (TTL: 4 hours)
    {
        let state_for_cleanup = state.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(std::time::Duration::from_secs(300)).await;
                let mut urls = state_for_cleanup.teams_service_urls.lock().await;
                let before = urls.len();
                urls.retain(|_, (_, t)| t.elapsed().as_secs() < 4 * 3600);
                let after = urls.len();
                if before != after {
                    info!(
                        removed = before - after,
                        remaining = after,
                        "teams service_url cache cleanup"
                    );
                }
            }
        });
    }

    // Resolve feishu bot identity and spawn feishu WebSocket long-connection if configured
    let (feishu_shutdown_tx, feishu_shutdown_rx) = tokio::sync::watch::channel(false);
    let feishu_shutdown_tx_opt = if let Some(ref f) = state.feishu {
        f.resolve_bot_identity().await;
        let ws_mode = adapters::feishu::FeishuConfig::from_env()
            .map(|c| c.connection_mode == adapters::feishu::ConnectionMode::Websocket)
            .unwrap_or(false);
        if ws_mode {
            match adapters::feishu::start_websocket(f, state.event_tx.clone(), feishu_shutdown_rx)
                .await
            {
                Ok(_handle) => info!("feishu websocket task spawned"),
                Err(e) => tracing::error!(err = %e, "feishu websocket startup failed"),
            }
        }
        Some(feishu_shutdown_tx)
    } else {
        None
    };

    // Background task: evict expired media files (colocate store, TTL 2 min)
    tokio::spawn(store::eviction_loop());

    let handle = AppHandle {
        feishu_shutdown_tx: feishu_shutdown_tx_opt,
    };

    (app.with_state(state), handle)
}
