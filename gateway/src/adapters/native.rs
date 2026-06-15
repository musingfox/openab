/// Native adapter — browser WebSocket interface.
///
/// Exposes two routes (meant to be mounted by the binary's main.rs or test harness):
///   GET /native            — minimal browser UI (HTML + WS script)
///   GET /native/ws         — browser WebSocket connections
///
/// Inbound (browser → adapter): JSON `{"text": "..."}`
///   → produces a GatewayEvent (openab.gateway.event.v1, platform=native)
///   → sent through the shared `event_tx` broadcast channel.
///
/// Outbound (OAB reply → browser): a GatewayReply (openab.gateway.reply.v1,
///   platform=native) whose `channel.id` matches a connected browser's connection ID
///   → pushes `{"type":"message","text":"..."}` over that WS connection.
use crate::schema::*;
use axum::{
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        State,
    },
    response::{Html, IntoResponse, Response},
    routing::get,
    Router,
};
use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::{broadcast, mpsc, Mutex};
use tracing::{error, info};

// ─── shared state ────────────────────────────────────────────────────────────

/// Per-connection sender handle keyed by connection_id.
pub type NativeSenders = Arc<Mutex<HashMap<String, mpsc::Sender<String>>>>;

/// Inbound message from the browser (`{"text": "..."}`).
#[derive(Debug, Deserialize)]
pub struct BrowserMessage {
    pub text: String,
}

/// Outbound push to the browser.
#[derive(Debug, Serialize)]
pub struct BrowserPush {
    #[serde(rename = "type")]
    pub msg_type: String,
    pub text: String,
}

// ─── HTTP GET / — minimal browser UI ─────────────────────────────────────────

pub async fn ui_handler() -> impl IntoResponse {
    Html(
        r#"<!DOCTYPE html>
<html lang="en">
<head><meta charset="UTF-8"><title>OpenAB Native</title></head>
<body>
<ul id="messages"></ul>
<input id="input" type="text" placeholder="Type a message…">
<button onclick="send()">Send</button>
<script>
const ws = new WebSocket((location.protocol==='https:'?'wss':'ws')+'://'+location.host+'/native/ws');
ws.onmessage = e => {
  const li = document.createElement('li');
  li.textContent = JSON.parse(e.data).text;
  document.getElementById('messages').appendChild(li);
};
function send() {
  const inp = document.getElementById('input');
  ws.send(JSON.stringify({text: inp.value}));
  inp.value = '';
}
document.getElementById('input').addEventListener('keydown', e => { if(e.key==='Enter') send(); });
</script>
</body>
</html>"#,
    )
}

// ─── WS handler — one connection per browser tab ──────────────────────────────

pub async fn ws_handler(
    State((senders, event_tx)): State<(NativeSenders, broadcast::Sender<String>)>,
    ws: WebSocketUpgrade,
) -> Response {
    ws.on_upgrade(move |socket| handle_browser(socket, senders, event_tx))
}

async fn handle_browser(
    socket: WebSocket,
    senders: NativeSenders,
    event_tx: broadcast::Sender<String>,
) {
    let conn_id = uuid::Uuid::new_v4().to_string();
    info!(conn_id = %conn_id, "native browser connected");

    let (mut ws_tx, mut ws_rx) = socket.split();
    let (push_tx, mut push_rx) = mpsc::channel::<String>(64);

    // Register this connection so outbound replies can find it.
    senders.lock().await.insert(conn_id.clone(), push_tx);

    // Spawn a task that forwards queued pushes to the WS sink.
    let sender_task = tokio::spawn(async move {
        while let Some(msg) = push_rx.recv().await {
            if ws_tx.send(Message::Text(msg.into())).await.is_err() {
                break;
            }
        }
    });

    // Receive loop: browser → GatewayEvent.
    while let Some(Ok(msg)) = ws_rx.next().await {
        let text_payload = match msg {
            Message::Text(t) => t.to_string(),
            Message::Close(_) => break,
            _ => continue,
        };

        let browser_msg: BrowserMessage = match serde_json::from_str(&text_payload) {
            Ok(m) => m,
            Err(e) => {
                error!(conn_id = %conn_id, "native parse error: {e}");
                continue;
            }
        };

        let event = GatewayEvent::new(
            "native",
            ChannelInfo {
                id: conn_id.clone(),
                channel_type: "native".into(),
                thread_id: None,
            },
            SenderInfo {
                id: conn_id.clone(),
                name: "browser".into(),
                display_name: "Browser User".into(),
                is_bot: false,
            },
            &browser_msg.text,
            &format!("native_{}", uuid::Uuid::new_v4()),
            vec![],
        );

        let json = serde_json::to_string(&event).unwrap_or_default();
        let _ = event_tx.send(json);
    }

    // Cleanup.
    senders.lock().await.remove(&conn_id);
    sender_task.abort();
    info!(conn_id = %conn_id, "native browser disconnected");
}

// ─── Outbound dispatch: called by the OAB reply loop ─────────────────────────

/// Dispatch a GatewayReply (platform=native) to the matching browser WS connection.
/// `reply.channel.id` is the connection_id assigned at connect time.
pub async fn dispatch_reply(senders: &NativeSenders, reply: &GatewayReply) {
    let conn_id = &reply.channel.id;
    let push = BrowserPush {
        msg_type: "message".into(),
        text: reply.content.text.clone(),
    };
    let json = match serde_json::to_string(&push) {
        Ok(j) => j,
        Err(e) => {
            error!(conn_id = %conn_id, "native serialize error: {e}");
            return;
        }
    };
    let guard = senders.lock().await;
    if let Some(tx) = guard.get(conn_id) {
        if tx.send(json).await.is_err() {
            error!(conn_id = %conn_id, "native push failed (channel closed)");
        }
    } else {
        error!(conn_id = %conn_id, "native dispatch: no WS connection for id");
    }
}

// ─── Router factory ──────────────────────────────────────────────────────────

/// Build the native-adapter sub-router to be merged into the main axum Router.
///
/// Routes exposed:
///   GET /native        → browser UI
///   GET /native/ws     → browser WebSocket
pub fn router(senders: NativeSenders, event_tx: broadcast::Sender<String>) -> Router {
    Router::new()
        .route("/native", get(ui_handler))
        .route("/native/ws", get(ws_handler))
        .with_state((senders, event_tx))
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::to_bytes;
    use axum::http::{Request, StatusCode};
    use tokio::net::TcpListener;
    use tokio_tungstenite::{connect_async, tungstenite::Message as TMsg};

    fn make_state() -> (NativeSenders, broadcast::Sender<String>, broadcast::Receiver<String>) {
        let senders: NativeSenders = Arc::new(Mutex::new(HashMap::new()));
        let (event_tx, event_rx) = broadcast::channel::<String>(64);
        (senders, event_tx, event_rx)
    }

    async fn spawn_server(app: Router) -> u16 {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        port
    }

    // Example 1: inbound text → GatewayEvent(platform=native, schema=openab.gateway.event.v1)
    #[tokio::test]
    async fn inbound_text_produces_native_gateway_event() {
        let (senders, event_tx, mut event_rx) = make_state();
        let app = router(senders, event_tx);
        let port = spawn_server(app).await;

        let url = format!("ws://127.0.0.1:{port}/native/ws");
        let (mut ws, _) = connect_async(&url).await.expect("connect failed");

        ws.send(TMsg::Text(r#"{"text":"hi"}"#.to_string().into()))
            .await
            .unwrap();

        // Give the server a moment to process then receive the event.
        let raw = tokio::time::timeout(std::time::Duration::from_secs(3), event_rx.recv())
            .await
            .expect("timeout waiting for event")
            .expect("broadcast recv error");

        let event: GatewayEvent = serde_json::from_str(&raw).expect("must parse as GatewayEvent");
        assert_eq!(event.schema, "openab.gateway.event.v1");
        assert_eq!(event.platform, "native");
        assert_eq!(event.content.text, "hi");

        ws.close(None).await.unwrap();
    }

    // Example 2: GatewayReply(platform=native) → content.text pushed to browser WS
    #[tokio::test]
    async fn outbound_reply_pushes_text_to_ws_client() {
        let (senders, event_tx, mut event_rx) = make_state();
        let app = router(senders.clone(), event_tx);
        let port = spawn_server(app).await;

        let url = format!("ws://127.0.0.1:{port}/native/ws");
        let (mut ws, _) = connect_async(&url).await.expect("connect failed");

        // Send a message to get the conn_id from the resulting event.
        ws.send(TMsg::Text(r#"{"text":"ping"}"#.to_string().into()))
            .await
            .unwrap();

        let raw = tokio::time::timeout(std::time::Duration::from_secs(3), event_rx.recv())
            .await
            .expect("timeout")
            .expect("recv");
        let event: GatewayEvent = serde_json::from_str(&raw).unwrap();
        let conn_id = event.channel.id.clone();

        // Now dispatch a GatewayReply targeting that conn_id.
        let reply = GatewayReply {
            schema: "openab.gateway.reply.v1".into(),
            reply_to: "evt_x".into(),
            platform: "native".into(),
            channel: ReplyChannel {
                id: conn_id.clone(),
                thread_id: None,
            },
            content: Content {
                content_type: "text".into(),
                text: "pong".into(),
                attachments: vec![],
            },
            command: None,
            request_id: None,
            quote_message_id: None,
        };

        dispatch_reply(&senders, &reply).await;

        // The browser WS should receive {"type":"message","text":"pong"}.
        let pushed = tokio::time::timeout(std::time::Duration::from_secs(3), ws.next())
            .await
            .expect("timeout waiting for push")
            .expect("stream ended")
            .expect("ws error");

        let text = match pushed {
            TMsg::Text(t) => t.to_string(),
            other => panic!("unexpected message: {other:?}"),
        };
        let v: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(v["type"], "message");
        assert_eq!(v["text"], "pong");

        ws.close(None).await.unwrap();
    }

    // Example 3: two clients, reply routes only to the target — no cross-talk.
    #[tokio::test]
    async fn reply_routes_to_target_client_only() {
        let (senders, event_tx, mut event_rx) = make_state();
        let app = router(senders.clone(), event_tx);
        let port = spawn_server(app).await;

        let url = format!("ws://127.0.0.1:{port}/native/ws");

        // Connect client A and B.
        let (mut ws_a, _) = connect_async(&url).await.unwrap();
        let (mut ws_b, _) = connect_async(&url).await.unwrap();

        // Get conn_id for A.
        ws_a.send(TMsg::Text(r#"{"text":"from_a"}"#.to_string().into()))
            .await
            .unwrap();
        let raw_a = tokio::time::timeout(std::time::Duration::from_secs(3), event_rx.recv())
            .await
            .expect("timeout a")
            .expect("recv a");
        let ev_a: GatewayEvent = serde_json::from_str(&raw_a).unwrap();
        let conn_id_a = ev_a.channel.id.clone();

        // Get conn_id for B.
        ws_b.send(TMsg::Text(r#"{"text":"from_b"}"#.to_string().into()))
            .await
            .unwrap();
        let raw_b = tokio::time::timeout(std::time::Duration::from_secs(3), event_rx.recv())
            .await
            .expect("timeout b")
            .expect("recv b");
        let ev_b: GatewayEvent = serde_json::from_str(&raw_b).unwrap();
        let conn_id_b = ev_b.channel.id.clone();

        // Dispatch a reply targeted at B only.
        let reply = GatewayReply {
            schema: "openab.gateway.reply.v1".into(),
            reply_to: "evt_y".into(),
            platform: "native".into(),
            channel: ReplyChannel {
                id: conn_id_b.clone(),
                thread_id: None,
            },
            content: Content {
                content_type: "text".into(),
                text: "only_for_b".into(),
                attachments: vec![],
            },
            command: None,
            request_id: None,
            quote_message_id: None,
        };
        dispatch_reply(&senders, &reply).await;

        // B must receive the message.
        let pushed_b = tokio::time::timeout(std::time::Duration::from_secs(3), ws_b.next())
            .await
            .expect("timeout b push")
            .expect("stream b ended")
            .expect("ws b error");
        let text_b = match pushed_b {
            TMsg::Text(t) => t.to_string(),
            other => panic!("unexpected: {other:?}"),
        };
        let vb: serde_json::Value = serde_json::from_str(&text_b).unwrap();
        assert_eq!(vb["text"], "only_for_b");

        // A must NOT receive anything within a short window.
        let result_a =
            tokio::time::timeout(std::time::Duration::from_millis(200), ws_a.next()).await;
        assert!(
            result_a.is_err(),
            "client A should not have received the reply targeting B (conn_id_a={conn_id_a})"
        );

        ws_a.close(None).await.unwrap();
        ws_b.close(None).await.unwrap();
    }

    // Example 4: HTTP GET / returns HTML with WS-connect script.
    #[tokio::test]
    async fn http_get_root_returns_ui_with_ws_script() {
        use axum::body::Body;

        let (senders, event_tx, _) = make_state();
        let app = router(senders, event_tx);

        let req = Request::builder()
            .uri("/native")
            .body(Body::empty())
            .unwrap();

        use tower::util::ServiceExt;
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        let body = to_bytes(resp.into_body(), 1024 * 64).await.unwrap();
        let html = String::from_utf8(body.to_vec()).unwrap();

        assert!(
            html.contains("<input"),
            "UI must contain an input element"
        );
        assert!(
            html.contains("new WebSocket"),
            "UI must contain a WebSocket connection script"
        );
        assert!(
            html.contains("/native/ws"),
            "WS script must reference /native/ws"
        );
    }
}
