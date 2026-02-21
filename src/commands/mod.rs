mod admin;
mod memory_cmds;
mod scheduling;

use admin::*;
use memory_cmds::*;
use scheduling::*;

// Re-export pub(crate) items so main.rs can access them via `commands::`
pub(crate) use admin::handle_unauthorized;
pub(crate) use scheduling::{deliver_due_cron_jobs, deliver_due_reminders};

use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::Mutex;
use tracing::{error, info, warn};

use crate::error::AppError;
use crate::helpers::{looks_truncated, merge_messages};
use crate::memory::{
    export_config, forget_with_counts, inject_context, memory_status, save_memory,
    store_message_pair,
};
use crate::signal::{classify_attachment, AttachmentType};
use crate::state::{State, TokenBucket};

/// Download and classify attachments, returning file paths and whether audio was found.
pub(crate) async fn download_attachments(
    state: &State,
    reply_to: &str,
    raw_attachments: &[crate::signal::AttachmentInfo],
) -> (Vec<PathBuf>, bool) {
    let mut file_paths = Vec::new();
    let mut has_audio = false;
    for att in raw_attachments {
        match classify_attachment(&att.content_type) {
            AttachmentType::Image | AttachmentType::Document => {
                match state.download_attachment(att).await {
                    Ok(path) => file_paths.push(path),
                    Err(e) => {
                        error!("Failed to download attachment {}: {e}", att.id);
                        let _ = state
                            .send_message(reply_to, &format!("Failed to download attachment: {e}"))
                            .await;
                    }
                }
            }
            AttachmentType::Audio => match state.download_attachment(att).await {
                Ok(path) => {
                    has_audio = true;
                    file_paths.push(path);
                }
                Err(e) => {
                    error!("Failed to download audio {}: {e}", att.id);
                    let _ = state
                        .send_message(reply_to, &format!("Failed to download voice message: {e}"))
                        .await;
                }
            },
            AttachmentType::Other => {
                info!("Unsupported attachment type: {}", att.content_type);
                let _ = state
                    .send_message(
                        reply_to,
                        &format!("Unsupported attachment type: {}", att.content_type),
                    )
                    .await;
            }
        }
    }
    (file_paths, has_audio)
}

/// Buffer a message for debounce; spawn flush timer if needed.
pub(crate) fn buffer_debounced(state: &Arc<State>, reply_to: &str, message_text: &str) {
    {
        let mut entry = state
            .debounce
            .buffers
            .entry(reply_to.to_string())
            .or_insert_with(|| (Vec::new(), Instant::now()));
        entry.0.push(message_text.to_string());
        entry.1 = Instant::now();
    }
    if state
        .debounce
        .active
        .insert(reply_to.to_string(), ())
        .is_none()
    {
        let state = Arc::clone(state);
        let reply_to = reply_to.to_string();
        let debounce_ms = state.config.debounce_ms;
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_millis(debounce_ms)).await;
                let should_flush = state
                    .debounce
                    .buffers
                    .get(&reply_to)
                    .map(|e| e.1.elapsed() >= Duration::from_millis(debounce_ms))
                    .unwrap_or(true);
                if should_flush {
                    break;
                }
            }
            state.debounce.active.remove(&reply_to);
            let messages = state
                .debounce
                .buffers
                .remove(&reply_to)
                .map(|(_, (msgs, _))| msgs)
                .unwrap_or_default();
            if messages.is_empty() {
                return;
            }
            let merged = merge_messages(&messages);
            info!(sender = %reply_to, count = messages.len(), "Debounced messages flushed");
            if let Err(e) = handle_message(&state, &reply_to, &merged, &[]).await {
                error!("Error handling message from {reply_to}: {e}");
                let _ = state.send_message(&reply_to, &format!("Error: {e}")).await;
            }
        });
    }
}

pub(crate) fn handle_command(state: &State, sender: &str, text: &str) -> Option<String> {
    let text = text.trim();
    // /reset and /more are handled in handle_message (need async)
    if text == "/help" {
        return Some(cmd_help());
    }
    if text == "/status" {
        return Some(cmd_status(state, sender));
    }
    if text == "/pending" {
        return Some(cmd_pending(state));
    }
    if let Some(arg) = text.strip_prefix("/allow ") {
        return Some(cmd_allow(state, arg.trim()));
    }
    if text == "/allow" {
        return Some(cmd_allow(state, ""));
    }
    if let Some(id) = text.strip_prefix("/revoke ") {
        return Some(cmd_revoke(state, id.trim()));
    }
    if let Some(model) = text.strip_prefix("/model ") {
        return Some(cmd_model(state, sender, model));
    }
    if text == "/memory" {
        return Some(memory_status(sender));
    }
    if text == "/forget" {
        return Some(forget_with_counts(sender));
    }
    if text == "/search" || text.starts_with("/search ") {
        let query = text.strip_prefix("/search").unwrap_or("").trim();
        return Some(cmd_search(sender, query));
    }
    if text == "/export-config" {
        return Some(export_config(&state.allowed_ids, &state.config.account));
    }
    if text == "/export" {
        return Some(cmd_export(sender));
    }
    if text == "/audit" {
        return Some(cmd_audit());
    }
    if text == "/usage" {
        return Some(cmd_usage(state, sender));
    }
    if text == "/pin" || text.starts_with("/pin ") {
        let label = text.strip_prefix("/pin").unwrap_or("").trim();
        return Some(cmd_pin(sender, label));
    }
    if text == "/pins" {
        return Some(cmd_pins(sender));
    }
    if text == "/recall" || text.starts_with("/recall ") {
        let label = text.strip_prefix("/recall").unwrap_or("").trim();
        return Some(cmd_recall(state, sender, label));
    }
    if text == "/remind" || text.starts_with("/remind ") {
        let arg = text.strip_prefix("/remind").unwrap_or("").trim();
        return Some(cmd_remind(sender, arg));
    }
    if text == "/reminders" {
        return Some(cmd_reminders(sender));
    }
    if text == "/cancel" || text.starts_with("/cancel ") {
        let arg = text.strip_prefix("/cancel").unwrap_or("").trim();
        return Some(cmd_cancel_reminder(sender, arg));
    }
    // Cron commands: check more specific prefixes first to avoid /cron matching /crons etc.
    if text == "/crons" {
        return Some(cmd_crons(sender));
    }
    if text == "/cron-cancel" || text.starts_with("/cron-cancel ") {
        let arg = text.strip_prefix("/cron-cancel").unwrap_or("").trim();
        return Some(cmd_cron_cancel(sender, arg));
    }
    if text == "/cron-pause" || text.starts_with("/cron-pause ") {
        let arg = text.strip_prefix("/cron-pause").unwrap_or("").trim();
        return Some(cmd_cron_pause(sender, arg));
    }
    if text == "/cron-resume" || text.starts_with("/cron-resume ") {
        let arg = text.strip_prefix("/cron-resume").unwrap_or("").trim();
        return Some(cmd_cron_resume(sender, arg));
    }
    if text == "/cron" || text.starts_with("/cron ") {
        let arg = text.strip_prefix("/cron").unwrap_or("").trim();
        return Some(cmd_cron(sender, arg));
    }
    if text == "/every" || text.starts_with("/every ") {
        let arg = text.strip_prefix("/every").unwrap_or("").trim();
        return Some(cmd_every(sender, arg));
    }
    if text == "/daily" || text.starts_with("/daily ") {
        let arg = text.strip_prefix("/daily").unwrap_or("").trim();
        return Some(cmd_daily(sender, arg));
    }
    None
}

async fn handle_more(state: &State, sender: &str) -> Result<(), AppError> {
    if let Some((_, session_id)) = state.session_mgr.truncated_sessions.remove(sender) {
        return handle_continuation(state, sender, &session_id).await;
    }
    state.send_message(sender, "Nothing to continue.").await?;
    Ok(())
}

async fn handle_reset(state: &State, sender: &str) -> Result<(), AppError> {
    if let Some((_, session)) = state.session_mgr.sessions.remove(sender) {
        let model = session.model.clone();
        if let Some(summary) = state
            .claude_runner
            .summarize_session(&session.session_id, &model)
            .await
        {
            save_memory(sender, &summary);
            info!(sender = %sender, "Saved memory on reset");
        }
    }
    state
        .send_message(
            sender,
            "Session reset. Next message starts a fresh conversation.",
        )
        .await
}

/// Returns true if the sender is rate-limited and should not proceed.
async fn check_rate_limit(state: &State, sender: &str) -> Result<bool, AppError> {
    if let Some((cap, rate)) = state.config.rate_limit_config {
        let mut bucket = state
            .rate_limits
            .entry(sender.to_string())
            .or_insert_with(|| TokenBucket::new(cap, rate));
        if !bucket.try_consume() {
            warn!(sender = %sender, "Rate limited");
            state
                .send_message(
                    sender,
                    "Rate limited. Please wait before sending more messages.",
                )
                .await?;
            return Ok(true);
        }
    }
    Ok(false)
}

pub(crate) async fn handle_message(
    state: &State,
    sender: &str,
    text: &str,
    attachments: &[PathBuf],
) -> Result<(), AppError> {
    if text.trim() == "/more" {
        return handle_more(state, sender).await;
    }
    if text.trim() == "/reset" {
        return handle_reset(state, sender).await;
    }
    if let Some(response) = handle_command(state, sender, text) {
        state.send_message(sender, &response).await?;
        return Ok(());
    }
    if check_rate_limit(state, sender).await? {
        return Ok(());
    }

    // ── Prompt injection guard ─────────────────────────────────────────
    if let Some(ref lakera_key) = state.config.lakera_api_key {
        let (decision, msg) = crate::guard::run_guard(text, lakera_key, &state.http).await;
        if decision == crate::guard::GuardDecision::Block {
            state.send_message(sender, msg).await?;
            return Ok(());
        }
    }

    let _ = state.set_typing(sender, true).await;
    let (session_id, model, lock, is_new_session) = state.get_or_create_session(sender);

    let base_prompt = if is_new_session {
        inject_context(sender, text)
    } else {
        text.to_string()
    };
    let recalled = state.pending_recalls.remove(sender).map(|(_, v)| v);
    let prompt = if let Some(ref pin_content) = recalled {
        format!("[Recalled context]\n{pin_content}\n\n[Current message]\n{base_prompt}")
    } else {
        base_prompt
    };

    let _guard = lock.lock().await;
    let system_prompt = state.get_system_prompt(sender);
    let call_start = Instant::now();
    let result = state
        .claude_runner
        .run_claude(
            &prompt,
            &session_id,
            &model,
            attachments,
            sender,
            state.config.max_budget,
            &system_prompt,
        )
        .await;
    state.record_latency(call_start.elapsed().as_millis() as u64);

    for path in attachments {
        if let Err(e) = std::fs::remove_file(path) {
            tracing::debug!("Failed to remove temp file {}: {e}", path.display());
        }
    }
    let _ = state.set_typing(sender, false).await;

    if let Ok((ref response, _)) = result {
        info!(sender = %sender, response_len = response.len(), "Reply sent");
        store_message_pair(sender, text, response, &session_id);

        // Increment session message count and check for auto-summarize
        let should_summarize = {
            if let Some(mut entry) = state.session_mgr.sessions.get_mut(sender) {
                entry.message_count += 1;
                if entry.message_count >= crate::constants::AUTO_SUMMARIZE_THRESHOLD {
                    entry.message_count = 0;
                    true
                } else {
                    false
                }
            } else {
                false
            }
        };
        if should_summarize {
            if let Some(summary) = state
                .claude_runner
                .summarize_session(&session_id, &model)
                .await
            {
                save_memory(sender, &summary);
                info!(sender = %sender, "Auto-summarized mid-conversation");
            }
        }
    }

    send_claude_response(state, sender, result, &session_id, &prompt).await
}

/// Send a Claude response: check truncation, store session for /more if needed, send to user.
/// On error, enqueues the original prompt for background retry.
async fn send_claude_response(
    state: &State,
    sender: &str,
    result: Result<(String, Option<f64>), AppError>,
    session_id: &str,
    original_prompt: &str,
) -> Result<(), AppError> {
    match result {
        Ok((response, cost)) => {
            if let Some(c) = cost {
                state.add_cost(c);
                state.add_sender_cost(sender, c);
                info!(sender = %sender, cost_usd = c, total_cost_usd = state.total_cost_usd(), "Claude call completed");
            }
            if looks_truncated(&response) {
                state
                    .session_mgr
                    .truncated_sessions
                    .insert(sender.to_string(), session_id.to_string());
                let msg =
                    format!("{response}\n\n(Response may be truncated. Send /more to continue.)");
                state.send_long_message(sender, &msg).await?;
            } else {
                state.session_mgr.truncated_sessions.remove(sender);
                state.send_long_message(sender, &response).await?;
            }
            // Send any file references as attachments
            let file_refs = crate::helpers::extract_file_references(&response);
            for file_path in &file_refs {
                match std::fs::read(file_path) {
                    Ok(data) => {
                        let ct = crate::helpers::content_type_from_extension(file_path);
                        let fname = file_path.file_name().unwrap_or_default().to_string_lossy();
                        if let Err(e) = state
                            .signal_api
                            .send_attachment(sender, &data, ct, &fname)
                            .await
                        {
                            warn!(sender = %sender, file = %file_path.display(), "Attachment send failed: {e}");
                        }
                    }
                    Err(e) => {
                        warn!(sender = %sender, file = %file_path.display(), "Failed to read file: {e}")
                    }
                }
            }
            Ok(())
        }
        Err(e) => {
            state.metrics.error_count.fetch_add(1, Ordering::Relaxed);
            crate::webhook::fire_if_configured(
                &state.config.webhook_url,
                "error",
                sender,
                &e.to_string(),
            );
            // Enqueue for background retry
            if !original_prompt.is_empty() {
                if let Ok(qconn) = crate::queue::open_queue_db() {
                    crate::queue::enqueue(&qconn, sender, original_prompt, "[]");
                }
            }
            state
                .send_message(sender, &format!("Claude error: {e}"))
                .await?;
            Ok(())
        }
    }
}

pub(crate) async fn handle_continuation(
    state: &State,
    sender: &str,
    session_id: &str,
) -> Result<(), AppError> {
    let _ = state.set_typing(sender, true).await;

    let model = state
        .session_mgr
        .sessions
        .get(sender)
        .map(|s| s.model.clone())
        .unwrap_or_else(|| state.config.model.clone());

    let lock = state
        .session_mgr
        .sessions
        .get(sender)
        .map(|s| s.lock.clone())
        .unwrap_or_else(|| Arc::new(Mutex::new(())));

    let system_prompt = state.get_system_prompt(sender);
    let _guard = lock.lock().await;
    let result = state
        .claude_runner
        .run_claude(
            "continue from where you left off",
            session_id,
            &model,
            &[],
            sender,
            state.config.max_budget,
            &system_prompt,
        )
        .await;

    let _ = state.set_typing(sender, false).await;

    send_claude_response(
        state,
        sender,
        result,
        session_id,
        "continue from where you left off",
    )
    .await
}

/// Retry pending messages from the queue. Called periodically by the background loop.
pub(crate) async fn retry_pending_messages(state: &State) {
    let Ok(qconn) = crate::queue::open_queue_db() else {
        return;
    };
    for (id, sender, content, _attachments) in crate::queue::get_pending(&qconn) {
        let (session_id, model, lock, _is_new) = state.get_or_create_session(&sender);
        let system_prompt = state.get_system_prompt(&sender);
        let _guard = lock.lock().await;
        match state
            .claude_runner
            .run_claude(
                &content,
                &session_id,
                &model,
                &[],
                &sender,
                state.config.max_budget,
                &system_prompt,
            )
            .await
        {
            Ok(result) => {
                if let Err(e) =
                    send_claude_response(state, &sender, Ok(result), &session_id, "").await
                {
                    warn!("Retry send failed for {sender}: {e}");
                    crate::queue::increment_retry(&qconn, id);
                } else {
                    crate::queue::mark_completed(&qconn, id);
                }
            }
            Err(_) => {
                crate::queue::increment_retry(&qconn, id);
            }
        }
    }
    crate::queue::purge_completed(&qconn);
}

#[cfg(test)]
mod tests;
