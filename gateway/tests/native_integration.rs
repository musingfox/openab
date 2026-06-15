//! Integration tests for the native adapter wired into the full app.
//!
//! These are derived from the FROZEN spec Examples E1–E4, NOT from any
//! implementation. They drive the real app over real sockets:
//!   - the app is built via `openab_gateway::build_app()` (the extracted
//!     full-app factory — NOT the isolated `native::router`),
//!   - served on `127.0.0.1:0` with `axum::serve`,
//!   - exercised with `reqwest` (HTTP) and `tokio_tungstenite` (WS).
//!
//! Behavioral checks are preferred: an outbound reply is verified by the
//! browser WS *actually receiving* it (which proves the reply loop's native
//! branch ran and did NOT fall into the `other => "unknown reply platform"`
//! arm), not by scraping log strings.

use std::time::Duration;
use tokio::net::TcpListener;
use tokio_tungstenite::{connect_async, tungstenite::Message as TMsg};
use futures_util::{SinkExt, StreamExt};

/// Build the full app and serve it on an ephemeral port; return the port.
/// Clears auth env so the OAB-side `/ws` accepts connections without a token.
async fn spawn_full_app() -> u16 {
    std::env::remove_var("GATEWAY_WS_TOKEN");
    let app = openab_gateway::build_app();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    // brief moment for the listener task to be scheduled
    tokio::time::sleep(Duration::from_millis(50)).await;
    port
}

/// Connect a browser WS to /native/ws, send one message, and harvest the
/// connection id (the GatewayEvent.channel.id) by reading it off the OAB side.
/// Returns (browser_ws, oab_ws, conn_id).
async fn connect_browser_and_get_conn_id(
    port: u16,
) -> (
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>,
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>,
    String,
) {
    // OAB side first so it is subscribed before the event is broadcast.
    let oab_url = format!("ws://127.0.0.1:{port}/ws");
    let (mut oab_ws, _) = connect_async(&oab_url).await.expect("OAB /ws connect failed");
    tokio::time::sleep(Duration::from_millis(50)).await;

    let browser_url = format!("ws://127.0.0.1:{port}/native/ws");
    let (mut browser_ws, _) = connect_async(&browser_url)
        .await
        .expect("browser /native/ws connect failed");

    browser_ws
        .send(TMsg::Text(r#"{"text":"hello from browser"}"#.to_string().into()))
        .await
        .unwrap();

    // The OAB side should receive the GatewayEvent; extract channel.id.
    let raw = loop {
        let msg = tokio::time::timeout(Duration::from_secs(3), oab_ws.next())
            .await
            .expect("timeout waiting for event on OAB /ws")
            .expect("OAB ws stream ended")
            .expect("OAB ws error");
        if let TMsg::Text(t) = msg {
            break t.to_string();
        }
    };
    let v: serde_json::Value = serde_json::from_str(&raw).expect("event must be JSON");
    assert_eq!(v["platform"], "native", "event platform must be native");
    let conn_id = v["channel"]["id"]
        .as_str()
        .expect("channel.id must be a string")
        .to_string();
    (browser_ws, oab_ws, conn_id)
}

/// E1 — native router merged into the main app: the FULL app (via build_app)
/// serves GET /native with 200 and a body that references the WS client script.
#[tokio::test]
async fn e1_full_app_serves_native_ui() {
    let port = spawn_full_app().await;
    let url = format!("http://127.0.0.1:{port}/native");
    let resp = reqwest::get(&url).await.expect("GET /native failed");
    assert_eq!(resp.status(), 200, "GET /native must return 200");
    let body = resp.text().await.unwrap();
    assert!(
        body.contains("new WebSocket"),
        "GET /native body must contain `new WebSocket`"
    );
    assert!(
        body.contains("/native/ws"),
        "GET /native body must reference `/native/ws`"
    );
}

/// E2 + E3 — an outbound platform=native GatewayReply, sent on the OAB-side
/// `/ws`, traverses the main reply loop's native branch (shared native_senders
/// + native::dispatch_reply) and is pushed to the matching browser WS.
/// The browser actually receiving `{"type":"message","text":...}` is the
/// behavioral proof that the reply did NOT fall into `other => unknown`.
#[tokio::test]
async fn e2_e3_native_reply_round_trips_through_full_app() {
    let port = spawn_full_app().await;
    let (mut browser_ws, mut oab_ws, conn_id) = connect_browser_and_get_conn_id(port).await;

    // OAB sends a GatewayReply targeting that conn_id over the SAME /ws socket.
    let reply = serde_json::json!({
        "schema": "openab.gateway.reply.v1",
        "reply_to": "evt_x",
        "platform": "native",
        "channel": { "id": conn_id, "thread_id": null },
        "content": { "type": "text", "text": "pong-from-oab", "attachments": [] },
        "command": null,
        "request_id": null,
        "quote_message_id": null
    });
    oab_ws
        .send(TMsg::Text(reply.to_string().into()))
        .await
        .unwrap();

    // The browser WS must receive {"type":"message","text":"pong-from-oab"}.
    let pushed = loop {
        let msg = tokio::time::timeout(Duration::from_secs(3), browser_ws.next())
            .await
            .expect("timeout waiting for push to browser (native branch did not deliver)")
            .expect("browser ws stream ended")
            .expect("browser ws error");
        if let TMsg::Text(t) = msg {
            break t.to_string();
        }
    };
    let v: serde_json::Value = serde_json::from_str(&pushed).expect("push must be JSON");
    assert_eq!(v["type"], "message", "push type must be `message`");
    assert_eq!(
        v["text"], "pong-from-oab",
        "browser must receive the reply text routed via the native branch"
    );

    browser_ws.close(None).await.ok();
    oab_ws.close(None).await.ok();
}
