use crate::state::State;

pub(super) fn cmd_remind(sender: &str, arg: &str) -> String {
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

pub(super) fn cmd_reminders(sender: &str) -> String {
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

pub(super) fn cmd_cancel_reminder(sender: &str, id_str: &str) -> String {
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

pub(super) fn cmd_cron(sender: &str, arg: &str) -> String {
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

pub(super) fn cmd_every(sender: &str, arg: &str) -> String {
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

pub(super) fn cmd_daily(sender: &str, arg: &str) -> String {
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

pub(super) fn cmd_crons(sender: &str) -> String {
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

pub(super) fn cmd_cron_cancel(sender: &str, id_str: &str) -> String {
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

pub(super) fn cmd_cron_pause(sender: &str, id_str: &str) -> String {
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

pub(super) fn cmd_cron_resume(sender: &str, id_str: &str) -> String {
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

/// Deliver due reminders. Called periodically by background loop.
pub(crate) async fn deliver_due_reminders(state: &State) {
    let Ok(conn) = crate::schedule::open_schedule_db() else {
        return;
    };
    for (id, sender, message) in crate::schedule::get_due_reminders(&conn) {
        let text = format!("Reminder: {message}");
        if let Err(e) = state.send_message(&sender, &text).await {
            tracing::warn!(sender = %sender, "Failed to deliver reminder: {e}");
        } else {
            crate::schedule::mark_delivered(&conn, id);
        }
    }
    crate::schedule::purge_delivered(&conn);
}

/// Deliver due cron jobs. Called periodically by background loop.
pub(crate) async fn deliver_due_cron_jobs(state: &State) {
    let Ok(conn) = crate::schedule::open_schedule_db() else {
        return;
    };
    for (id, sender, message, cron_pattern, interval_secs) in
        crate::schedule::get_due_cron_jobs(&conn)
    {
        let text = format!("Scheduled: {message}");
        if let Err(e) = state.send_message(&sender, &text).await {
            tracing::warn!(sender = %sender, "Failed to deliver cron job: {e}");
        } else {
            crate::schedule::advance_cron_job(&conn, id, cron_pattern.as_deref(), interval_secs);
        }
    }
}
