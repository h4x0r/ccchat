use tracing::error;

use super::messages::{get_recent_summaries, search_memory, store_message, store_summary};
use super::schema::{memory_db_path, memory_json_path, open_memory_db};

pub(crate) fn save_memory(sender: &str, summary: &str) {
    match open_memory_db(sender) {
        Ok(conn) => store_summary(&conn, summary),
        Err(e) => error!("Failed to open memory DB: {e}"),
    }
}

pub(crate) fn delete_memory(sender: &str) {
    let db_path = memory_db_path(sender);
    let _ = std::fs::remove_file(&db_path);
    let _ = std::fs::remove_file(db_path.with_extension("db-wal"));
    let _ = std::fs::remove_file(db_path.with_extension("db-shm"));
    let _ = std::fs::remove_file(memory_json_path(sender));
}

pub(crate) fn inject_context(sender: &str, text: &str) -> String {
    let conn = match open_memory_db(sender) {
        Ok(c) => c,
        Err(_) => return text.to_string(),
    };

    let summaries = get_recent_summaries(&conn, crate::constants::MEMORY_SEARCH_LIMIT);
    let relevant = search_memory(&conn, text, crate::constants::MEMORY_SEARCH_LIMIT);

    if summaries.is_empty() && relevant.is_empty() {
        return text.to_string();
    }

    let mut ctx = String::new();

    if !summaries.is_empty() {
        ctx.push_str("Previous conversations:\n");
        for (i, (summary, ts)) in summaries.iter().rev().enumerate() {
            let date = format_epoch(*ts as u64);
            ctx.push_str(&format!("- Session {} ({}): {}\n", i + 1, date, summary));
        }
    }

    if !relevant.is_empty() {
        ctx.push_str("\nRelevant past messages:\n");
        for (role, content, _ts) in &relevant {
            let short = if content.len() > 200 {
                format!("{}...", &content[..200])
            } else {
                content.clone()
            };
            ctx.push_str(&format!("- [{role}]: {short}\n"));
        }
    }

    ctx.push_str("---\n");
    ctx.push_str(text);
    ctx
}

pub(crate) fn store_message_pair(
    sender: &str,
    user_msg: &str,
    assistant_msg: &str,
    session_id: &str,
) {
    match open_memory_db(sender) {
        Ok(conn) => {
            store_message(&conn, "user", user_msg, session_id);
            store_message(&conn, "assistant", assistant_msg, session_id);
        }
        Err(e) => error!("Failed to store message pair: {e}"),
    }
}

pub(crate) fn format_epoch(epoch: u64) -> String {
    let days = epoch / crate::constants::SECS_PER_DAY as u64;
    let mut y = 1970i64;
    let mut remaining = days as i64;
    loop {
        let days_in_year = if y % 4 == 0 && (y % 100 != 0 || y % 400 == 0) {
            366
        } else {
            365
        };
        if remaining < days_in_year {
            break;
        }
        remaining -= days_in_year;
        y += 1;
    }
    let leap = y % 4 == 0 && (y % 100 != 0 || y % 400 == 0);
    let days_in_months: [i64; 12] = [
        31,
        if leap { 29 } else { 28 },
        31,
        30,
        31,
        30,
        31,
        31,
        30,
        31,
        30,
        31,
    ];
    let mut m = 0usize;
    for (i, &dim) in days_in_months.iter().enumerate() {
        if remaining < dim {
            m = i;
            break;
        }
        remaining -= dim;
    }
    format!("{:04}-{:02}-{:02}", y, m + 1, remaining + 1)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::{open_memory_db, store_message};

    #[test]
    fn test_format_epoch() {
        assert_eq!(format_epoch(0), "1970-01-01");
        assert_eq!(format_epoch(86400), "1970-01-02");
        assert_eq!(format_epoch(1704067200), "2024-01-01");
    }

    #[test]
    fn test_inject_context_no_memory() {
        let sender = format!("no_memory_sender_{}", std::process::id());
        let result = inject_context(&sender, "Hello");
        assert_eq!(result, "Hello");
        delete_memory(&sender);
    }

    #[test]
    fn test_inject_context_with_memory() {
        let sender = format!("ctx_sender_{}", std::process::id());
        save_memory(&sender, "User prefers concise answers.");
        let result = inject_context(&sender, "What is Rust?");
        assert!(result.contains("Previous conversations:"));
        assert!(result.contains("User prefers concise answers."));
        assert!(result.contains("What is Rust?"));
        delete_memory(&sender);
    }

    #[test]
    fn test_inject_context_with_relevant_messages() {
        let sender = format!("ctx_msg_{}", std::process::id());
        let conn = open_memory_db(&sender).unwrap();
        store_message(&conn, "user", "How do I configure nginx?", "sess1");
        store_message(
            &conn,
            "assistant",
            "Edit /etc/nginx/nginx.conf for main config.",
            "sess1",
        );
        drop(conn);
        let result = inject_context(&sender, "Tell me about nginx");
        assert!(result.contains("Relevant past messages:"));
        assert!(result.contains("nginx"));
        delete_memory(&sender);
    }

    #[test]
    fn test_save_memory_uses_sqlite() {
        let sender = format!("save_mem_{}", std::process::id());
        save_memory(&sender, "Discussed Rust testing patterns.");
        let conn = open_memory_db(&sender).unwrap();
        let summaries = crate::memory::messages::get_recent_summaries(&conn, 5);
        assert_eq!(summaries.len(), 1);
        assert_eq!(summaries[0].0, "Discussed Rust testing patterns.");
        delete_memory(&sender);
    }

    #[test]
    fn test_store_message_pair() {
        let sender = format!("pair_test_{}", std::process::id());
        store_message_pair(&sender, "What is 2+2?", "2+2 equals 4.", "sess1");
        let conn = open_memory_db(&sender).unwrap();
        assert_eq!(crate::memory::messages::get_message_count(&conn), 2);
        let results = crate::memory::messages::search_memory(&conn, "2+2", 5);
        assert!(!results.is_empty());
        delete_memory(&sender);
    }

    #[test]
    fn test_delete_all_memory() {
        let sender = format!("del_test_{}", std::process::id());
        save_memory(&sender, "Some context.");
        let conn = open_memory_db(&sender).unwrap();
        store_message(&conn, "user", "hello", "sess1");
        drop(conn);
        assert!(crate::memory::schema::memory_db_path(&sender).exists());
        delete_memory(&sender);
        assert!(!crate::memory::schema::memory_db_path(&sender).exists());
    }

    #[test]
    fn test_forget_clears_memory() {
        let sender = format!("forget_sender_{}", std::process::id());
        save_memory(&sender, "Some old context.");
        assert!(crate::memory::schema::memory_db_path(&sender).exists());
        delete_memory(&sender);
        assert!(!crate::memory::schema::memory_db_path(&sender).exists());
    }
}
