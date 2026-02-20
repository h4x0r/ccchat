mod audit;
mod commands;
mod constants;
mod error;
mod helpers;
mod memory;
mod queue;
mod signal;
mod state;
mod stats;
mod traits;

use clap::Parser;
use dashmap::DashMap;
use futures_util::StreamExt;
use reqwest::Client;
use serde_json::Value;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::process::Command;
use tracing::{debug, error, info, warn};

use commands::{buffer_debounced, download_attachments, handle_message, handle_unauthorized};
use helpers::{find_free_port, is_command, truncate, voice_prompt};
use memory::{
    allowed_file_path, load_config_file, load_persisted_allowed, open_memory_db,
    purge_old_messages, reload_config_full, save_memory, validate_config_entries,
};
use signal::{parse_envelope, ParsedEnvelope};
use error::AppError;
use state::State;
use traits::{ClaudeRunnerImpl, SignalApiImpl};

pub(crate) const NO_MEMORY_PROMPT: &str = "IMPORTANT: Do not write to CLAUDE.md, memory files, or any persistent storage. This is a multi-user bot environment. Memory writes would contaminate other users' sessions. Use the conversation context provided to you instead.";

// --- CLI args ---

#[derive(Parser)]
#[command(name = "ccchat", about = "Claude Code Chat")]
struct Args {
    /// Your Signal account number (e.g., +44...)
    #[arg(long, env = "CCCHAT_ACCOUNT")]
    account: String,

    /// Claude model to use
    #[arg(long, default_value = constants::DEFAULT_MODEL, env = "CCCHAT_MODEL")]
    model: String,

    /// Max budget per message in USD
    #[arg(long, default_value_t = constants::DEFAULT_MAX_BUDGET, env = "CCCHAT_MAX_BUDGET")]
    max_budget: f64,

    /// signal-cli-api base URL (auto-detected when managed)
    #[arg(long, env = "CCCHAT_API_URL")]
    api_url: Option<String>,

    /// Port for signal-cli-api (0 = auto-select free port)
    #[arg(long, default_value_t = constants::DEFAULT_PORT, env = "CCCHAT_PORT")]
    port: u16,

    /// Rate limit per sender (e.g., "5/min", "10/hour", "1/sec")
    #[arg(long, env = "CCCHAT_RATE_LIMIT")]
    rate_limit: Option<String>,

    /// Session TTL / auto-expiry (e.g., "4h", "30m", "1d"). 0 or omit to disable.
    #[arg(long, env = "CCCHAT_SESSION_TTL")]
    session_ttl: Option<String>,

    /// Debounce window in ms to merge burst messages (0 = disabled)
    #[arg(long, default_value_t = constants::DEFAULT_DEBOUNCE_MS, env = "CCCHAT_DEBOUNCE_MS")]
    debounce_ms: u64,

    /// Log output format: "text" (default) or "json" (Axiom-compatible NDJSON)
    #[arg(long, default_value = "text", env = "CCCHAT_LOG_FORMAT")]
    log_format: String,

    /// Path to config file with pre-loaded allowed numbers (JSON or YAML)
    #[arg(long, env = "CCCHAT_CONFIG")]
    config: Option<String>,

    /// Port for stats HTTP endpoint (0 = disabled)
    #[arg(long, default_value_t = 0, env = "CCCHAT_STATS_PORT")]
    stats_port: u16,
}

// --- signal-cli-api lifecycle ---

async fn ensure_signal_cli_api() -> Result<String, AppError> {
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

async fn kill_stale_processes() {
    // Kill any existing signal-cli-api processes (which also spawn signal-cli daemons)
    let output = Command::new("pgrep")
        .arg("-f")
        .arg("signal-cli-api")
        .output()
        .await;

    if let Ok(out) = output {
        let pids = String::from_utf8_lossy(&out.stdout);
        for pid in pids.lines() {
            if let Ok(pid) = pid.trim().parse::<u32>() {
                info!("Killing stale signal-cli-api (pid {pid})");
                let _ = Command::new("kill").arg(pid.to_string()).status().await;
            }
        }
    }

    // Kill any orphaned signal-cli daemons
    let output = Command::new("pgrep")
        .arg("-f")
        .arg("signal-cli.*daemon")
        .output()
        .await;

    if let Ok(out) = output {
        let pids = String::from_utf8_lossy(&out.stdout);
        for pid in pids.lines() {
            if let Ok(pid) = pid.trim().parse::<u32>() {
                info!("Killing stale signal-cli daemon (pid {pid})");
                let _ = Command::new("kill").arg(pid.to_string()).status().await;
            }
        }
    }

    // Brief pause to let processes release the config lock
    tokio::time::sleep(std::time::Duration::from_secs(1)).await;
}

async fn start_signal_cli_api(
    binary: &str,
    port: u16,
) -> Result<(tokio::process::Child, String), AppError> {
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

    Err(AppError::Signal(format!("signal-cli-api failed to start on port {port} within 60s")))
}

// --- Main ---

#[tokio::main]
async fn main() {
    let args = Args::parse();

    let env_filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| "ccchat=info".parse().unwrap());

    match args.log_format.as_str() {
        "json" => {
            tracing_subscriber::fmt()
                .with_env_filter(env_filter)
                .json()
                .flatten_event(true)
                .with_current_span(false)
                .init();
        }
        _ => {
            tracing_subscriber::fmt().with_env_filter(env_filter).init();
        }
    }

    // Determine API URL: use explicit --api-url, or auto-manage signal-cli-api
    let (_child, api_url) = if let Some(url) = args.api_url {
        info!("Using external signal-cli-api at {url}");
        (None, url)
    } else {
        kill_stale_processes().await;

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
    // Load config file if provided
    if let Some(config_path) = &args.config {
        match load_config_file(config_path) {
            Ok(entries) => {
                for warning in validate_config_entries(&entries) {
                    warn!("Config validation: {warning}");
                }
                for entry in &entries {
                    info!(id = %entry.id, name = %entry.name, "Loaded allowed sender from config");
                    allowed_ids.insert(entry.id.clone(), ());
                }
                info!(count = entries.len(), "Loaded allowed senders from config file");
            }
            Err(e) => {
                error!("{e}");
                std::process::exit(1);
            }
        }
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

    let rate_limit_config = args.rate_limit.as_deref().and_then(|s| {
        let parsed = helpers::parse_rate_limit(s);
        if parsed.is_none() {
            error!("Invalid --rate-limit value: {s:?}. Expected format: 5/min, 10/hour, 1/sec");
        }
        parsed
    });

    let session_ttl = args.session_ttl.as_deref().and_then(|s| {
        let parsed = helpers::parse_duration(s);
        if parsed.is_none() && s != "0" {
            error!("Invalid --session-ttl value: {s:?}. Expected format: 30s, 5m, 4h, 1d");
        }
        parsed
    });

    let sent_hashes = Arc::new(DashMap::new());
    let signal_api = Box::new(SignalApiImpl {
        http,
        api_url: api_url.clone(),
        account: args.account.clone(),
    });

    let state = Arc::new(State {
        config: state::Config {
            model: args.model,
            max_budget: args.max_budget,
            rate_limit_config,
            session_ttl,
            debounce_ms: args.debounce_ms,
            account: args.account,
            api_url,
            config_path: args.config,
            system_prompt: None,
        },
        metrics: state::Metrics {
            start_time: Instant::now(),
            message_count: AtomicU64::new(0),
            total_cost: AtomicU64::new(0),
            error_count: AtomicU64::new(0),
            latency_sum_ms: AtomicU64::new(0),
            latency_count: AtomicU64::new(0),
        },
        session_mgr: state::SessionManager {
            sessions: DashMap::new(),
            truncated_sessions: DashMap::new(),
        },
        debounce: state::DebounceState {
            buffers: DashMap::new(),
            active: DashMap::new(),
        },
        allowed_ids,
        pending_senders: DashMap::new(),
        pending_counter: AtomicU64::new(0),
        sent_hashes,
        rate_limits: DashMap::new(),
        sender_costs: DashMap::new(),
        sender_prompts: DashMap::new(),
        runtime_system_prompt: std::sync::RwLock::new(None),
        signal_api,
        claude_runner: Box::new(ClaudeRunnerImpl),
    });

    // Spawn session reaper task if TTL is configured
    if let Some(ttl) = state.config.session_ttl {
        let reaper_state = Arc::clone(&state);
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(60)).await;
                let now = Instant::now();
                // Collect expired sessions for async summarization
                let expired: Vec<(String, String)> = reaper_state
                    .session_mgr
                    .sessions
                    .iter()
                    .filter(|entry| now.duration_since(entry.value().last_activity) > ttl)
                    .map(|entry| (entry.key().clone(), entry.value().session_id.clone()))
                    .collect();
                for (sender, session_id) in &expired {
                    info!(sender = %sender, "Session expired by TTL reaper");
                    if let Some(summary) = reaper_state
                        .claude_runner
                        .summarize_session(session_id, &reaper_state.config.model)
                        .await
                    {
                        save_memory(sender, &summary);
                        info!(sender = %sender, "Saved memory on expiry");
                    }
                    // Purge messages older than 30 days
                    if let Ok(conn) = open_memory_db(sender) {
                        purge_old_messages(&conn, 30);
                    }
                    reaper_state.session_mgr.sessions.remove(sender);
                }
            }
        });
    }

    // Spawn SIGHUP handler for config hot-reload
    #[cfg(unix)]
    {
        let reload_state = Arc::clone(&state);
        tokio::spawn(async move {
            let mut sig = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::hangup())
                .expect("Failed to register SIGHUP handler");
            loop {
                sig.recv().await;
                info!("SIGHUP received, reloading config...");
                let (added, removed) = reload_config_full(
                    reload_state.config.config_path.as_deref(),
                    &reload_state.config.account,
                    &reload_state.allowed_ids,
                    &reload_state.runtime_system_prompt,
                    &reload_state.sender_prompts,
                );
                audit::log_action("config_reload", "", &format!("+{added} -{removed}"));
                info!("Config reloaded: +{added} -{removed} senders");
            }
        });
    }

    // Spawn graceful shutdown handler (SIGINT/SIGTERM)
    {
        let shutdown_state = Arc::clone(&state);
        tokio::spawn(async move {
            tokio::signal::ctrl_c()
                .await
                .expect("Failed to listen for ctrl-c");
            audit::log_action("shutdown", "", "graceful");
            info!("Shutdown signal received, saving active sessions...");
            match tokio::time::timeout(
                std::time::Duration::from_secs(30),
                shutdown_state.shutdown_save_sessions(),
            )
            .await
            {
                Ok(()) => info!("Shutdown complete, all sessions saved"),
                Err(_) => error!("Shutdown timed out after 30s"),
            }
            std::process::exit(0);
        });
    }
    #[cfg(unix)]
    {
        let shutdown_state = Arc::clone(&state);
        tokio::spawn(async move {
            let mut sig =
                tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                    .expect("Failed to register SIGTERM handler");
            sig.recv().await;
            audit::log_action("shutdown", "", "SIGTERM");
            info!("SIGTERM received, saving active sessions...");
            match tokio::time::timeout(
                std::time::Duration::from_secs(30),
                shutdown_state.shutdown_save_sessions(),
            )
            .await
            {
                Ok(()) => info!("Shutdown complete, all sessions saved"),
                Err(_) => error!("Shutdown timed out after 30s"),
            }
            std::process::exit(0);
        });
    }

    info!("ccchat starting for account {}", state.config.account);
    info!("Allowed list: {}", allowed_file_path().display());
    info!(
        "Allowed senders: {} (+ account owner)",
        persisted.allowed.len()
    );
    info!("API: {}", state.config.api_url);
    if let Some((cap, rate)) = state.config.rate_limit_config {
        info!("Rate limit: {cap} msgs burst, {rate:.4}/sec refill");
    }
    if let Some(ttl) = state.config.session_ttl {
        info!("Session TTL: {}s", ttl.as_secs());
    }
    if state.config.debounce_ms > 0 {
        info!("Debounce: {}ms", state.config.debounce_ms);
    }

    // Spawn queue retry loop (every 30s)
    {
        let retry_state = Arc::clone(&state);
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(30)).await;
                commands::retry_pending_messages(&retry_state).await;
            }
        });
    }

    // Spawn stats HTTP server if configured
    if args.stats_port > 0 {
        let stats_listener =
            tokio::net::TcpListener::bind(("127.0.0.1", args.stats_port))
                .await
                .expect("Failed to bind stats port");
        let stats_state = state.clone();
        tokio::spawn(stats::run_stats_server(stats_listener, stats_state));
    }

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

async fn connect_and_listen(state: &Arc<State>) -> Result<(), AppError> {
    let ws_url = format!(
        "{}/v1/receive/{}",
        state.config.api_url.replace("http", "ws"),
        state.config.account
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

        let envelope = if parsed.get("params").is_some() {
            &parsed["params"]
        } else {
            &parsed
        };

        let parsed_env = match parse_envelope(envelope) {
            Some(p) => p,
            None => continue,
        };

        let ParsedEnvelope {
            source,
            message_text,
            is_sync,
            source_uuid,
            source_name,
            attachments: raw_attachments,
        } = parsed_env;

        // Suppress Note to Self echoes
        let msg_hash = helpers::hash_message(&message_text);
        if state.sent_hashes.remove(&msg_hash).is_some() {
            debug!("Suppressed echo: {}", truncate(&message_text, 40));
            continue;
        }

        if !is_sync && !state.is_allowed(&source) && !state.is_allowed(&source_uuid) {
            handle_unauthorized(state, &source, &source_name);
            continue;
        }

        state.metrics.message_count.fetch_add(1, Ordering::Relaxed);

        let reply_to = if is_sync {
            state.config.account.clone()
        } else {
            source.clone()
        };

        let has_attachments = !raw_attachments.is_empty();
        if has_attachments {
            info!(sender = %source, attachment_count = raw_attachments.len(), message_type = "attachment", "Incoming message");
        } else {
            info!(sender = %source, message_type = "text", "Incoming message");
        }

        if is_command(&message_text) || state.config.debounce_ms == 0 || has_attachments {
            let state = Arc::clone(state);
            tokio::spawn(async move {
                let (file_paths, has_audio) =
                    download_attachments(&state, &reply_to, &raw_attachments).await;
                let final_text = if has_audio {
                    voice_prompt(&message_text)
                } else {
                    message_text
                };
                if let Err(e) = handle_message(&state, &reply_to, &final_text, &file_paths).await
                {
                    error!("Error handling message from {reply_to}: {e}");
                    let _ = state
                        .send_message(&reply_to, &format!("Error: {e}"))
                        .await;
                }
            });
        } else {
            buffer_debounced(state, &reply_to, &message_text);
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_no_memory_prompt_content() {
        // The prompt must instruct Claude not to write to CLAUDE.md or memory files
        assert!(NO_MEMORY_PROMPT.contains("CLAUDE.md"));
        assert!(NO_MEMORY_PROMPT.contains("memory"));
        assert!(NO_MEMORY_PROMPT.contains("multi-user"));
        assert!(NO_MEMORY_PROMPT.contains("Do not write"));
    }

    // --- Args / --log-format parsing tests ---

    #[test]
    fn test_log_format_defaults_to_text() {
        let args =
            Args::try_parse_from(["ccchat", "--account", "+1234567890"]).expect("parse failed");
        assert_eq!(args.log_format, "text");
    }

    #[test]
    fn test_log_format_json_accepted() {
        let args =
            Args::try_parse_from(["ccchat", "--account", "+1234567890", "--log-format", "json"])
                .expect("parse failed");
        assert_eq!(args.log_format, "json");
    }

    #[test]
    fn test_log_format_text_explicit() {
        let args =
            Args::try_parse_from(["ccchat", "--account", "+1234567890", "--log-format", "text"])
                .expect("parse failed");
        assert_eq!(args.log_format, "text");
    }

    #[test]
    fn test_log_format_arbitrary_value_accepted() {
        // The field is a free-form String, so any value is accepted at the parse level.
        // The match in main() treats anything other than "json" as text (the default branch).
        let args = Args::try_parse_from([
            "ccchat",
            "--account",
            "+1234567890",
            "--log-format",
            "yaml",
        ])
        .expect("parse failed");
        assert_eq!(args.log_format, "yaml");
    }

    #[test]
    fn test_args_account_required() {
        // Parsing without --account should fail
        let result = Args::try_parse_from(["ccchat"]);
        assert!(result.is_err(), "expected error when --account is missing");
    }

    #[test]
    fn test_args_default_values() {
        let args =
            Args::try_parse_from(["ccchat", "--account", "+1234567890"]).expect("parse failed");
        assert_eq!(args.model, "opus");
        assert!((args.max_budget - 5.0).abs() < f64::EPSILON);
        assert_eq!(args.port, 8080);
        assert_eq!(args.debounce_ms, 3000);
        assert_eq!(args.log_format, "text");
        assert!(args.api_url.is_none());
        assert!(args.rate_limit.is_none());
        assert!(args.session_ttl.is_none());
        assert!(args.config.is_none());
    }

    #[test]
    fn test_args_config_flag() {
        let args = Args::try_parse_from([
            "ccchat",
            "--account",
            "+1234567890",
            "--config",
            "/tmp/ccchat.json",
        ])
        .expect("parse failed");
        assert_eq!(args.config, Some("/tmp/ccchat.json".to_string()));
    }

    #[test]
    fn test_args_config_defaults_to_none() {
        let args =
            Args::try_parse_from(["ccchat", "--account", "+1234567890"]).expect("parse failed");
        assert!(args.config.is_none());
    }
}
