use anyhow::Result;
use tracing::info;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    let listen_addr = std::env::var("GATEWAY_LISTEN").unwrap_or_else(|_| "0.0.0.0:8080".into());

    let (app, _handle) = openab_gateway::build_app().await;

    info!(addr = %listen_addr, "gateway starting");
    let listener = tokio::net::TcpListener::bind(&listen_addr).await?;
    axum::serve(listener, app).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use openab_gateway::{adapters, ReplyTokenCache, REPLY_TOKEN_TTL_SECS};
    use openab_gateway::schema;
    use std::collections::HashMap;
    use std::sync::Arc;
    use std::time::{Duration, Instant};
    use wiremock::matchers::{body_json, header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn make_reply(event_id: &str) -> schema::GatewayReply {
        schema::GatewayReply {
            schema: "openab.gateway.reply.v1".into(),
            reply_to: event_id.into(),
            platform: "line".into(),
            channel: schema::ReplyChannel {
                id: "U1234".into(),
                thread_id: None,
            },
            content: schema::Content {
                content_type: "text".into(),
                text: "hello".into(),
                attachments: Vec::new(),
            },
            command: None,
            request_id: None,
            quote_message_id: None,
        }
    }

    fn make_reply_with_command(event_id: &str, command: &str, text: &str) -> schema::GatewayReply {
        schema::GatewayReply {
            schema: "openab.gateway.reply.v1".into(),
            reply_to: event_id.into(),
            platform: "line".into(),
            channel: schema::ReplyChannel {
                id: "U1234".into(),
                thread_id: None,
            },
            content: schema::Content {
                content_type: "text".into(),
                text: text.into(),
                attachments: Vec::new(),
            },
            command: Some(command.into()),
            request_id: None,
            quote_message_id: None,
        }
    }

    fn make_cache() -> ReplyTokenCache {
        Arc::new(std::sync::Mutex::new(HashMap::new()))
    }

    /// Cache hit: uses Reply API with correct replyToken, bearer token, and message body.
    /// Does NOT call Push API.
    #[tokio::test]
    async fn cache_hit_uses_reply_api() {
        let server = MockServer::start().await;
        let _reply = Mock::given(method("POST"))
            .and(path("/v2/bot/message/reply"))
            .and(header("authorization", "Bearer test_access_token"))
            .and(body_json(serde_json::json!({
                "replyToken": "tok_abc",
                "messages": [{"type": "text", "text": "hello"}]
            })))
            .respond_with(ResponseTemplate::new(200).set_body_string("{}"))
            .expect(1)
            .mount_as_scoped(&server)
            .await;
        let _push = Mock::given(method("POST"))
            .and(path("/v2/bot/message/push"))
            .respond_with(ResponseTemplate::new(200))
            .expect(0)
            .mount_as_scoped(&server)
            .await;

        let cache = make_cache();
        cache
            .lock()
            .unwrap()
            .insert("evt_1".into(), ("tok_abc".into(), Instant::now()));

        let client = reqwest::Client::new();
        let used = adapters::line::dispatch_line_reply(
            &client,
            "test_access_token",
            &cache,
            &make_reply("evt_1"),
            &server.uri(),
        )
        .await;

        assert!(used, "should report Reply API was used");
    }

    /// All unsupported LINE commands should be ignored without consuming the cached reply token.
    #[tokio::test]
    async fn line_ignores_unsupported_commands_without_touching_cache() {
        let unsupported = &["add_reaction", "remove_reaction", "create_topic"];

        for cmd in unsupported {
            let server = MockServer::start().await;
            let _reply = Mock::given(method("POST"))
                .and(path("/v2/bot/message/reply"))
                .respond_with(ResponseTemplate::new(200))
                .expect(0)
                .mount_as_scoped(&server)
                .await;
            let _push = Mock::given(method("POST"))
                .and(path("/v2/bot/message/push"))
                .respond_with(ResponseTemplate::new(200))
                .expect(0)
                .mount_as_scoped(&server)
                .await;

            let cache = make_cache();
            cache
                .lock()
                .unwrap()
                .insert("evt_unsup".into(), ("tok_unsup".into(), Instant::now()));

            let client = reqwest::Client::new();
            let used = adapters::line::dispatch_line_reply(
                &client,
                "test_access_token",
                &cache,
                &make_reply_with_command("evt_unsup", cmd, "payload"),
                &server.uri(),
            )
            .await;

            assert!(!used, "{cmd}: should not report reply usage");
            assert!(
                cache.lock().unwrap().contains_key("evt_unsup"),
                "{cmd}: should not consume the cached reply token"
            );
        }
    }

    /// Cache miss: falls back to Push API with correct "to", bearer token, and message body.
    #[tokio::test]
    async fn cache_miss_uses_push_api() {
        let server = MockServer::start().await;
        let _reply = Mock::given(method("POST"))
            .and(path("/v2/bot/message/reply"))
            .respond_with(ResponseTemplate::new(200))
            .expect(0)
            .mount_as_scoped(&server)
            .await;
        let _push = Mock::given(method("POST"))
            .and(path("/v2/bot/message/push"))
            .and(header("authorization", "Bearer test_access_token"))
            .and(body_json(serde_json::json!({
                "to": "U1234",
                "messages": [{"type": "text", "text": "hello"}]
            })))
            .respond_with(ResponseTemplate::new(200).set_body_string("{}"))
            .expect(1)
            .mount_as_scoped(&server)
            .await;

        let cache = make_cache();

        let client = reqwest::Client::new();
        let used = adapters::line::dispatch_line_reply(
            &client,
            "test_access_token",
            &cache,
            &make_reply("evt_miss"),
            &server.uri(),
        )
        .await;

        assert!(!used, "should report Push API was used (no reply token)");
    }

    /// Expired cached token: falls back to Push API.
    #[tokio::test]
    async fn expired_token_uses_push_api() {
        let server = MockServer::start().await;
        let _reply = Mock::given(method("POST"))
            .and(path("/v2/bot/message/reply"))
            .respond_with(ResponseTemplate::new(200))
            .expect(0)
            .mount_as_scoped(&server)
            .await;
        let _push = Mock::given(method("POST"))
            .and(path("/v2/bot/message/push"))
            .and(header("authorization", "Bearer test_access_token"))
            .and(body_json(serde_json::json!({
                "to": "U1234",
                "messages": [{"type": "text", "text": "hello"}]
            })))
            .respond_with(ResponseTemplate::new(200).set_body_string("{}"))
            .expect(1)
            .mount_as_scoped(&server)
            .await;

        let cache = make_cache();
        let expired_time = Instant::now() - Duration::from_secs(REPLY_TOKEN_TTL_SECS + 10);
        cache
            .lock()
            .unwrap()
            .insert("evt_exp".into(), ("tok_old".into(), expired_time));

        let client = reqwest::Client::new();
        let used = adapters::line::dispatch_line_reply(
            &client,
            "test_access_token",
            &cache,
            &make_reply("evt_exp"),
            &server.uri(),
        )
        .await;

        assert!(!used, "should report Push API was used (expired token)");
    }

    /// Reply API 400 with invalid/expired reply token: falls back to Push API.
    #[tokio::test]
    async fn reply_400_invalid_token_falls_back_to_push() {
        let server = MockServer::start().await;
        let _reply = Mock::given(method("POST"))
            .and(path("/v2/bot/message/reply"))
            .and(header("authorization", "Bearer test_access_token"))
            .respond_with(
                ResponseTemplate::new(400).set_body_string(r#"{"message":"Invalid reply token"}"#),
            )
            .expect(1)
            .mount_as_scoped(&server)
            .await;
        let _push = Mock::given(method("POST"))
            .and(path("/v2/bot/message/push"))
            .and(header("authorization", "Bearer test_access_token"))
            .and(body_json(serde_json::json!({
                "to": "U1234",
                "messages": [{"type": "text", "text": "hello"}]
            })))
            .respond_with(ResponseTemplate::new(200).set_body_string("{}"))
            .expect(1)
            .mount_as_scoped(&server)
            .await;

        let cache = make_cache();
        cache
            .lock()
            .unwrap()
            .insert("evt_400".into(), ("tok_bad".into(), Instant::now()));

        let client = reqwest::Client::new();
        let used = adapters::line::dispatch_line_reply(
            &client,
            "test_access_token",
            &cache,
            &make_reply("evt_400"),
            &server.uri(),
        )
        .await;

        assert!(!used, "should fall back to Push on 400 invalid token");
    }

    /// Reply API 5xx: does NOT fall back to Push (duplicate risk).
    #[tokio::test]
    async fn reply_5xx_does_not_fallback() {
        let server = MockServer::start().await;
        let _reply = Mock::given(method("POST"))
            .and(path("/v2/bot/message/reply"))
            .and(header("authorization", "Bearer test_access_token"))
            .respond_with(ResponseTemplate::new(500).set_body_string("Internal Server Error"))
            .expect(1)
            .mount_as_scoped(&server)
            .await;
        let _push = Mock::given(method("POST"))
            .and(path("/v2/bot/message/push"))
            .respond_with(ResponseTemplate::new(200))
            .expect(0)
            .mount_as_scoped(&server)
            .await;

        let cache = make_cache();
        cache
            .lock()
            .unwrap()
            .insert("evt_5xx".into(), ("tok_5xx".into(), Instant::now()));

        let client = reqwest::Client::new();
        let used = adapters::line::dispatch_line_reply(
            &client,
            "test_access_token",
            &cache,
            &make_reply("evt_5xx"),
            &server.uri(),
        )
        .await;

        assert!(used, "should NOT fall back to Push on 5xx");
    }

    /// Reply API network/timeout error: does NOT fall back to Push (duplicate risk).
    #[tokio::test]
    async fn reply_network_error_does_not_fallback() {
        let bad_base = "http://127.0.0.1:1";

        let cache = make_cache();
        cache
            .lock()
            .unwrap()
            .insert("evt_net".into(), ("tok_net".into(), Instant::now()));

        let client = reqwest::Client::builder()
            .timeout(Duration::from_millis(100))
            .build()
            .unwrap();
        let used = adapters::line::dispatch_line_reply(
            &client,
            "test_access_token",
            &cache,
            &make_reply("evt_net"),
            bad_base,
        )
        .await;

        assert!(used, "should NOT fall back to Push on network error");
    }
}
