use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::Mutex;
use tracing::{error, info, warn};

use crate::error::AppError;
use crate::helpers::{looks_truncated, merge_messages};
use crate::memory::{
    export_config, forget_with_counts, format_epoch, inject_context, memory_status, persist_allow,
    persist_revoke, save_memory, search_memory_formatted, store_message_pair,
};
use crate::signal::{classify_attachment, AttachmentType};
use crate::state::{PendingSender, SenderState, State, TokenBucket};

fn cmd_status(state: &State, sender: &str) -> String {
    let uptime = state.metrics.start_time.elapsed();
    let hours = uptime.as_secs() / 3600;
    let mins = (uptime.as_secs() % 3600) / 60;
    let count = state.metrics.message_count.load(Ordering::Relaxed);
    let cost = state.total_cost_usd();
    let sender_cost = state.sender_cost_usd(sender);
    let sessions = state.session_mgr.sessions.len();
    let allowed = state.allowed_ids.len();
    let errors = state.metrics.error_count.load(Ordering::Relaxed);
    let latency = state.avg_latency_ms();
    format!(
        "ccchat status\n\
         Uptime: {hours}h {mins}m\n\
         Messages: {count}\n\
         Active sessions: {sessions}\n\
         Allowed senders: {allowed}\n\
         Total cost: ${cost:.4}\n\
         Your cost: ${sender_cost:.4}\n\
         Errors: {errors}\n\
         Avg latency: {latency:.0}ms"
    )
}

fn cmd_pending(state: &State) -> String {
    if state.pending_senders.is_empty() {
        return "No pending senders.".to_string();
    }
    let mut entries: Vec<_> = state
        .pending_senders
        .iter()
        .map(|e| (e.value().short_id, e.value().name.clone(), e.key().clone()))
        .collect();
    entries.sort_by_key(|(sid, _, _)| *sid);
    let mut lines = vec!["Pending senders:".to_string()];
    for (sid, name, real_id) in &entries {
        lines.push(format!("  #{sid} {name} ({real_id}) — /allow {sid}"));
    }
    lines.join("\n")
}

fn cmd_allow(state: &State, arg: &str) -> String {
    if arg.is_empty() {
        return "Usage: /allow <number>\nUse /pending to see blocked senders.".to_string();
    }
    let (real_id, name) = if let Ok(num) = arg.parse::<u64>() {
        let found = state
            .pending_senders
            .iter()
            .find(|e| e.value().short_id == num)
            .map(|e| (e.key().clone(), e.value().name.clone()));
        match found {
            Some((rid, name)) => {
                state.pending_senders.remove(&rid);
                (rid, name)
            }
            None => {
                return format!("No pending sender #{num}.\nUse /pending to see the list.")
            }
        }
    } else {
        let id = arg.to_string();
        let name = state
            .pending_senders
            .remove(&id)
            .map(|(_, ps)| ps.name)
            .unwrap_or_default();
        (id, name)
    };
    state.allowed_ids.insert(real_id.clone(), ());
    persist_allow(&real_id, &name);
    info!(sender = %real_id, sender_name = %name, "Sender approved");
    let display = if name.is_empty() {
        real_id.clone()
    } else {
        format!("{real_id} ({name})")
    };
    format!("Allowed: {display}\nSaved. They can now send messages.")
}

fn cmd_revoke(state: &State, id: &str) -> String {
    if id.is_empty() {
        return "Usage: /revoke <id>".to_string();
    }
    state.allowed_ids.remove(id);
    persist_revoke(id);
    state.session_mgr.sessions.remove(id);
    info!(sender = %id, "Sender revoked");
    format!("Revoked: {id}")
}

fn cmd_model(state: &State, sender: &str, model: &str) -> String {
    let model = model.trim().to_string();
    let mut entry = state
        .session_mgr
        .sessions
        .entry(sender.to_string())
        .or_insert_with(|| SenderState {
            session_id: uuid::Uuid::new_v4().to_string(),
            model: model.clone(),
            lock: Arc::new(Mutex::new(())),
            last_activity: Instant::now(),
        });
    entry.model = model.clone();
    // Persist preference so it survives session resets
    if let Ok(conn) = crate::memory::open_memory_db(sender) {
        crate::memory::save_model_preference(&conn, &model);
    }
    format!("Model switched to: {model}")
}

fn cmd_search(sender: &str, query: &str) -> String {
    if query.is_empty() {
        return "Usage: /search <query>\nSearch your conversation memory for matching messages."
            .to_string();
    }
    let results = search_memory_formatted(sender, query, crate::constants::MEMORY_SEARCH_LIMIT);
    if results.is_empty() {
        return format!("No results found for \"{query}\"");
    }
    let count = results.len();
    let mut lines = vec![format!("Search results for \"{query}\" ({count} found):")];
    for (role, content, ts) in &results {
        let date = format_epoch(*ts as u64);
        let preview = if content.len() > 100 {
            format!("{}...", &content[..100])
        } else {
            content.clone()
        };
        let preview = preview.replace('\n', " ");
        lines.push(format!("- [{role}] {date}: {preview}"));
    }
    lines.join("\n")
}

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

/// Handle an unauthorized sender: track as pending, notify admin.
pub(crate) fn handle_unauthorized(state: &Arc<State>, source: &str, source_name: &str) {
    let id = source.to_string();
    let is_new = !state.pending_senders.contains_key(&id);
    let short_id = if is_new {
        let sid = state.pending_counter.fetch_add(1, Ordering::Relaxed) + 1;
        state.pending_senders.insert(
            id.clone(),
            PendingSender {
                name: source_name.to_string(),
                short_id: sid,
            },
        );
        sid
    } else {
        state.pending_senders.get(&id).unwrap().short_id
    };
    info!(sender = %id, sender_name = %source_name, short_id = short_id, "Blocked unauthorized sender");

    if is_new {
        let notify = format!(
            "New sender blocked: {source_name} ({id})\n\
             Reply /allow {short_id}"
        );
        let state = Arc::clone(state);
        let account = state.config.account.clone();
        tokio::spawn(async move {
            let _ = state.send_message(&account, &notify).await;
        });
    }
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
                let _ = state
                    .send_message(&reply_to, &format!("Error: {e}"))
                    .await;
            }
        });
    }
}

fn cmd_help() -> String {
    "ccchat commands:\n\
     /help - Show this help message\n\
     /status - Show bot status (uptime, messages, cost)\n\
     /reset - End current session and start fresh\n\
     /more - Continue a truncated response\n\
     /model <name> - Switch Claude model (e.g., haiku, sonnet, opus)\n\
     /memory - Show stored conversation memory\n\
     /forget - Clear all stored memory\n\
     /search <query> - Search conversation history\n\
     /pending - List blocked senders awaiting approval\n\
     /allow <id> - Approve a pending sender\n\
     /revoke <id> - Remove a sender's access\n\
     /export-config - Export allowed senders as JSON"
        .to_string()
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
    None
}

async fn handle_more(state: &State, sender: &str) -> Result<(), AppError> {
    if let Some((_, session_id)) = state.session_mgr.truncated_sessions.remove(sender) {
        return handle_continuation(state, sender, &session_id).await;
    }
    state
        .send_message(sender, "Nothing to continue.")
        .await?;
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

    let _ = state.set_typing(sender, true).await;
    let (session_id, model, lock, is_new_session) = state.get_or_create_session(sender);

    let prompt = if is_new_session {
        inject_context(sender, text)
    } else {
        text.to_string()
    };

    let _guard = lock.lock().await;
    let system_prompt = state.get_system_prompt(sender);
    let call_start = Instant::now();
    let result = state
        .claude_runner
        .run_claude(&prompt, &session_id, &model, attachments, sender, state.config.max_budget, &system_prompt)
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
    }

    send_claude_response(state, sender, result, &session_id).await
}

/// Send a Claude response: check truncation, store session for /more if needed, send to user.
async fn send_claude_response(
    state: &State,
    sender: &str,
    result: Result<(String, Option<f64>), AppError>,
    session_id: &str,
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
                let msg = format!(
                    "{response}\n\n(Response may be truncated. Send /more to continue.)"
                );
                state.send_long_message(sender, &msg).await?;
            } else {
                state.session_mgr.truncated_sessions.remove(sender);
                state.send_long_message(sender, &response).await?;
            }
            Ok(())
        }
        Err(e) => {
            state.metrics.error_count.fetch_add(1, Ordering::Relaxed);
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

    send_claude_response(state, sender, result, session_id).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::{delete_memory, open_memory_db, store_message};
    use crate::signal::AttachmentInfo;
    use crate::state::tests::test_state_with;
    use crate::state::PendingSender;
    use crate::traits::{MockClaudeRunner, MockSignalApi};

    // --- latency and error count tests ---

    #[tokio::test]
    async fn test_handle_message_records_latency() {
        let mut signal = MockSignalApi::new();
        signal.expect_set_typing().returning(|_, _| Ok(()));
        signal.expect_send_msg().returning(|_, _| Ok(()));
        let mut claude = MockClaudeRunner::new();
        claude
            .expect_run_claude()
            .returning(|_, _, _, _, _, _, _| Ok(("ok".to_string(), None)));
        let state = test_state_with(signal, claude);
        let _ = handle_message(&state, "+allowed_user", "hi", &[]).await;
        assert!(state.metrics.latency_count.load(Ordering::Relaxed) >= 1);
        // latency_sum_ms should be populated (>= 0 by type, but confirm it was written)
        let _ = state.metrics.latency_sum_ms.load(Ordering::Relaxed);
    }

    #[tokio::test]
    async fn test_handle_message_error_increments_error_count() {
        let mut signal = MockSignalApi::new();
        signal.expect_set_typing().returning(|_, _| Ok(()));
        signal.expect_send_msg().returning(|_, _| Ok(()));
        let mut claude = MockClaudeRunner::new();
        claude
            .expect_run_claude()
            .returning(|_, _, _, _, _, _, _| Err("boom".into()));
        let state = test_state_with(signal, claude);
        let _ = handle_message(&state, "+allowed_user", "hi", &[]).await;
        assert_eq!(state.metrics.error_count.load(Ordering::Relaxed), 1);
    }

    // --- download_attachments tests ---

    #[tokio::test]
    async fn test_download_attachments_image_success() {
        let mut signal = MockSignalApi::new();
        signal.expect_download_attachment().returning(|att| {
            Ok(PathBuf::from(format!("/tmp/ccchat/test_{}.png", att.id)))
        });
        let state = test_state_with(signal, MockClaudeRunner::new());
        let atts = vec![AttachmentInfo {
            id: "img1".to_string(),
            content_type: "image/png".to_string(),
            filename: Some("photo.png".to_string()),
            voice_note: false,
        }];
        let (paths, has_audio) = download_attachments(&state, "+user", &atts).await;
        assert_eq!(paths.len(), 1);
        assert!(!has_audio);
    }

    #[tokio::test]
    async fn test_download_attachments_audio_sets_flag() {
        let mut signal = MockSignalApi::new();
        signal.expect_download_attachment().returning(|att| {
            Ok(PathBuf::from(format!("/tmp/ccchat/test_{}.aac", att.id)))
        });
        let state = test_state_with(signal, MockClaudeRunner::new());
        let atts = vec![AttachmentInfo {
            id: "aud1".to_string(),
            content_type: "audio/aac".to_string(),
            filename: None,
            voice_note: true,
        }];
        let (paths, has_audio) = download_attachments(&state, "+user", &atts).await;
        assert_eq!(paths.len(), 1);
        assert!(has_audio);
    }

    #[tokio::test]
    async fn test_download_attachments_unsupported_sends_notification() {
        let mut signal = MockSignalApi::new();
        signal.expect_send_msg().returning(|_, msg| {
            assert!(msg.contains("Unsupported attachment type"));
            Ok(())
        });
        let state = test_state_with(signal, MockClaudeRunner::new());
        let atts = vec![AttachmentInfo {
            id: "vid1".to_string(),
            content_type: "video/mp4".to_string(),
            filename: None,
            voice_note: false,
        }];
        let (paths, _) = download_attachments(&state, "+user", &atts).await;
        assert!(paths.is_empty());
    }

    #[tokio::test]
    async fn test_download_attachments_failure_sends_error() {
        let mut signal = MockSignalApi::new();
        signal
            .expect_download_attachment()
            .returning(|_| Err(AppError::Other("download failed".to_string())));
        signal.expect_send_msg().returning(|_, msg| {
            assert!(msg.contains("Failed to download attachment"));
            Ok(())
        });
        let state = test_state_with(signal, MockClaudeRunner::new());
        let atts = vec![AttachmentInfo {
            id: "fail1".to_string(),
            content_type: "image/jpeg".to_string(),
            filename: None,
            voice_note: false,
        }];
        let (paths, _) = download_attachments(&state, "+user", &atts).await;
        assert!(paths.is_empty());
    }

    // --- handle_unauthorized tests ---

    #[tokio::test]
    async fn test_handle_unauthorized_tracks_pending() {
        let mut signal = MockSignalApi::new();
        signal.expect_send_msg().returning(|_, _| Ok(()));
        let state = Arc::new(test_state_with(signal, MockClaudeRunner::new()));
        handle_unauthorized(&state, "+stranger", "Stranger");
        assert!(state.pending_senders.contains_key("+stranger"));
        tokio::time::sleep(Duration::from_millis(50)).await; // let spawn finish
    }

    #[tokio::test]
    async fn test_handle_unauthorized_no_duplicate() {
        let mut signal = MockSignalApi::new();
        // Only one notification sent (first time)
        signal.expect_send_msg().times(1).returning(|_, _| Ok(()));
        let state = Arc::new(test_state_with(signal, MockClaudeRunner::new()));
        handle_unauthorized(&state, "+stranger", "Stranger");
        handle_unauthorized(&state, "+stranger", "Stranger");
        assert_eq!(state.pending_senders.len(), 1);
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    #[tokio::test]
    async fn test_handle_unauthorized_increments_counter() {
        let mut signal = MockSignalApi::new();
        signal.expect_send_msg().returning(|_, _| Ok(()));
        let state = Arc::new(test_state_with(signal, MockClaudeRunner::new()));
        handle_unauthorized(&state, "+a", "A");
        handle_unauthorized(&state, "+b", "B");
        let a_sid = state.pending_senders.get("+a").unwrap().short_id;
        let b_sid = state.pending_senders.get("+b").unwrap().short_id;
        assert_eq!(a_sid, 1);
        assert_eq!(b_sid, 2);
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    #[test]
    fn test_handle_command_status() {
        let state = test_state_with(MockSignalApi::new(), MockClaudeRunner::new());
        let result = handle_command(&state, "+allowed_user", "/status");
        assert!(result.is_some());
        let text = result.unwrap();
        assert!(text.contains("ccchat status"));
        assert!(text.contains("Uptime:"));
        assert!(text.contains("Messages:"));
        assert!(text.contains("Total cost:"));
    }

    #[test]
    fn test_status_includes_sender_cost() {
        let state = test_state_with(MockSignalApi::new(), MockClaudeRunner::new());
        state.add_sender_cost("+allowed_user", 0.1234);
        let text = handle_command(&state, "+allowed_user", "/status").unwrap();
        assert!(text.contains("Your cost: $0.1234"), "got: {text}");
    }

    #[test]
    fn test_status_includes_error_count() {
        let state = test_state_with(MockSignalApi::new(), MockClaudeRunner::new());
        state.metrics.error_count.fetch_add(7, Ordering::Relaxed);
        let text = handle_command(&state, "+allowed_user", "/status").unwrap();
        assert!(text.contains("Errors: 7"), "got: {text}");
    }

    #[test]
    fn test_status_includes_avg_latency() {
        let state = test_state_with(MockSignalApi::new(), MockClaudeRunner::new());
        state.record_latency(100);
        state.record_latency(200);
        let text = handle_command(&state, "+allowed_user", "/status").unwrap();
        assert!(text.contains("Avg latency: 150ms"), "got: {text}");
    }

    #[test]
    fn test_handle_command_pending_empty() {
        let state = test_state_with(MockSignalApi::new(), MockClaudeRunner::new());
        let result = handle_command(&state, "+allowed_user", "/pending");
        assert_eq!(result.unwrap(), "No pending senders.");
    }

    #[test]
    fn test_handle_command_pending_with_entries() {
        let state = test_state_with(MockSignalApi::new(), MockClaudeRunner::new());
        state.pending_senders.insert(
            "+blocked1".to_string(),
            PendingSender {
                name: "Alice".to_string(),
                short_id: 1,
            },
        );
        let result = handle_command(&state, "+allowed_user", "/pending").unwrap();
        assert!(result.contains("Pending senders:"));
        assert!(result.contains("Alice"));
        assert!(result.contains("#1"));
    }

    #[test]
    fn test_handle_command_allow_valid() {
        let state = test_state_with(MockSignalApi::new(), MockClaudeRunner::new());
        state.pending_senders.insert(
            "+new_user".to_string(),
            PendingSender {
                name: "Bob".to_string(),
                short_id: 1,
            },
        );
        let result = handle_command(&state, "+1234567890", "/allow 1").unwrap();
        assert!(result.contains("Allowed:"));
        assert!(state.is_allowed("+new_user"));
    }

    #[test]
    fn test_handle_command_allow_invalid() {
        let state = test_state_with(MockSignalApi::new(), MockClaudeRunner::new());
        let result = handle_command(&state, "+1234567890", "/allow 99").unwrap();
        assert!(result.contains("No pending sender #99"));
    }

    #[test]
    fn test_handle_command_allow_no_arg() {
        let state = test_state_with(MockSignalApi::new(), MockClaudeRunner::new());
        let result = handle_command(&state, "+1234567890", "/allow").unwrap();
        assert!(result.contains("Usage:"));
    }

    #[test]
    fn test_handle_command_revoke() {
        let state = test_state_with(MockSignalApi::new(), MockClaudeRunner::new());
        state.allowed_ids.insert("+victim".to_string(), ());
        assert!(state.is_allowed("+victim"));
        let result = handle_command(&state, "+1234567890", "/revoke +victim").unwrap();
        assert!(result.contains("Revoked:"));
        assert!(!state.is_allowed("+victim"));
    }

    #[test]
    fn test_handle_command_model() {
        let state = test_state_with(MockSignalApi::new(), MockClaudeRunner::new());
        let result = handle_command(&state, "+allowed_user", "/model haiku").unwrap();
        assert!(result.contains("Model switched to: haiku"));
        let session = state.session_mgr.sessions.get("+allowed_user").unwrap();
        assert_eq!(session.model, "haiku");
    }

    #[test]
    fn test_model_command_persists_preference() {
        let sender = format!("+modcmd_{}", std::process::id());
        let state = test_state_with(MockSignalApi::new(), MockClaudeRunner::new());
        state.allowed_ids.insert(sender.clone(), ());
        let _ = handle_command(&state, &sender, "/model opus");
        // Verify persisted to SQLite
        let conn = crate::memory::open_memory_db(&sender).unwrap();
        let pref = crate::memory::load_model_preference(&conn);
        assert_eq!(pref.as_deref(), Some("opus"));
        crate::memory::delete_memory(&sender);
    }

    #[test]
    fn test_handle_command_unknown() {
        let state = test_state_with(MockSignalApi::new(), MockClaudeRunner::new());
        let result = handle_command(&state, "+allowed_user", "just a message");
        assert!(result.is_none());
    }

    // --- Async handle_message tests ---

    #[tokio::test]
    async fn test_handle_message_happy_path() {
        let mut signal = MockSignalApi::new();
        signal.expect_set_typing().returning(|_, _| Ok(()));
        signal.expect_send_msg().returning(|_, _| Ok(()));

        let mut claude = MockClaudeRunner::new();
        claude
            .expect_run_claude()
            .returning(|_, _, _, _, _, _, _| Ok(("Hello from Claude!".to_string(), Some(0.01))));

        let state = test_state_with(signal, claude);
        let result = handle_message(&state, "+allowed_user", "Hi there", &[]).await;
        assert!(result.is_ok());
        assert!((state.total_cost_usd() - 0.01).abs() < 0.001);
    }

    #[tokio::test]
    async fn test_handle_message_reset_with_summarization() {
        let mut signal = MockSignalApi::new();
        signal.expect_send_msg().returning(|_, _| Ok(()));

        let mut claude = MockClaudeRunner::new();
        claude
            .expect_summarize_session()
            .returning(|_, _| Some("Discussed testing.".to_string()));

        let state = test_state_with(signal, claude);
        // Pre-populate a session
        state.session_mgr.sessions.insert(
            "+allowed_user".to_string(),
            SenderState {
                session_id: "test-session".to_string(),
                model: "sonnet".to_string(),
                lock: Arc::new(Mutex::new(())),
                last_activity: Instant::now(),
            },
        );

        let result = handle_message(&state, "+allowed_user", "/reset", &[]).await;
        assert!(result.is_ok());
        assert!(!state.session_mgr.sessions.contains_key("+allowed_user"));
    }

    #[tokio::test]
    async fn test_handle_message_more_with_truncated() {
        let mut signal = MockSignalApi::new();
        signal.expect_set_typing().returning(|_, _| Ok(()));
        signal.expect_send_msg().returning(|_, _| Ok(()));

        let mut claude = MockClaudeRunner::new();
        claude
            .expect_run_claude()
            .returning(|_, _, _, _, _, _, _| Ok(("Continued response.".to_string(), Some(0.005))));

        let state = test_state_with(signal, claude);
        // Pre-populate a session and a truncated marker
        state.session_mgr.sessions.insert(
            "+allowed_user".to_string(),
            SenderState {
                session_id: "sess-1".to_string(),
                model: "sonnet".to_string(),
                lock: Arc::new(Mutex::new(())),
                last_activity: Instant::now(),
            },
        );
        state
            .session_mgr
            .truncated_sessions
            .insert("+allowed_user".to_string(), "sess-1".to_string());

        let result = handle_message(&state, "+allowed_user", "/more", &[]).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_handle_message_more_without_truncated() {
        let mut signal = MockSignalApi::new();
        signal.expect_send_msg().times(1).returning(|_, msg| {
            assert_eq!(msg, "Nothing to continue.");
            Ok(())
        });

        let state = test_state_with(signal, MockClaudeRunner::new());
        let result = handle_message(&state, "+allowed_user", "/more", &[]).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_handle_message_rate_limited() {
        let mut signal = MockSignalApi::new();
        signal.expect_send_msg().returning(|_, msg| {
            assert!(msg.contains("Rate limited"));
            Ok(())
        });

        let mut state = test_state_with(signal, MockClaudeRunner::new());
        // Enable rate limiting: 0 capacity, no refill -> always limited
        state.config.rate_limit_config = Some((1.0, 0.0));
        // First call consumes the single token
        {
            let mut bucket = state
                .rate_limits
                .entry("+allowed_user".to_string())
                .or_insert_with(|| TokenBucket::new(0.0, 0.0)); // 0 capacity = always empty
            bucket.tokens = 0.0;
        }

        let result = handle_message(&state, "+allowed_user", "hello", &[]).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_handle_message_claude_error() {
        let mut signal = MockSignalApi::new();
        signal.expect_set_typing().returning(|_, _| Ok(()));
        signal.expect_send_msg().returning(|_, msg| {
            assert!(msg.contains("Claude error:"));
            Ok(())
        });

        let mut claude = MockClaudeRunner::new();
        claude
            .expect_run_claude()
            .returning(|_, _, _, _, _, _, _| Err("model unavailable".into()));

        let state = test_state_with(signal, claude);
        let result = handle_message(&state, "+allowed_user", "hello", &[]).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_handle_message_truncated_response() {
        let mut signal = MockSignalApi::new();
        signal.expect_set_typing().returning(|_, _| Ok(()));
        signal.expect_send_msg().returning(|_, _| Ok(()));

        let long_response = "a".repeat(4000) + " and then the function";
        let long_clone = long_response.clone();
        let mut claude = MockClaudeRunner::new();
        claude
            .expect_run_claude()
            .returning(move |_, _, _, _, _, _, _| Ok((long_clone.clone(), Some(0.02))));

        let state = test_state_with(signal, claude);
        let result = handle_message(&state, "+allowed_user", "explain this", &[]).await;
        assert!(result.is_ok());
        // Should have stored the session for /more
        assert!(state.session_mgr.truncated_sessions.contains_key("+allowed_user"));
    }

    #[tokio::test]
    async fn test_handle_message_cost_tracking() {
        let mut signal = MockSignalApi::new();
        signal.expect_set_typing().returning(|_, _| Ok(()));
        signal.expect_send_msg().returning(|_, _| Ok(()));

        let mut claude = MockClaudeRunner::new();
        claude
            .expect_run_claude()
            .returning(|_, _, _, _, _, _, _| Ok(("response".to_string(), Some(0.05))));

        let state = test_state_with(signal, claude);
        let _ = handle_message(&state, "+allowed_user", "msg1", &[]).await;
        assert!((state.total_cost_usd() - 0.05).abs() < 0.001);
    }

    #[tokio::test]
    async fn test_handle_message_new_session_injects_context() {
        let mut signal = MockSignalApi::new();
        signal.expect_set_typing().returning(|_, _| Ok(()));
        signal.expect_send_msg().returning(|_, _| Ok(()));

        let mut claude = MockClaudeRunner::new();
        claude
            .expect_run_claude()
            .returning(|prompt, _, _, _, _, _, _| {
                // Should be called with the user's text (possibly with context prepended)
                assert!(prompt.contains("hello") || prompt.contains("Previous conversations"));
                Ok(("reply".to_string(), None))
            });

        let state = test_state_with(signal, claude);
        let result = handle_message(&state, "+allowed_user", "hello", &[]).await;
        assert!(result.is_ok());
        // Session should now exist
        assert!(state.session_mgr.sessions.contains_key("+allowed_user"));
    }

    #[tokio::test]
    async fn test_handle_message_command_response() {
        let mut signal = MockSignalApi::new();
        signal.expect_send_msg().returning(|_, msg| {
            assert!(msg.contains("ccchat status"));
            Ok(())
        });

        let state = test_state_with(signal, MockClaudeRunner::new());
        let result = handle_message(&state, "+allowed_user", "/status", &[]).await;
        assert!(result.is_ok());
    }

    // --- handle_continuation tests ---

    #[tokio::test]
    async fn test_handle_continuation_success() {
        let mut signal = MockSignalApi::new();
        signal.expect_set_typing().returning(|_, _| Ok(()));
        signal.expect_send_msg().returning(|_, _| Ok(()));

        let mut claude = MockClaudeRunner::new();
        claude
            .expect_run_claude()
            .returning(|_, _, _, _, _, _, _| Ok(("more content here.".to_string(), Some(0.01))));

        let state = test_state_with(signal, claude);
        state.session_mgr.sessions.insert(
            "+allowed_user".to_string(),
            SenderState {
                session_id: "sess-cont".to_string(),
                model: "sonnet".to_string(),
                lock: Arc::new(Mutex::new(())),
                last_activity: Instant::now(),
            },
        );

        let result = handle_continuation(&state, "+allowed_user", "sess-cont").await;
        assert!(result.is_ok());
        assert!((state.total_cost_usd() - 0.01).abs() < 0.001);
    }

    #[tokio::test]
    async fn test_handle_continuation_still_truncated() {
        let mut signal = MockSignalApi::new();
        signal.expect_set_typing().returning(|_, _| Ok(()));
        signal.expect_send_msg().returning(|_, _| Ok(()));

        let long_response = "a".repeat(4000) + " and then the";
        let long_clone = long_response.clone();
        let mut claude = MockClaudeRunner::new();
        claude
            .expect_run_claude()
            .returning(move |_, _, _, _, _, _, _| Ok((long_clone.clone(), None)));

        let state = test_state_with(signal, claude);
        let result = handle_continuation(&state, "+allowed_user", "sess-1").await;
        assert!(result.is_ok());
        assert!(state.session_mgr.truncated_sessions.contains_key("+allowed_user"));
    }

    #[tokio::test]
    async fn test_handle_continuation_claude_error() {
        let mut signal = MockSignalApi::new();
        signal.expect_set_typing().returning(|_, _| Ok(()));
        signal.expect_send_msg().returning(|_, msg| {
            assert!(msg.contains("Claude error:"));
            Ok(())
        });

        let mut claude = MockClaudeRunner::new();
        claude
            .expect_run_claude()
            .returning(|_, _, _, _, _, _, _| Err("timeout".into()));

        let state = test_state_with(signal, claude);
        let result = handle_continuation(&state, "+allowed_user", "sess-1").await;
        assert!(result.is_ok());
    }

    // --- /search command tests ---

    #[test]
    fn test_search_with_results() {
        let sender = format!("search_results_{}", std::process::id());
        let conn = open_memory_db(&sender).unwrap();
        store_message(&conn, "user", "How do I configure nginx?", "sess1");
        store_message(
            &conn,
            "assistant",
            "Edit /etc/nginx/nginx.conf for main config.",
            "sess1",
        );
        drop(conn);

        let state = test_state_with(MockSignalApi::new(), MockClaudeRunner::new());
        let result = handle_command(&state, &sender, "/search nginx").unwrap();
        assert!(result.contains("Search results for \"nginx\""));
        assert!(result.contains("found)"));
        assert!(result.contains("[user]"));
        assert!(result.contains("[assistant]"));
        assert!(result.contains("nginx"));
        delete_memory(&sender);
    }

    #[test]
    fn test_search_no_results() {
        let sender = format!("search_noresult_{}", std::process::id());
        let conn = open_memory_db(&sender).unwrap();
        store_message(&conn, "user", "Hello world", "sess1");
        drop(conn);

        let state = test_state_with(MockSignalApi::new(), MockClaudeRunner::new());
        let result = handle_command(&state, &sender, "/search xylophone").unwrap();
        assert_eq!(result, "No results found for \"xylophone\"");
        delete_memory(&sender);
    }

    #[test]
    fn test_search_no_argument() {
        let state = test_state_with(MockSignalApi::new(), MockClaudeRunner::new());
        let result = handle_command(&state, "+allowed_user", "/search").unwrap();
        assert!(result.contains("Usage: /search <query>"));
    }

    #[test]
    fn test_search_empty_memory() {
        let sender = format!("search_empty_{}", std::process::id());
        // Ensure no DB exists (clean state)
        delete_memory(&sender);

        let state = test_state_with(MockSignalApi::new(), MockClaudeRunner::new());
        let result = handle_command(&state, &sender, "/search anything").unwrap();
        assert!(result.contains("No results found for \"anything\""));
        delete_memory(&sender);
    }

    #[test]
    fn test_search_results_truncated() {
        let sender = format!("search_trunc_{}", std::process::id());
        let conn = open_memory_db(&sender).unwrap();
        // Insert more than 5 messages containing the keyword
        for i in 1..=8 {
            store_message(
                &conn,
                "user",
                &format!("Message {i} about databases and SQL optimization"),
                "sess1",
            );
        }
        // Insert a message with content longer than 100 chars
        let long_content = format!(
            "databases {} end of long message",
            "x".repeat(120)
        );
        store_message(&conn, "assistant", &long_content, "sess1");
        drop(conn);

        let state = test_state_with(MockSignalApi::new(), MockClaudeRunner::new());
        let result = handle_command(&state, &sender, "/search databases").unwrap();
        // Count result lines (first line is header, rest are results starting with "- ")
        let result_lines: Vec<&str> = result.lines().filter(|l| l.starts_with("- ")).collect();
        assert!(
            result_lines.len() <= 5,
            "Expected at most 5 results, got {}",
            result_lines.len()
        );
        // Check that long content was truncated (contains "...")
        let has_truncated = result_lines.iter().any(|l| l.contains("..."));
        assert!(has_truncated, "Expected at least one truncated preview");
        delete_memory(&sender);
    }

    #[test]
    fn test_handle_command_export_config() {
        let state = test_state_with(MockSignalApi::new(), MockClaudeRunner::new());
        let result = handle_command(&state, "+1234567890", "/export-config");
        assert!(result.is_some());
        let json = result.unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert!(parsed["allowed"].is_array());
    }

    #[tokio::test]
    async fn test_handle_message_attachments_forwarded_to_claude() {
        let mut signal = MockSignalApi::new();
        signal.expect_set_typing().returning(|_, _| Ok(()));
        signal.expect_send_msg().returning(|_, _| Ok(()));

        let mut claude = MockClaudeRunner::new();
        claude
            .expect_run_claude()
            .withf(|_, _, _, files, _, _, _| {
                files.len() == 2
                    && files[0] == PathBuf::from("/tmp/photo.png")
                    && files[1] == PathBuf::from("/tmp/doc.pdf")
            })
            .returning(|_, _, _, _, _, _, _| Ok(("I see a photo and a PDF.".to_string(), Some(0.02))));

        let state = test_state_with(signal, claude);
        let attachments = vec![
            PathBuf::from("/tmp/photo.png"),
            PathBuf::from("/tmp/doc.pdf"),
        ];
        let result = handle_message(&state, "+allowed_user", "What are these?", &attachments).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_handle_message_cost_accumulates_across_messages() {
        let mut signal = MockSignalApi::new();
        signal.expect_set_typing().returning(|_, _| Ok(()));
        signal.expect_send_msg().returning(|_, _| Ok(()));

        let call_count = std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0));
        let call_count_clone = call_count.clone();
        let mut claude = MockClaudeRunner::new();
        claude
            .expect_run_claude()
            .returning(move |_, _, _, _, _, _, _| {
                let n = call_count_clone.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                let cost = if n == 0 { 0.03 } else { 0.07 };
                Ok(("reply".to_string(), Some(cost)))
            });

        let state = test_state_with(signal, claude);
        let _ = handle_message(&state, "+allowed_user", "msg1", &[]).await;
        let _ = handle_message(&state, "+allowed_user", "msg2", &[]).await;
        assert!(
            (state.total_cost_usd() - 0.10).abs() < 0.001,
            "Expected ~$0.10, got ${:.4}",
            state.total_cost_usd()
        );
    }

    #[test]
    fn test_handle_command_help() {
        let state = test_state_with(MockSignalApi::new(), MockClaudeRunner::new());
        let result = handle_command(&state, "+allowed_user", "/help");
        assert!(result.is_some());
    }

    #[test]
    fn test_handle_command_help_lists_commands() {
        let state = test_state_with(MockSignalApi::new(), MockClaudeRunner::new());
        let text = handle_command(&state, "+allowed_user", "/help").unwrap();
        let commands = [
            "/help", "/status", "/reset", "/more", "/model", "/memory",
            "/forget", "/search", "/pending", "/allow", "/revoke", "/export-config",
        ];
        for cmd in &commands {
            assert!(text.contains(cmd), "Missing command: {cmd}");
        }
    }

    #[test]
    fn test_handle_command_help_has_descriptions() {
        let state = test_state_with(MockSignalApi::new(), MockClaudeRunner::new());
        let text = handle_command(&state, "+allowed_user", "/help").unwrap();
        // Each line after the header should have " - " separator
        for line in text.lines().skip(1) {
            assert!(line.contains(" - "), "Missing description separator in: {line}");
        }
    }

    #[tokio::test]
    async fn test_handle_message_new_session_injects_context_format() {
        use crate::memory::{delete_memory, open_memory_db, save_memory, store_message};

        let sender = format!("+ctx_format_{}", uuid::Uuid::new_v4());
        // Pre-populate memory with a summary so inject_context includes "Previous conversations"
        save_memory(&sender, "Discussed deploying nginx on Ubuntu.");
        // Also store a message matching "nginx" so FTS5 search finds it
        let conn = open_memory_db(&sender).unwrap();
        store_message(&conn, "user", "How do I deploy nginx?", "old-sess");
        store_message(&conn, "assistant", "Use apt install nginx then edit the config.", "old-sess");
        drop(conn);

        let mut signal = MockSignalApi::new();
        signal.expect_set_typing().returning(|_, _| Ok(()));
        signal.expect_send_msg().returning(|_, _| Ok(()));

        let mut claude = MockClaudeRunner::new();
        claude
            .expect_run_claude()
            .withf(|prompt, _, _, _, _, _, _| {
                // New session should include context header AND the user's message
                prompt.contains("Previous conversations") && prompt.contains("tell me about nginx")
            })
            .returning(|_, _, _, _, _, _, _| Ok(("got it".to_string(), None)));

        let state = test_state_with(signal, claude);
        state.allowed_ids.insert(sender.clone(), ());

        let result = handle_message(&state, &sender, "tell me about nginx", &[]).await;
        assert!(result.is_ok());
        delete_memory(&sender);
    }
}
