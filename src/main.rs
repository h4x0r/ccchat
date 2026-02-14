use clap::Parser;
use dashmap::DashMap;
use futures_util::StreamExt;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::net::TcpListener;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Instant;
use tokio::process::Command;
use tracing::{debug, error, info, warn};

type BoxError = Box<dyn std::error::Error + Send + Sync>;

fn hash_message(text: &str) -> u64 {
    let mut h = DefaultHasher::new();
    text.hash(&mut h);
    h.finish()
}

// --- Persistent allowed list (~/.config/ccchat/allowed.json) ---

#[derive(Serialize, Deserialize, Default)]
struct PersistedAllowed {
    allowed: Vec<AllowedEntry>,
}

#[derive(Serialize, Deserialize, Clone)]
struct AllowedEntry {
    id: String,
    #[serde(default)]
    name: String,
}

fn config_dir() -> PathBuf {
    let dir = std::env::var("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("."))
        .join(".config")
        .join("ccchat");
    let _ = std::fs::create_dir_all(&dir);
    dir
}

fn allowed_file_path() -> PathBuf {
    config_dir().join("allowed.json")
}

fn load_persisted_allowed() -> PersistedAllowed {
    let path = allowed_file_path();
    match std::fs::read_to_string(&path) {
        Ok(contents) => serde_json::from_str(&contents).unwrap_or_default(),
        Err(_) => PersistedAllowed::default(),
    }
}

fn save_persisted_allowed(data: &PersistedAllowed) {
    let path = allowed_file_path();
    match serde_json::to_string_pretty(data) {
        Ok(json) => {
            if let Err(e) = std::fs::write(&path, json) {
                error!("Failed to save allowed list to {}: {e}", path.display());
            } else {
                debug!("Saved allowed list to {}", path.display());
            }
        }
        Err(e) => error!("Failed to serialize allowed list: {e}"),
    }
}

fn persist_allow(id: &str, name: &str) {
    let mut data = load_persisted_allowed();
    if !data.allowed.iter().any(|e| e.id == id) {
        data.allowed.push(AllowedEntry {
            id: id.to_string(),
            name: name.to_string(),
        });
        save_persisted_allowed(&data);
    }
}

fn persist_revoke(id: &str) {
    let mut data = load_persisted_allowed();
    data.allowed.retain(|e| e.id != id);
    save_persisted_allowed(&data);
}

// --- CLI args ---

#[derive(Parser)]
#[command(name = "ccchat", about = "Claude Code Chat")]
struct Args {
    /// Your Signal account number (e.g., +44...)
    #[arg(long, env = "CCCHAT_ACCOUNT")]
    account: String,

    /// Claude model to use
    #[arg(long, default_value = "opus", env = "CCCHAT_MODEL")]
    model: String,

    /// Max budget per message in USD
    #[arg(long, default_value_t = 5.0, env = "CCCHAT_MAX_BUDGET")]
    max_budget: f64,

    /// signal-cli-api base URL (auto-detected when managed)
    #[arg(long, env = "CCCHAT_API_URL")]
    api_url: Option<String>,

    /// Port for signal-cli-api (0 = auto-select free port)
    #[arg(long, default_value_t = 8080, env = "CCCHAT_PORT")]
    port: u16,
}

// --- State ---

struct State {
    sessions: DashMap<String, SenderState>,
    http: Client,
    api_url: String,
    account: String,
    allowed_ids: DashMap<String, ()>,
    pending_senders: DashMap<String, String>,
    sent_hashes: DashMap<u64, ()>,
    model: String,
    max_budget: f64,
    start_time: Instant,
    message_count: AtomicU64,
    total_cost: AtomicU64, // stored as microdollars
}

struct SenderState {
    session_id: String,
    model: String,
}

impl State {
    fn is_allowed(&self, sender: &str) -> bool {
        !sender.is_empty() && self.allowed_ids.contains_key(sender)
    }

    fn add_cost(&self, cost: f64) {
        let micros = (cost * 1_000_000.0) as u64;
        self.total_cost.fetch_add(micros, Ordering::Relaxed);
    }

    fn total_cost_usd(&self) -> f64 {
        self.total_cost.load(Ordering::Relaxed) as f64 / 1_000_000.0
    }
}

// --- signal-cli-api lifecycle ---

fn find_free_port(preferred: u16) -> u16 {
    if preferred == 0 {
        let listener = TcpListener::bind("127.0.0.1:0").expect("failed to bind ephemeral port");
        return listener.local_addr().unwrap().port();
    }
    for port in preferred..=preferred.saturating_add(100) {
        if TcpListener::bind(("127.0.0.1", port)).is_ok() {
            return port;
        }
    }
    let listener = TcpListener::bind("127.0.0.1:0").expect("failed to bind ephemeral port");
    listener.local_addr().unwrap().port()
}

async fn ensure_signal_cli_api() -> Result<String, BoxError> {
    let check = Command::new("which")
        .arg("signal-cli-api")
        .output()
        .await?;

    if check.status.success() {
        let path = String::from_utf8(check.stdout)?.trim().to_string();
        info!("Found signal-cli-api at {path}");
        return Ok(path);
    }

    info!("signal-cli-api not found, installing via cargo...");
    let install = Command::new("cargo")
        .arg("install")
        .arg("signal-cli-api")
        .status()
        .await?;

    if !install.success() {
        return Err("Failed to install signal-cli-api via cargo install".into());
    }

    let check = Command::new("which")
        .arg("signal-cli-api")
        .output()
        .await?;

    if check.status.success() {
        let path = String::from_utf8(check.stdout)?.trim().to_string();
        info!("Installed signal-cli-api at {path}");
        Ok(path)
    } else {
        Err("signal-cli-api installed but not found in PATH".into())
    }
}

async fn start_signal_cli_api(
    binary: &str,
    port: u16,
) -> Result<(tokio::process::Child, String), BoxError> {
    let listen_addr = format!("127.0.0.1:{port}");
    let api_url = format!("http://{listen_addr}");

    info!("Starting signal-cli-api on {listen_addr}");

    let child = Command::new(binary)
        .arg("--listen")
        .arg(&listen_addr)
        .kill_on_drop(true)
        .spawn()?;

    let client = Client::new();
    let health_url = format!("{api_url}/v1/health");
    for i in 0..120 {
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        match client.get(&health_url).send().await {
            Ok(resp) if resp.status().is_success() => {
                info!("signal-cli-api ready on port {port}");
                return Ok((child, api_url));
            }
            Ok(resp) => debug!("Health check attempt {i}: status {}", resp.status()),
            Err(_) => debug!("Health check attempt {i}: not ready yet"),
        }
    }

    Err(format!("signal-cli-api failed to start on port {port} within 60s").into())
}

// --- Main ---

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "ccchat=info".parse().unwrap()),
        )
        .init();

    let args = Args::parse();

    // Determine API URL: use explicit --api-url, or auto-manage signal-cli-api
    let (_child, api_url) = if let Some(url) = args.api_url {
        info!("Using external signal-cli-api at {url}");
        (None, url)
    } else {
        let binary = match ensure_signal_cli_api().await {
            Ok(b) => b,
            Err(e) => {
                error!("Cannot find or install signal-cli-api: {e}");
                std::process::exit(1);
            }
        };

        let port = find_free_port(args.port);
        if port != args.port {
            warn!("Port {} in use, using port {} instead", args.port, port);
        }

        match start_signal_cli_api(&binary, port).await {
            Ok((child, url)) => (Some(child), url),
            Err(e) => {
                error!("Failed to start signal-cli-api: {e}");
                std::process::exit(1);
            }
        }
    };

    // Load allowed list from persistent file
    let allowed_ids: DashMap<String, ()> = DashMap::new();
    let persisted = load_persisted_allowed();
    for entry in &persisted.allowed {
        info!("Loaded: {} ({})", entry.id, entry.name);
        allowed_ids.insert(entry.id.clone(), ());
    }
    // Account owner is always allowed (for admin commands via Note to Self)
    allowed_ids.insert(args.account.clone(), ());

    // Also resolve the account owner's UUID, since linked devices see
    // Note to Self messages with source=UUID, not sourceNumber.
    let http = Client::new();
    if let Ok(resp) = http
        .get(format!("{api_url}/v1/identities/{}", args.account))
        .send()
        .await
    {
        if let Ok(identities) = resp.json::<Vec<Value>>().await {
            for id in &identities {
                if id["number"].as_str() == Some(&args.account) {
                    if let Some(uuid) = id["uuid"].as_str() {
                        info!("Account owner UUID: {uuid}");
                        allowed_ids.insert(uuid.to_string(), ());
                    }
                }
            }
        }
    }

    let state = Arc::new(State {
        sessions: DashMap::new(),
        http,
        api_url,
        account: args.account,
        allowed_ids,
        pending_senders: DashMap::new(),
        sent_hashes: DashMap::new(),
        model: args.model,
        max_budget: args.max_budget,
        start_time: Instant::now(),
        message_count: AtomicU64::new(0),
        total_cost: AtomicU64::new(0),
    });

    info!("ccchat starting for account {}", state.account);
    info!("Allowed list: {}", allowed_file_path().display());
    info!(
        "Allowed senders: {} (+ account owner)",
        persisted.allowed.len()
    );
    info!("API: {}", state.api_url);

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

// --- WebSocket message loop ---

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

        let parsed: Value = match serde_json::from_str(&text) {
            Ok(v) => v,
            Err(e) => {
                warn!("Failed to parse message: {e}");
                continue;
            }
        };

        // Unwrap JSON-RPC wrapper: signal-cli daemon sends
        // {"jsonrpc":"2.0","method":"receive","params":{"envelope":{...}}}
        let envelope = if parsed.get("params").is_some() {
            &parsed["params"]
        } else {
            &parsed
        };

        // Extract sender: prefer sourceNumber (phone), fall back to source (UUID)
        let source = envelope["envelope"]["sourceNumber"]
            .as_str()
            .or_else(|| envelope["envelope"]["source"].as_str());
        let source = match source {
            Some(s) if !s.is_empty() => s.to_string(),
            _ => continue,
        };

        let message_text = match envelope["envelope"]["dataMessage"]["message"].as_str() {
            Some(m) if !m.is_empty() => m.to_string(),
            _ => continue,
        };

        // Suppress Note to Self echoes
        let msg_hash = hash_message(&message_text);
        if state.sent_hashes.remove(&msg_hash).is_some() {
            debug!("Suppressed echo: {}", truncate(&message_text, 40));
            continue;
        }

        let source_uuid = envelope["envelope"]["sourceUuid"]
            .as_str()
            .unwrap_or_default();
        let source_name = envelope["envelope"]["sourceName"]
            .as_str()
            .unwrap_or("unknown");

        if !state.is_allowed(&source) && !state.is_allowed(source_uuid) {
            let id = if !source_uuid.is_empty() {
                source_uuid.to_string()
            } else {
                source.clone()
            };
            let is_new = !state.pending_senders.contains_key(&id);
            state
                .pending_senders
                .insert(id.clone(), source_name.to_string());
            info!("Blocked message from {source_name} ({id})");

            // Notify account owner via Note to Self (once per new sender)
            if is_new {
                let notify = format!(
                    "New sender blocked: {source_name}\n\
                     Reply to approve:\n\
                     /allow {id}"
                );
                let state = Arc::clone(state);
                let account = state.account.clone();
                tokio::spawn(async move {
                    let _ = send_message(&state, &account, &notify).await;
                });
            }
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

// --- Message handling ---

async fn handle_message(state: &State, sender: &str, text: &str) -> Result<(), BoxError> {
    if let Some(response) = handle_command(state, sender, text) {
        send_message(state, sender, &response).await?;
        return Ok(());
    }

    let _ = set_typing(state, sender, true).await;

    let (session_id, model) = {
        let entry = state.sessions.entry(sender.to_string()).or_insert_with(|| {
            let session_id = uuid::Uuid::new_v4().to_string();
            info!("New session for {sender}: {session_id}");
            SenderState {
                session_id,
                model: state.model.clone(),
            }
        });
        (entry.session_id.clone(), entry.model.clone())
    };

    let result = run_claude(state, text, &session_id, &model).await;

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
        let allowed = state.allowed_ids.len();
        return Some(format!(
            "ccchat status\n\
             Uptime: {hours}h {mins}m\n\
             Messages: {count}\n\
             Active sessions: {sessions}\n\
             Allowed senders: {allowed}\n\
             Total cost: ${cost:.4}"
        ));
    }

    if text == "/pending" {
        if state.pending_senders.is_empty() {
            return Some("No pending senders.".to_string());
        }
        let mut lines = vec!["Blocked senders (reply /allow <id> to approve):".to_string()];
        for entry in state.pending_senders.iter() {
            lines.push(format!("  {} — /allow {}", entry.value(), entry.key()));
        }
        return Some(lines.join("\n"));
    }

    if let Some(id) = text.strip_prefix("/allow ") {
        let id = id.trim().to_string();
        if id.is_empty() {
            return Some("Usage: /allow <id>".to_string());
        }
        let name = state
            .pending_senders
            .remove(&id)
            .map(|(_, n)| n)
            .unwrap_or_default();
        state.allowed_ids.insert(id.clone(), ());
        persist_allow(&id, &name);
        info!("Approved: {id} ({name})");
        let display = if name.is_empty() {
            id.clone()
        } else {
            format!("{id} ({name})")
        };
        return Some(format!(
            "Allowed: {display}\nSaved. They can now send messages."
        ));
    }

    if text == "/allow" {
        return Some(
            "Usage: /allow <id>\n\
             Permanently allows a sender.\n\
             Use /pending to see blocked senders."
                .to_string(),
        );
    }

    if let Some(id) = text.strip_prefix("/revoke ") {
        let id = id.trim().to_string();
        if id.is_empty() {
            return Some("Usage: /revoke <id>".to_string());
        }
        state.allowed_ids.remove(&id);
        persist_revoke(&id);
        state.sessions.remove(&id);
        info!("Revoked: {id}");
        return Some(format!("Revoked: {id}"));
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

// --- Claude CLI ---

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
    let parsed: Value = serde_json::from_str(&stdout).unwrap_or_else(|_| {
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

// --- Signal messaging ---

async fn set_typing(state: &State, recipient: &str, typing: bool) -> Result<(), BoxError> {
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

async fn send_message(state: &State, recipient: &str, message: &str) -> Result<(), BoxError> {
    state.sent_hashes.insert(hash_message(message), ());

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

// --- Helpers ---

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
        let split_at = chunk
            .rfind("\n\n")
            .or_else(|| chunk.rfind('\n'))
            .unwrap_or(max_len);

        let (part, rest) = remaining.split_at(split_at);
        parts.push(part.to_string());
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
