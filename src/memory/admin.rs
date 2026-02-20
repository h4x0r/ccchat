use tracing::error;

use super::context::{delete_memory, format_epoch};
use super::messages::{
    get_message_count, get_oldest_message_ts, get_recent_summaries, get_summary_count,
    search_memory,
};
use super::schema::open_memory_db;

pub(crate) fn memory_status(sender: &str) -> String {
    let conn = match open_memory_db(sender) {
        Ok(c) => c,
        Err(_) => {
            return "No stored memory for this sender.\nMemory will be saved when your session ends."
                .to_string()
        }
    };

    let msg_count = get_message_count(&conn);
    let sum_count = get_summary_count(&conn);

    if msg_count == 0 && sum_count == 0 {
        return "No stored memory for this sender.\nMemory will be saved when your session ends."
            .to_string();
    }

    let mut out = format!(
        "Memory for this sender:\n  Stored messages: {msg_count}\n  Session summaries: {sum_count}"
    );

    if let Some(oldest) = get_oldest_message_ts(&conn) {
        out.push_str(&format!(
            "\n  Oldest record: {}",
            format_epoch(oldest as u64)
        ));
    }

    let summaries = get_recent_summaries(&conn, crate::constants::MAX_SUMMARIES);
    if !summaries.is_empty() {
        out.push_str("\n\nRecent summaries:");
        for (summary, ts) in summaries.iter().rev() {
            let date = format_epoch(*ts as u64);
            out.push_str(&format!("\n  - {date}: {summary}"));
        }
    }

    out.push_str("\n\nUse /forget to clear all memory.");
    out
}

pub(crate) fn search_memory_formatted(
    sender: &str,
    query: &str,
    limit: usize,
) -> Vec<(String, String, i64)> {
    match open_memory_db(sender) {
        Ok(conn) => search_memory(&conn, query, limit),
        Err(e) => {
            error!("Failed to open memory DB for search: {e}");
            Vec::new()
        }
    }
}

pub(crate) fn forget_with_counts(sender: &str) -> String {
    let (msg_count, sum_count) = match open_memory_db(sender) {
        Ok(conn) => (get_message_count(&conn), get_summary_count(&conn)),
        Err(_) => (0, 0),
    };
    delete_memory(sender);
    if msg_count == 0 && sum_count == 0 {
        "No memory to clear. Next conversation starts fresh.".to_string()
    } else {
        format!(
            "Memory cleared:\n  Deleted {msg_count} messages and {sum_count} summaries.\n  Next conversation starts completely fresh."
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::schema::memory_db_path;
    use crate::memory::{open_memory_db, save_memory};

    #[test]
    fn test_memory_command_no_data() {
        let sender = format!("memcmd_empty_{}", std::process::id());
        let result = memory_status(&sender);
        assert!(result.contains("No stored memory"));
        delete_memory(&sender);
    }

    #[test]
    fn test_memory_command_with_data() {
        let sender = format!("memcmd_data_{}", std::process::id());
        save_memory(&sender, "Discussed async Rust patterns.");
        let conn = open_memory_db(&sender).unwrap();
        crate::memory::messages::store_message(&conn, "user", "How does tokio work?", "sess1");
        crate::memory::messages::store_message(
            &conn,
            "assistant",
            "Tokio is an async runtime for Rust.",
            "sess1",
        );
        drop(conn);
        let result = memory_status(&sender);
        assert!(result.contains("Stored messages: 2"));
        assert!(result.contains("Session summaries: 1"));
        assert!(result.contains("Discussed async Rust patterns."));
        assert!(result.contains("/forget"));
        delete_memory(&sender);
    }

    #[test]
    fn test_forget_command_with_counts() {
        let sender = format!("forget_cnt_{}", std::process::id());
        save_memory(&sender, "Some context.");
        let conn = open_memory_db(&sender).unwrap();
        crate::memory::messages::store_message(&conn, "user", "hello", "sess1");
        crate::memory::messages::store_message(&conn, "assistant", "hi there", "sess1");
        drop(conn);
        let result = forget_with_counts(&sender);
        assert!(result.contains("2 messages"));
        assert!(result.contains("1 summaries"));
        assert!(result.contains("completely fresh"));
        assert!(!memory_db_path(&sender).exists());
    }

    #[test]
    fn test_forget_command_no_data() {
        let sender = format!("forget_empty_{}", std::process::id());
        let result = forget_with_counts(&sender);
        assert!(result.contains("No memory to clear"));
        delete_memory(&sender);
    }
}
