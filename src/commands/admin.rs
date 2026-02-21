use std::sync::atomic::Ordering;
use std::sync::Arc;
use tracing::info;

use crate::memory::{export_messages, format_epoch, persist_allow, persist_revoke};
use crate::state::{PendingSender, State};

pub(super) fn cmd_status(state: &State, sender: &str) -> String {
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

pub(super) fn cmd_pending(state: &State) -> String {
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

pub(super) fn cmd_allow(state: &State, arg: &str) -> String {
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

pub(super) fn cmd_revoke(state: &State, id: &str) -> String {
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

pub(super) fn cmd_audit() -> String {
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

pub(super) fn cmd_export(sender: &str) -> String {
    let Ok(conn) = crate::memory::open_memory_db(sender) else {
        return "No messages to export.".to_string();
    };
    export_messages(&conn, 100)
}

pub(super) fn cmd_usage(state: &State, sender: &str) -> String {
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

pub(super) fn cmd_help() -> String {
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
