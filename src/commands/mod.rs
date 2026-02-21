use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::Mutex;
use tracing::{error, info, warn};

use crate::error::AppError;
use crate::helpers::{looks_truncated, merge_messages};
use crate::memory::{
    export_config, export_messages, forget_with_counts, format_epoch, inject_context,
    memory_status, persist_allow, persist_revoke, save_memory, search_memory_formatted,
    store_message_pair,
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
            None => return format!("No pending sender #{num}.\nUse /pending to see the list."),
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
    crate::audit::log_action("allow", &real_id, &name);
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
    crate::audit::log_action("revoke", id, "");
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
            message_count: 0,
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
                let _ = state.send_message(&reply_to, &format!("Error: {e}")).await;
            }
        });
    }
}

fn cmd_usage(state: &State, sender: &str) -> String {
    let cost = state.sender_cost_usd(sender);
    let (_, model, _, _) = state.get_or_create_session(sender);
    let (sent, received, first_date) = match crate::memory::open_memory_db(sender) {
        Ok(conn) => {
            let (u, a) = crate::memory::get_message_count_by_role(&conn);
            let first = crate::memory::messages::get_oldest_message_ts(&conn)
                .map(|ts| format_epoch(ts as u64))
                .unwrap_or_else(|| "N/A".to_string());
            (u, a, first)
        }
        Err(_) => (0, 0, "N/A".to_string()),
    };
    format!(
        "Your usage:\n\
         Cost: ${cost:.4}\n\
         Messages: {sent} sent, {received} received\n\
         First message: {first_date}\n\
         Current model: {model}"
    )
}

fn cmd_audit() -> String {
    let actions = crate::audit::get_recent_actions(20);
    if actions.is_empty() {
        return "No audit log entries.".to_string();
    }
    let mut lines = vec![format!("Audit log ({} entries):", actions.len())];
    for (action, target, detail, ts) in &actions {
        let date = format_epoch(*ts as u64);
        let target_str = if target.is_empty() {
            String::new()
        } else {
            format!(" {target}")
        };
        let detail_str = if detail.is_empty() {
            String::new()
        } else {
            format!(" ({detail})")
        };
        lines.push(format!("[{date}] {action}{target_str}{detail_str}"));
    }
    lines.join("\n")
}

fn cmd_pin(sender: &str, label: &str) -> String {
    if label.is_empty() {
        return "Usage: /pin <label>".to_string();
    }
    let Ok(conn) = crate::memory::open_memory_db(sender) else {
        return "Failed to access memory.".to_string();
    };
    let messages =
        crate::memory::messages::get_recent_messages(&conn, crate::constants::PIN_MESSAGE_COUNT);
    if messages.is_empty() {
        return "No recent messages to pin.".to_string();
    }
    let content: String = messages
        .iter()
        .map(|(role, content, _)| format!("{role}: {content}"))
        .collect::<Vec<_>>()
        .join("\n");
    crate::memory::messages::save_pin(&conn, label, &content);
    format!("Pinned {} messages as '{label}'", messages.len())
}

fn cmd_pins(sender: &str) -> String {
    let Ok(conn) = crate::memory::open_memory_db(sender) else {
        return "Failed to access memory.".to_string();
    };
    let pins = crate::memory::messages::list_pins(&conn);
    if pins.is_empty() {
        return "No saved pins.".to_string();
    }
    let mut lines = vec![format!("Saved pins ({}):", pins.len())];
    for (label, ts) in &pins {
        let date = format_epoch(*ts as u64);
        lines.push(format!("  {label} ({date})"));
    }
    lines.join("\n")
}

fn cmd_recall(state: &State, sender: &str, label: &str) -> String {
    if label.is_empty() {
        return "Usage: /recall <label>".to_string();
    }
    let Ok(conn) = crate::memory::open_memory_db(sender) else {
        return "Failed to access memory.".to_string();
    };
    match crate::memory::messages::get_pin(&conn, label) {
        Some(content) => {
            state.pending_recalls.insert(sender.to_string(), content);
            format!("Recalled pin '{label}'. It will be included in your next message.")
        }
        None => format!("No pin named '{label}'. Use /pins to see available pins."),
    }
}

fn cmd_remind(sender: &str, arg: &str) -> String {
    let parts: Vec<&str> = arg.splitn(2, ' ').collect();
    if parts.len() < 2 || parts[0].is_empty() {
        return "Usage: /remind <time> <message>\nExamples: /remind 5m Check the oven\n          /remind 1h Call dentist".to_string();
    }
    let duration = match crate::helpers::parse_duration(parts[0]) {
        Some(d) => d,
        None => return format!("Invalid time format: '{}'. Use 5m, 1h, 30s, 2d.", parts[0]),
    };
    let message = parts[1];
    let deliver_at = crate::helpers::epoch_now() + duration.as_secs() as i64;
    let Ok(conn) = crate::schedule::open_schedule_db() else {
        return "Failed to access schedule database.".to_string();
    };
    let id = crate::schedule::add_reminder(&conn, sender, message, deliver_at);
    let human = crate::helpers::format_duration_human(duration.as_secs());
    format!("Reminder #{id} set for {human} from now: {message}")
}

fn cmd_reminders(sender: &str) -> String {
    let Ok(conn) = crate::schedule::open_schedule_db() else {
        return "Failed to access schedule database.".to_string();
    };
    let reminders = crate::schedule::get_pending_reminders(&conn, sender);
    if reminders.is_empty() {
        return "No pending reminders.".to_string();
    }
    let now = crate::helpers::epoch_now();
    let mut lines = vec![format!("Pending reminders ({}):", reminders.len())];
    for (id, message, deliver_at) in &reminders {
        let remaining = (*deliver_at - now).max(0) as u64;
        let human = crate::helpers::format_duration_human(remaining);
        lines.push(format!("  #{id} - {message} (in {human})"));
    }
    lines.join("\n")
}

fn cmd_cancel_reminder(sender: &str, id_str: &str) -> String {
    let id: i64 = match id_str.parse() {
        Ok(id) => id,
        Err(_) => return "Usage: /cancel <id>".to_string(),
    };
    let Ok(conn) = crate::schedule::open_schedule_db() else {
        return "Failed to access schedule database.".to_string();
    };
    if crate::schedule::cancel_reminder(&conn, id, sender) {
        format!("Reminder #{id} cancelled.")
    } else {
        format!("No pending reminder #{id} found for you.")
    }
}

fn cmd_cron(sender: &str, arg: &str) -> String {
    if arg.is_empty() {
        return "Usage: /cron \"0 9 * * MON\" <message>\n       /cron 0 9 * * MON <message>\nAll times are UTC.".to_string();
    }
    // Parse quoted or unquoted cron pattern
    let (pattern, message) = if let Some(after_open) = arg.strip_prefix('"') {
        // Quoted: /cron "0 9 * * MON" message
        match after_open.find('"') {
            Some(end) => {
                let p = &after_open[..end];
                let msg = after_open[end + 1..].trim();
                (p.to_string(), msg.to_string())
            }
            None => {
                return "Missing closing quote. Usage: /cron \"0 9 * * *\" <message>".to_string()
            }
        }
    } else {
        // Unquoted: assume exactly 5 fields then message
        let parts: Vec<&str> = arg.splitn(6, ' ').collect();
        if parts.len() < 6 {
            return "Usage: /cron <min> <hour> <day> <month> <dow> <message>\n       /cron \"0 9 * * MON\" <message>".to_string();
        }
        let p = parts[..5].join(" ");
        (p, parts[5].to_string())
    };
    if message.is_empty() {
        return "Missing message. Usage: /cron \"0 9 * * *\" <message>".to_string();
    }
    let Ok(conn) = crate::schedule::open_schedule_db() else {
        return "Failed to access schedule database.".to_string();
    };
    let id = crate::schedule::add_cron_job(&conn, sender, &message, &pattern);
    if id == 0 {
        return format!("Invalid cron pattern: \"{pattern}\"");
    }
    let desc = crate::helpers::format_cron_human(&pattern);
    crate::audit::log_action("cron_create", sender, &format!("#{id} cron {pattern}"));
    format!("Cron job #{id} created: {desc}\nMessage: {message}")
}

fn cmd_every(sender: &str, arg: &str) -> String {
    let parts: Vec<&str> = arg.splitn(2, ' ').collect();
    if parts.len() < 2 || parts[0].is_empty() {
        return "Usage: /every <interval> <message>\nExamples: /every 1h Check status\n          /every 30m Drink water".to_string();
    }
    let secs = match crate::helpers::parse_interval_secs(parts[0]) {
        Some(s) => s,
        None => return format!("Invalid interval: '{}'. Use 30s, 5m, 1h, 2d.", parts[0]),
    };
    let message = parts[1];
    let Ok(conn) = crate::schedule::open_schedule_db() else {
        return "Failed to access schedule database.".to_string();
    };
    let id = crate::schedule::add_interval_job(&conn, sender, message, secs);
    let human = crate::helpers::format_duration_human(secs as u64);
    crate::audit::log_action("cron_create", sender, &format!("#{id} interval {human}"));
    format!("Interval job #{id} created: every {human}\nMessage: {message}")
}

fn cmd_daily(sender: &str, arg: &str) -> String {
    let parts: Vec<&str> = arg.splitn(2, ' ').collect();
    if parts.len() < 2 || parts[0].is_empty() {
        return "Usage: /daily <HH:MM> <message>\nExamples: /daily 09:00 Morning standup\n          /daily 17:30 EOD review\nAll times are UTC.".to_string();
    }
    let pattern = match crate::helpers::parse_daily_time(parts[0]) {
        Some(p) => p,
        None => {
            return format!(
                "Invalid time: '{}'. Use HH:MM format (e.g., 09:00).",
                parts[0]
            )
        }
    };
    let message = parts[1];
    let Ok(conn) = crate::schedule::open_schedule_db() else {
        return "Failed to access schedule database.".to_string();
    };
    let id = crate::schedule::add_cron_job(&conn, sender, message, &pattern);
    if id == 0 {
        return "Failed to create daily job.".to_string();
    }
    crate::audit::log_action("cron_create", sender, &format!("#{id} daily {}", parts[0]));
    format!(
        "Daily job #{id} created: every day at {} UTC\nMessage: {message}",
        parts[0]
    )
}

fn cmd_crons(sender: &str) -> String {
    let Ok(conn) = crate::schedule::open_schedule_db() else {
        return "Failed to access schedule database.".to_string();
    };
    let jobs = crate::schedule::get_active_cron_jobs(&conn, sender);
    if jobs.is_empty() {
        return "No active cron jobs.".to_string();
    }
    let mut lines = vec![format!("Active cron jobs ({}):", jobs.len())];
    for (id, message, job_type, pattern, interval, _next_at) in &jobs {
        let schedule = if *job_type == "cron" {
            pattern
                .as_deref()
                .map(crate::helpers::format_cron_human)
                .unwrap_or_default()
        } else {
            interval
                .map(|s| format!("every {}", crate::helpers::format_duration_human(s as u64)))
                .unwrap_or_default()
        };
        lines.push(format!("  #{id} [{job_type}] {schedule} — {message}"));
    }
    lines.join("\n")
}

fn cmd_cron_cancel(sender: &str, id_str: &str) -> String {
    let id: i64 = match id_str.parse() {
        Ok(id) => id,
        Err(_) => return "Usage: /cron-cancel <id>".to_string(),
    };
    let Ok(conn) = crate::schedule::open_schedule_db() else {
        return "Failed to access schedule database.".to_string();
    };
    if crate::schedule::cancel_cron_job(&conn, id, sender) {
        crate::audit::log_action("cron_cancel", sender, &format!("#{id}"));
        format!("Cron job #{id} cancelled.")
    } else {
        format!("No cron job #{id} found for you.")
    }
}

fn cmd_cron_pause(sender: &str, id_str: &str) -> String {
    let id: i64 = match id_str.parse() {
        Ok(id) => id,
        Err(_) => return "Usage: /cron-pause <id>".to_string(),
    };
    let Ok(conn) = crate::schedule::open_schedule_db() else {
        return "Failed to access schedule database.".to_string();
    };
    if crate::schedule::pause_cron_job(&conn, id, sender) {
        crate::audit::log_action("cron_pause", sender, &format!("#{id}"));
        format!("Cron job #{id} paused.")
    } else {
        format!("No active cron job #{id} found for you.")
    }
}

fn cmd_cron_resume(sender: &str, id_str: &str) -> String {
    let id: i64 = match id_str.parse() {
        Ok(id) => id,
        Err(_) => return "Usage: /cron-resume <id>".to_string(),
    };
    let Ok(conn) = crate::schedule::open_schedule_db() else {
        return "Failed to access schedule database.".to_string();
    };
    if crate::schedule::resume_cron_job(&conn, id, sender) {
        crate::audit::log_action("cron_resume", sender, &format!("#{id}"));
        format!("Cron job #{id} resumed.")
    } else {
        format!("No paused cron job #{id} found for you.")
    }
}

fn cmd_export(sender: &str) -> String {
    let Ok(conn) = crate::memory::open_memory_db(sender) else {
        return "No messages to export.".to_string();
    };
    export_messages(&conn, 100)
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
     /export - Export conversation history\n\
     /usage - Show your personal usage stats\n\
     /pin <label> - Pin recent messages with a label\n\
     /pins - List saved pins\n\
     /recall <label> - Recall a pinned conversation for context\n\
     /remind <time> <msg> - Set a reminder (e.g., /remind 5m Check oven)\n\
     /reminders - List your pending reminders\n\
     /cancel <id> - Cancel a reminder\n\
     /cron <pattern> <msg> - Create a cron job (e.g., /cron \"0 9 * * MON\" Standup)\n\
     /every <interval> <msg> - Repeat every N time (e.g., /every 1h Check status)\n\
     /daily <HH:MM> <msg> - Daily job at time UTC (e.g., /daily 09:00 Standup)\n\
     /crons - List active cron jobs\n\
     /cron-cancel <id> - Cancel a cron job\n\
     /cron-pause <id> - Pause a cron job\n\
     /cron-resume <id> - Resume a paused cron job\n\
     /audit - View recent admin actions\n\
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

pub(crate) async fn deliver_due_reminders(state: &State) {
    let Ok(conn) = crate::schedule::open_schedule_db() else {
        return;
    };
    for (id, sender, message) in crate::schedule::get_due_reminders(&conn) {
        let text = format!("Reminder: {message}");
        if let Err(e) = state.send_message(&sender, &text).await {
            warn!(sender = %sender, "Failed to deliver reminder: {e}");
        } else {
            crate::schedule::mark_delivered(&conn, id);
        }
    }
    crate::schedule::purge_delivered(&conn);
}

pub(crate) async fn deliver_due_cron_jobs(state: &State) {
    let Ok(conn) = crate::schedule::open_schedule_db() else {
        return;
    };
    for (id, sender, message, cron_pattern, interval_secs) in
        crate::schedule::get_due_cron_jobs(&conn)
    {
        let text = format!("Scheduled: {message}");
        if let Err(e) = state.send_message(&sender, &text).await {
            warn!(sender = %sender, "Failed to deliver cron job: {e}");
        } else {
            crate::schedule::advance_cron_job(&conn, id, cron_pattern.as_deref(), interval_secs);
        }
    }
}

#[cfg(test)]
mod tests;
