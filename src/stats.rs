use crate::state::State;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use tokio::io::AsyncWriteExt;
use tokio::net::TcpListener;
use tracing::{debug, error, info};

pub(crate) fn build_health_json(state: &State) -> serde_json::Value {
    serde_json::json!({
        "status": "ok",
        "uptime_secs": state.metrics.start_time.elapsed().as_secs(),
        "version": env!("CARGO_PKG_VERSION"),
    })
}

pub(crate) fn build_stats_json(state: &State) -> serde_json::Value {
    let uptime = state.metrics.start_time.elapsed();
    let sender_costs: serde_json::Map<String, serde_json::Value> = state
        .sender_costs
        .iter()
        .map(|e| {
            let cost = e.value().load(Ordering::Relaxed) as f64 / 1_000_000.0;
            (e.key().clone(), serde_json::json!(cost))
        })
        .collect();
    serde_json::json!({
        "uptime_secs": uptime.as_secs(),
        "messages": state.metrics.message_count.load(Ordering::Relaxed),
        "active_sessions": state.session_mgr.sessions.len(),
        "allowed_senders": state.allowed_ids.len(),
        "total_cost_usd": state.total_cost_usd(),
        "error_count": state.metrics.error_count.load(Ordering::Relaxed),
        "avg_latency_ms": state.avg_latency_ms(),
        "sender_costs": sender_costs,
        "model": state.config.model,
        "version": env!("CARGO_PKG_VERSION"),
    })
}

pub(crate) fn build_prometheus_metrics(state: &State) -> String {
    let uptime = state.metrics.start_time.elapsed().as_secs();
    let messages = state.metrics.message_count.load(Ordering::Relaxed);
    let errors = state.metrics.error_count.load(Ordering::Relaxed);
    let cost = state.total_cost_usd();
    let sessions = state.session_mgr.sessions.len();
    let latency = state.avg_latency_ms();
    format!(
        "# HELP ccchat_uptime_seconds Bot uptime in seconds\n\
         # TYPE ccchat_uptime_seconds gauge\n\
         ccchat_uptime_seconds {uptime}\n\
         # HELP ccchat_messages_total Total messages processed\n\
         # TYPE ccchat_messages_total counter\n\
         ccchat_messages_total {messages}\n\
         # HELP ccchat_errors_total Total errors\n\
         # TYPE ccchat_errors_total counter\n\
         ccchat_errors_total {errors}\n\
         # HELP ccchat_cost_usd_total Total cost in USD\n\
         # TYPE ccchat_cost_usd_total gauge\n\
         ccchat_cost_usd_total {cost}\n\
         # HELP ccchat_active_sessions Current active sessions\n\
         # TYPE ccchat_active_sessions gauge\n\
         ccchat_active_sessions {sessions}\n\
         # HELP ccchat_avg_latency_ms Average response latency\n\
         # TYPE ccchat_avg_latency_ms gauge\n\
         ccchat_avg_latency_ms {latency}\n"
    )
}

pub(crate) async fn run_stats_server(listener: TcpListener, state: Arc<State>) {
    info!(addr = %listener.local_addr().unwrap(), "Stats server listening");
    loop {
        let (mut stream, addr) = match listener.accept().await {
            Ok(conn) => conn,
            Err(e) => {
                error!("Stats accept error: {e}");
                continue;
            }
        };
        debug!(peer = %addr, "Stats connection");
        let state = state.clone();
        tokio::spawn(async move {
            let mut buf = [0u8; 1024];
            let _ = tokio::io::AsyncReadExt::read(&mut stream, &mut buf).await;
            let request = String::from_utf8_lossy(&buf);
            let path = request.split_whitespace().nth(1).unwrap_or("/");
            let (body, content_type) = if path == "/healthz" {
                (build_health_json(&state).to_string(), "application/json")
            } else if path == "/metrics" {
                (
                    build_prometheus_metrics(&state),
                    "text/plain; version=0.0.4; charset=utf-8",
                )
            } else {
                (build_stats_json(&state).to_string(), "application/json")
            };
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            let _ = stream.write_all(response.as_bytes()).await;
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::tests::test_state_with;
    use crate::traits::{MockClaudeRunner, MockSignalApi};

    #[test]
    fn test_build_health_json_has_status_ok() {
        let state = test_state_with(MockSignalApi::new(), MockClaudeRunner::new());
        let json = build_health_json(&state);
        assert_eq!(json["status"], "ok");
    }

    #[test]
    fn test_build_health_json_has_uptime() {
        let state = test_state_with(MockSignalApi::new(), MockClaudeRunner::new());
        let json = build_health_json(&state);
        assert!(json.get("uptime_secs").is_some());
        assert!(json["uptime_secs"].is_u64());
    }

    #[test]
    fn test_build_health_json_has_version() {
        let state = test_state_with(MockSignalApi::new(), MockClaudeRunner::new());
        let json = build_health_json(&state);
        assert!(json.get("version").is_some());
        assert!(json["version"].is_string());
        assert!(!json["version"].as_str().unwrap().is_empty());
    }

    #[test]
    fn test_build_stats_json_initially_zero() {
        let state = test_state_with(MockSignalApi::new(), MockClaudeRunner::new());
        let json = build_stats_json(&state);

        assert_eq!(json["messages"], 0);
        assert_eq!(json["active_sessions"], 0);
        assert_eq!(json["total_cost_usd"], 0.0);
    }

    #[test]
    fn test_stats_includes_all_fields() {
        let state = test_state_with(MockSignalApi::new(), MockClaudeRunner::new());
        let json = build_stats_json(&state);

        // All expected fields must be present
        assert!(json.get("uptime_secs").is_some(), "missing uptime_secs");
        assert!(json.get("messages").is_some(), "missing messages");
        assert!(
            json.get("active_sessions").is_some(),
            "missing active_sessions"
        );
        assert!(
            json.get("allowed_senders").is_some(),
            "missing allowed_senders"
        );
        assert!(
            json.get("total_cost_usd").is_some(),
            "missing total_cost_usd"
        );
        assert!(json.get("model").is_some(), "missing model");
        assert!(json.get("version").is_some(), "missing version");
    }

    #[test]
    fn test_stats_json_format() {
        let state = test_state_with(MockSignalApi::new(), MockClaudeRunner::new());
        let json = build_stats_json(&state);

        // Must be a valid JSON object (not array, not scalar)
        assert!(json.is_object());

        // Numeric fields are numbers
        assert!(json["uptime_secs"].is_u64());
        assert!(json["messages"].is_u64());
        assert!(json["active_sessions"].is_u64());
        assert!(json["allowed_senders"].is_u64());
        assert!(json["total_cost_usd"].is_f64());

        // String fields are strings
        assert!(json["model"].is_string());
        assert!(json["version"].is_string());

        // Model reflects the test state
        assert_eq!(json["model"], "sonnet");
    }

    #[test]
    fn test_stats_includes_error_count() {
        let state = test_state_with(MockSignalApi::new(), MockClaudeRunner::new());
        state.metrics.error_count.fetch_add(3, Ordering::Relaxed);
        let json = build_stats_json(&state);
        assert_eq!(json["error_count"], 3);
    }

    #[test]
    fn test_stats_includes_per_sender_costs() {
        let state = test_state_with(MockSignalApi::new(), MockClaudeRunner::new());
        state.add_sender_cost("+alice", 0.05);
        state.add_sender_cost("+bob", 0.10);
        let json = build_stats_json(&state);
        let costs = json["sender_costs"].as_object().unwrap();
        assert_eq!(costs.len(), 2);
        assert!((costs["+alice"].as_f64().unwrap() - 0.05).abs() < 0.001);
        assert!((costs["+bob"].as_f64().unwrap() - 0.10).abs() < 0.001);
    }

    #[test]
    fn test_stats_reflects_state_mutations() {
        let state = test_state_with(MockSignalApi::new(), MockClaudeRunner::new());

        // Initial state
        let json = build_stats_json(&state);
        assert_eq!(json["messages"], 0);
        assert_eq!(json["total_cost_usd"], 0.0);
        assert_eq!(json["active_sessions"], 0);

        // Mutate: add messages
        state.metrics.message_count.fetch_add(42, Ordering::Relaxed);
        let json = build_stats_json(&state);
        assert_eq!(json["messages"], 42);

        // Mutate: add cost
        state.add_cost(1.23);
        let json = build_stats_json(&state);
        assert!((json["total_cost_usd"].as_f64().unwrap() - 1.23).abs() < 0.001);

        // Mutate: add a session
        state.session_mgr.sessions.insert(
            "+someone".to_string(),
            crate::state::SenderState {
                session_id: "sess-1".to_string(),
                model: "sonnet".to_string(),
                lock: Arc::new(tokio::sync::Mutex::new(())),
                last_activity: std::time::Instant::now(),
                message_count: 0,
            },
        );
        let json = build_stats_json(&state);
        assert_eq!(json["active_sessions"], 1);
    }

    #[tokio::test]
    async fn test_stats_server_responds_with_json() {
        let state = test_state_with(MockSignalApi::new(), MockClaudeRunner::new());
        state.metrics.message_count.store(7, Ordering::Relaxed);
        state.add_cost(0.42);

        let state = Arc::new(state);
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        // Spawn the stats server
        let server_state = state.clone();
        tokio::spawn(run_stats_server(listener, server_state));

        // Connect and send a minimal HTTP request
        let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();
        tokio::io::AsyncWriteExt::write_all(
            &mut stream,
            b"GET /stats HTTP/1.1\r\nHost: localhost\r\n\r\n",
        )
        .await
        .unwrap();

        // Read response
        let mut response = Vec::new();
        tokio::io::AsyncReadExt::read_to_end(&mut stream, &mut response)
            .await
            .unwrap();
        let response_str = String::from_utf8(response).unwrap();

        // Verify HTTP response structure
        assert!(
            response_str.starts_with("HTTP/1.1 200 OK"),
            "Expected 200 OK, got: {}",
            &response_str[..40.min(response_str.len())]
        );
        assert!(response_str.contains("Content-Type: application/json"));

        // Extract JSON body (after \r\n\r\n)
        let body = response_str
            .split("\r\n\r\n")
            .nth(1)
            .expect("no body in response");
        let json: serde_json::Value = serde_json::from_str(body).expect("invalid JSON in body");

        assert_eq!(json["messages"], 7);
        assert!((json["total_cost_usd"].as_f64().unwrap() - 0.42).abs() < 0.001);
        assert_eq!(json["model"], "sonnet");
        assert!(json["version"].is_string());
    }

    // --- Prometheus metrics tests ---

    #[test]
    fn test_prometheus_metrics_has_uptime() {
        let state = test_state_with(MockSignalApi::new(), MockClaudeRunner::new());
        let metrics = build_prometheus_metrics(&state);
        assert!(metrics.contains("ccchat_uptime_seconds"), "got: {metrics}");
    }

    #[test]
    fn test_prometheus_metrics_has_messages() {
        let state = test_state_with(MockSignalApi::new(), MockClaudeRunner::new());
        state.metrics.message_count.store(42, Ordering::Relaxed);
        let metrics = build_prometheus_metrics(&state);
        assert!(
            metrics.contains("ccchat_messages_total 42"),
            "got: {metrics}"
        );
    }

    #[test]
    fn test_prometheus_metrics_has_errors() {
        let state = test_state_with(MockSignalApi::new(), MockClaudeRunner::new());
        state.metrics.error_count.store(3, Ordering::Relaxed);
        let metrics = build_prometheus_metrics(&state);
        assert!(metrics.contains("ccchat_errors_total 3"), "got: {metrics}");
    }

    #[test]
    fn test_prometheus_metrics_has_cost() {
        let state = test_state_with(MockSignalApi::new(), MockClaudeRunner::new());
        state.add_cost(1.2345);
        let metrics = build_prometheus_metrics(&state);
        assert!(metrics.contains("ccchat_cost_usd_total"), "got: {metrics}");
    }

    #[test]
    fn test_prometheus_metrics_has_sessions() {
        let state = test_state_with(MockSignalApi::new(), MockClaudeRunner::new());
        let metrics = build_prometheus_metrics(&state);
        assert!(
            metrics.contains("ccchat_active_sessions 0"),
            "got: {metrics}"
        );
    }

    #[test]
    fn test_prometheus_metrics_has_type_annotations() {
        let state = test_state_with(MockSignalApi::new(), MockClaudeRunner::new());
        let metrics = build_prometheus_metrics(&state);
        assert!(metrics.contains("# TYPE"), "missing # TYPE, got: {metrics}");
        assert!(metrics.contains("# HELP"), "missing # HELP, got: {metrics}");
    }

    #[tokio::test]
    async fn test_metrics_endpoint_responds() {
        let state = test_state_with(MockSignalApi::new(), MockClaudeRunner::new());
        let state = Arc::new(state);
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let server_state = state.clone();
        tokio::spawn(run_stats_server(listener, server_state));

        let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();
        tokio::io::AsyncWriteExt::write_all(
            &mut stream,
            b"GET /metrics HTTP/1.1\r\nHost: localhost\r\n\r\n",
        )
        .await
        .unwrap();

        let mut response = Vec::new();
        tokio::io::AsyncReadExt::read_to_end(&mut stream, &mut response)
            .await
            .unwrap();
        let response_str = String::from_utf8(response).unwrap();

        assert!(response_str.starts_with("HTTP/1.1 200 OK"));
        assert!(response_str.contains("text/plain"));
        let body = response_str.split("\r\n\r\n").nth(1).unwrap();
        assert!(body.contains("ccchat_uptime_seconds"));
        assert!(body.contains("ccchat_messages_total"));
    }

    #[tokio::test]
    async fn test_health_endpoint_responds() {
        let state = test_state_with(MockSignalApi::new(), MockClaudeRunner::new());
        let state = Arc::new(state);
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let server_state = state.clone();
        tokio::spawn(run_stats_server(listener, server_state));

        let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();
        tokio::io::AsyncWriteExt::write_all(
            &mut stream,
            b"GET /healthz HTTP/1.1\r\nHost: localhost\r\n\r\n",
        )
        .await
        .unwrap();

        let mut response = Vec::new();
        tokio::io::AsyncReadExt::read_to_end(&mut stream, &mut response)
            .await
            .unwrap();
        let response_str = String::from_utf8(response).unwrap();

        assert!(response_str.starts_with("HTTP/1.1 200 OK"));
        let body = response_str.split("\r\n\r\n").nth(1).unwrap();
        let json: serde_json::Value = serde_json::from_str(body).unwrap();
        assert_eq!(json["status"], "ok");
        assert!(json.get("uptime_secs").is_some());
        assert!(json.get("version").is_some());
        // Should NOT contain full stats fields
        assert!(json.get("messages").is_none());
    }
}
