use clap::Parser;
use dashmap::DashMap;
use futures_util::StreamExt;
use reqwest::Client;
use serde_json::Value;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Instant;
use tokio::process::Command;
use tracing::{debug, error, info, warn};

type BoxError = Box<dyn std::error::Error + Send + Sync>;

#[derive(Parser)]
#[command(name = "ccchat", about = "Claude Code Chat")]
struct Args {
    /// signal-cli-api base URL
    #[arg(long, default_value = "http://127.0.0.1:8080", env = "CCCHAT_API_URL")]
    api_url: String,

    /// Your Signal account number (e.g., +44...)
    #[arg(long, env = "CCCHAT_ACCOUNT")]
    account: String,

    /// Comma-separated allowed sender numbers (defaults to your own account number)
    #[arg(long, env = "CCCHAT_ALLOWED")]
    allowed: Option<String>,

    /// Claude model to use
    #[arg(long, default_value = "opus", env = "CCCHAT_MODEL")]
    model: String,

    /// Max budget per message in USD
    #[arg(long, default_value_t = 5.0, env = "CCCHAT_MAX_BUDGET")]
    max_budget: f64,
}

struct State {
    sessions: DashMap<String, SenderState>,
    http: Client,
    api_url: String,
    account: String,
    allowed: Vec<String>,
    model: String,
    max_budget: f64,
    start_time: Instant,
    message_count: AtomicU64,
    total_cost: std::sync::atomic::AtomicU64, // stored as microdollars
}

struct SenderState {
    session_id: String,
    model: String,
}

impl State {
    fn is_allowed(&self, sender: &str) -> bool {
        self.allowed.iter().any(|n| n == sender)
    }

    fn add_cost(&self, cost: f64) {
        let micros = (cost * 1_000_000.0) as u64;
        self.total_cost.fetch_add(micros, Ordering::Relaxed);
    }

    fn total_cost_usd(&self) -> f64 {
        self.total_cost.load(Ordering::Relaxed) as f64 / 1_000_000.0
    }
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "ccchat=info".parse().unwrap()),
        )
        .init();

    let args = Args::parse();
    let allowed: Vec<String> = match args.allowed {
        Some(s) => {
            let list: Vec<String> = s
                .split(',')
                .map(|n| n.trim().to_string())
                .filter(|n| !n.is_empty())
                .collect();
            if list.is_empty() {
                vec![args.account.clone()]
            } else {
                list
            }
        }
        None => vec![args.account.clone()],
    };

    let state = Arc::new(State {
        sessions: DashMap::new(),
        http: Client::new(),
        api_url: args.api_url,
        account: args.account,
        allowed,
        model: args.model,
        max_budget: args.max_budget,
        start_time: Instant::now(),
        message_count: AtomicU64::new(0),
        total_cost: std::sync::atomic::AtomicU64::new(0),
    });

    info!("ccchat starting for account {}", state.account);
    info!("Allowed senders: {:?}", state.allowed);

    let mut backoff = 1u64;

    loop {
        match connect_and_listen(&state).await {
            Ok(()) => {
                info!("WebSocket closed cleanly, reconnecting...");
                backoff = 1;
            }
            Err(e) => {
                error!("WebSocket error: {e}, reconnecting in {backoff}s...");
            }
        }
        tokio::time::sleep(std::time::Duration::from_secs(backoff)).await;
        backoff = (backoff * 2).min(60);
    }
}

async fn connect_and_listen(state: &Arc<State>) -> Result<(), BoxError> {
    let ws_url = format!(
        "{}/v1/receive/{}",
        state.api_url.replace("http", "ws"),
        state.account
    );
    info!("Connecting to {ws_url}");

    let (ws, _) = tokio_tungstenite::connect_async(&ws_url).await?;
    info!("WebSocket connected");

    let (_, mut read) = ws.split();

    while let Some(msg) = read.next().await {
        let msg = msg?;
        if !msg.is_text() {
            continue;
        }

        let text = msg.into_text()?;
        debug!("Received: {text}");

        let envelope: Value = match serde_json::from_str(&text) {
            Ok(v) => v,
            Err(e) => {
                warn!("Failed to parse message: {e}");
                continue;
            }
        };

        // Extract sender and message text
        let source = match envelope["envelope"]["source"].as_str() {
            Some(s) => s.to_string(),
            None => continue,
        };

        let message_text = match envelope["envelope"]["dataMessage"]["message"].as_str() {
            Some(m) if !m.is_empty() => m.to_string(),
            _ => continue, // skip receipts, typing indicators, empty messages
        };

        if !state.is_allowed(&source) {
            info!("Ignoring message from non-allowed sender: {source}");
            continue;
        }

        state.message_count.fetch_add(1, Ordering::Relaxed);
        info!("Message from {source}: {}", truncate(&message_text, 80));

        let state = Arc::clone(state);
        let source = source.clone();
        tokio::spawn(async move {
            if let Err(e) = handle_message(&state, &source, &message_text).await {
                error!("Error handling message from {source}: {e}");
                let _ = send_message(&state, &source, &format!("Error: {e}")).await;
            }
        });
    }

    Ok(())
}

async fn handle_message(
    state: &State,
    sender: &str,
    text: &str,
) -> Result<(), BoxError> {
    // Handle bridge-level commands
    if let Some(response) = handle_command(state, sender, text) {
        send_message(state, sender, &response).await?;
        return Ok(());
    }

    // Show typing indicator
    let _ = set_typing(state, sender, true).await;

    // Get or create session for this sender
    let model = {
        let entry = state.sessions.entry(sender.to_string()).or_insert_with(|| {
            let session_id = uuid::Uuid::new_v4().to_string();
            info!("New session for {sender}: {session_id}");
            SenderState {
                session_id,
                model: state.model.clone(),
            }
        });
        let session = entry.value();
        (session.session_id.clone(), session.model.clone())
    };
    let (session_id, model) = model;

    // Run claude CLI
    let result = run_claude(state, text, &session_id, &model).await;

    // Stop typing indicator
    let _ = set_typing(state, sender, false).await;

    match result {
        Ok((response, cost)) => {
            if let Some(c) = cost {
                state.add_cost(c);
                info!("Cost: ${c:.4} (total: ${:.4})", state.total_cost_usd());
            }
            send_long_message(state, sender, &response).await?;
        }
        Err(e) => {
            send_message(state, sender, &format!("Claude error: {e}")).await?;
        }
    }

    Ok(())
}

fn handle_command(state: &State, sender: &str, text: &str) -> Option<String> {
    let text = text.trim();

    if text == "/reset" {
        state.sessions.remove(sender);
        return Some("Session reset. Next message starts a fresh conversation.".to_string());
    }

    if text == "/status" {
        let uptime = state.start_time.elapsed();
        let hours = uptime.as_secs() / 3600;
        let mins = (uptime.as_secs() % 3600) / 60;
        let count = state.message_count.load(Ordering::Relaxed);
        let cost = state.total_cost_usd();
        let sessions = state.sessions.len();
        return Some(format!(
            "ccchat status\nUptime: {hours}h {mins}m\nMessages: {count}\nActive sessions: {sessions}\nTotal cost: ${cost:.4}"
        ));
    }

    if let Some(model) = text.strip_prefix("/model ") {
        let model = model.trim().to_string();
        let mut entry = state
            .sessions
            .entry(sender.to_string())
            .or_insert_with(|| SenderState {
                session_id: uuid::Uuid::new_v4().to_string(),
                model: model.clone(),
            });
        entry.model = model.clone();
        return Some(format!("Model switched to: {model}"));
    }

    None
}

async fn run_claude(
    state: &State,
    prompt: &str,
    session_id: &str,
    model: &str,
) -> Result<(String, Option<f64>), BoxError> {
    let output = Command::new("claude")
        .arg("-p")
        .arg(prompt)
        .arg("--session-id")
        .arg(session_id)
        .arg("--output-format")
        .arg("json")
        .arg("--model")
        .arg(model)
        .arg("--max-budget-usd")
        .arg(state.max_budget.to_string())
        .env_remove("CLAUDE_CODE_ENTRYPOINT")
        .output()
        .await?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("claude exited with {}: {stderr}", output.status).into());
    }

    let stdout = String::from_utf8(output.stdout)?;

    // Parse JSON response to extract result and cost
    let parsed: Value = serde_json::from_str(&stdout).unwrap_or_else(|_| {
        // If not valid JSON, treat entire output as the result
        serde_json::json!({"result": stdout.trim()})
    });

    let result = parsed["result"]
        .as_str()
        .unwrap_or_else(|| stdout.trim())
        .to_string();

    let cost = parsed["cost_usd"]
        .as_f64()
        .or_else(|| parsed["total_cost_usd"].as_f64());

    Ok((result, cost))
}

async fn set_typing(
    state: &State,
    recipient: &str,
    typing: bool,
) -> Result<(), BoxError> {
    let url = format!(
        "{}/v1/typing-indicator/{}",
        state.api_url, state.account
    );

    let body = serde_json::json!({ "recipient": recipient });

    let resp = if typing {
        state.http.put(&url).json(&body).send().await?
    } else {
        state.http.delete(&url).json(&body).send().await?
    };

    if !resp.status().is_success() {
        debug!("Typing indicator failed: {}", resp.status());
    }

    Ok(())
}

async fn send_message(
    state: &State,
    recipient: &str,
    message: &str,
) -> Result<(), BoxError> {
    let url = format!("{}/v2/send", state.api_url);
    let body = serde_json::json!({
        "message": message,
        "number": state.account,
        "recipients": [recipient],
    });

    let resp = state.http.post(&url).json(&body).send().await?;
    if !resp.status().is_success() {
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        error!("Send failed ({status}): {text}");
        return Err(format!("Send failed: {status}").into());
    }

    Ok(())
}

async fn send_long_message(
    state: &State,
    recipient: &str,
    message: &str,
) -> Result<(), BoxError> {
    let parts = split_message(message, 4000);

    for (i, part) in parts.iter().enumerate() {
        if i > 0 {
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        }
        send_message(state, recipient, part).await?;
    }

    Ok(())
}

fn split_message(text: &str, max_len: usize) -> Vec<String> {
    if text.len() <= max_len {
        return vec![text.to_string()];
    }

    let mut parts = Vec::new();
    let mut remaining = text;

    while !remaining.is_empty() {
        if remaining.len() <= max_len {
            parts.push(remaining.to_string());
            break;
        }

        let chunk = &remaining[..max_len];

        // Try to split at paragraph boundary
        let split_at = chunk
            .rfind("\n\n")
            .or_else(|| chunk.rfind('\n'))
            .unwrap_or(max_len);

        let (part, rest) = remaining.split_at(split_at);
        parts.push(part.to_string());

        // Skip the delimiter
        remaining = rest.trim_start_matches('\n');
    }

    parts
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        format!("{}...", &s[..max])
    }
}
