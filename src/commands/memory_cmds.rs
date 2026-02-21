use std::sync::Arc;
use std::time::Instant;
use tokio::sync::Mutex;

use crate::memory::{format_epoch, search_memory_formatted};
use crate::state::{SenderState, State};

pub(super) fn cmd_search(sender: &str, query: &str) -> String {
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

pub(super) fn cmd_model(state: &State, sender: &str, model: &str) -> String {
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

pub(super) fn cmd_pin(sender: &str, label: &str) -> String {
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

pub(super) fn cmd_pins(sender: &str) -> String {
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

pub(super) fn cmd_recall(state: &State, sender: &str, label: &str) -> String {
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
