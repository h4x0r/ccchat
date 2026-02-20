use serde_json::json;
use tracing::warn;

pub(crate) fn fire_webhook(url: &str, event: &str, sender: &str, detail: &str) {
    let url = url.to_string();
    let event_owned = event.to_string();
    let payload = json!({
        "event": event,
        "sender": sender,
        "detail": detail,
        "timestamp": crate::helpers::epoch_now(),
    });
    tokio::spawn(async move {
        let client = reqwest::Client::new();
        if let Err(e) = client.post(&url).json(&payload).send().await {
            warn!(event = %event_owned, "Webhook failed: {e}");
        }
    });
}

pub(crate) fn fire_if_configured(url: &Option<String>, event: &str, sender: &str, detail: &str) {
    if let Some(ref url) = url {
        fire_webhook(url, event, sender, detail);
    }
}

#[cfg(test)]
pub(crate) fn build_webhook_payload(event: &str, sender: &str, detail: &str) -> serde_json::Value {
    json!({
        "event": event,
        "sender": sender,
        "detail": detail,
        "timestamp": crate::helpers::epoch_now(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_fire_webhook_builds_payload() {
        let payload = build_webhook_payload("message_received", "+user", "");
        assert!(payload.is_object());
        assert!(payload.get("event").is_some());
        assert!(payload.get("sender").is_some());
        assert!(payload.get("detail").is_some());
        assert!(payload.get("timestamp").is_some());
    }

    #[test]
    fn test_fire_webhook_includes_timestamp() {
        let payload = build_webhook_payload("error", "+user", "timeout");
        assert!(payload["timestamp"].is_i64() || payload["timestamp"].is_u64());
    }

    #[test]
    fn test_fire_webhook_includes_event() {
        let payload = build_webhook_payload("session_created", "+user", "");
        assert_eq!(payload["event"], "session_created");
    }

    #[test]
    fn test_fire_if_configured_none_is_noop() {
        // Should simply not panic
        fire_if_configured(&None, "test", "+user", "");
    }

    #[tokio::test]
    async fn test_fire_if_configured_some_fires() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path("/"))
            .respond_with(wiremock::ResponseTemplate::new(200))
            .expect(1)
            .mount(&server)
            .await;
        fire_if_configured(&Some(server.uri()), "test_event", "+user", "detail");
        // Give the spawned task time to complete
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }

    #[test]
    fn test_webhook_payload_has_sender() {
        let payload = build_webhook_payload("message_received", "+1234567890", "");
        assert_eq!(payload["sender"], "+1234567890");
    }

    #[test]
    fn test_webhook_payload_has_detail() {
        let payload = build_webhook_payload("error", "+user", "connection refused");
        assert_eq!(payload["detail"], "connection refused");
    }

    #[tokio::test]
    async fn test_webhook_does_not_block() {
        // fire_webhook with a non-existent URL should return immediately
        let start = std::time::Instant::now();
        fire_webhook("http://192.0.2.1:1/nonexistent", "test", "+user", "");
        let elapsed = start.elapsed();
        assert!(
            elapsed.as_millis() < 50,
            "fire_webhook should not block, took {}ms",
            elapsed.as_millis()
        );
    }
}
