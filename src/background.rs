use std::sync::Arc;
use std::time::{Duration, Instant};
use tracing::{error, info};

use crate::memory::{open_memory_db, purge_old_messages, reload_config_full, save_memory};
use crate::state::State;
use crate::{audit, commands, webhook};

pub(crate) fn spawn_session_reaper(state: &Arc<State>, ttl: Duration) {
    let state = Arc::clone(state);
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_secs(60)).await;
            let now = Instant::now();
            let expired: Vec<(String, String)> = state
                .session_mgr
                .sessions
                .iter()
                .filter(|entry| now.duration_since(entry.value().last_activity) > ttl)
                .map(|entry| (entry.key().clone(), entry.value().session_id.clone()))
                .collect();
            for (sender, session_id) in &expired {
                info!(sender = %sender, "Session expired by TTL reaper");
                if let Some(summary) = state
                    .claude_runner
                    .summarize_session(session_id, &state.config.model)
                    .await
                {
                    save_memory(sender, &summary);
                    info!(sender = %sender, "Saved memory on expiry");
                }
                if let Ok(conn) = open_memory_db(sender) {
                    purge_old_messages(&conn, 30);
                }
                state.session_mgr.sessions.remove(sender);
            }
        }
    });
}

#[cfg(unix)]
pub(crate) fn spawn_sighup_handler(state: &Arc<State>) {
    let state = Arc::clone(state);
    tokio::spawn(async move {
        let mut sig = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::hangup())
            .expect("Failed to register SIGHUP handler");
        loop {
            sig.recv().await;
            info!("SIGHUP received, reloading config...");
            let (added, removed) = reload_config_full(
                state.config.config_path.as_deref(),
                &state.config.account,
                &state.allowed_ids,
                &state.runtime_system_prompt,
                &state.sender_prompts,
            );
            audit::log_action("config_reload", "", &format!("+{added} -{removed}"));
            info!("Config reloaded: +{added} -{removed} senders");
        }
    });
}

pub(crate) fn spawn_shutdown_handler(state: &Arc<State>) {
    // SIGINT / Ctrl-C
    {
        let state = Arc::clone(state);
        tokio::spawn(async move {
            tokio::signal::ctrl_c()
                .await
                .expect("Failed to listen for ctrl-c");
            audit::log_action("shutdown", "", "graceful");
            webhook::fire_if_configured(&state.config.webhook_url, "shutdown", "", "graceful");
            info!("Shutdown signal received, saving active sessions...");
            match tokio::time::timeout(Duration::from_secs(30), state.shutdown_save_sessions())
                .await
            {
                Ok(()) => info!("Shutdown complete, all sessions saved"),
                Err(_) => error!("Shutdown timed out after 30s"),
            }
            std::process::exit(0);
        });
    }

    // SIGTERM (Unix only)
    #[cfg(unix)]
    {
        let state = Arc::clone(state);
        tokio::spawn(async move {
            let mut sig = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                .expect("Failed to register SIGTERM handler");
            sig.recv().await;
            audit::log_action("shutdown", "", "SIGTERM");
            info!("SIGTERM received, saving active sessions...");
            match tokio::time::timeout(Duration::from_secs(30), state.shutdown_save_sessions())
                .await
            {
                Ok(()) => info!("Shutdown complete, all sessions saved"),
                Err(_) => error!("Shutdown timed out after 30s"),
            }
            std::process::exit(0);
        });
    }
}

pub(crate) fn spawn_retry_loop(state: &Arc<State>) {
    let state = Arc::clone(state);
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_secs(30)).await;
            commands::retry_pending_messages(&state).await;
        }
    });
}

pub(crate) fn spawn_reminder_loop(state: &Arc<State>) {
    let state = Arc::clone(state);
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_secs(30)).await;
            commands::deliver_due_reminders(&state).await;
        }
    });
}

pub(crate) fn spawn_cron_loop(state: &Arc<State>) {
    let state = Arc::clone(state);
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_secs(30)).await;
            commands::deliver_due_cron_jobs(&state).await;
        }
    });
}

pub(crate) async fn spawn_stats_server(state: &Arc<State>, port: u16) {
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", port))
        .await
        .expect("Failed to bind stats port");
    let state = Arc::clone(state);
    tokio::spawn(crate::stats::run_stats_server(listener, state));
}
