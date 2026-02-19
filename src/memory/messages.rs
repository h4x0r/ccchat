use rusqlite::Connection;
use tracing::error;

pub(crate) fn store_message(conn: &Connection, role: &str, content: &str, session_id: &str) {
    let timestamp = crate::helpers::epoch_now();
    if let Err(e) = conn.execute(
        "INSERT INTO messages (role, content, timestamp, session_id) VALUES (?1, ?2, ?3, ?4)",
        rusqlite::params![role, content, timestamp, session_id],
    ) {
        error!("Failed to store message: {e}");
    }
}

pub(crate) fn store_summary(conn: &Connection, summary: &str) {
    let timestamp = crate::helpers::epoch_now();
    if let Err(e) = conn.execute(
        "INSERT INTO summaries (summary, timestamp) VALUES (?1, ?2)",
        rusqlite::params![summary, timestamp],
    ) {
        error!("Failed to store summary: {e}");
    }
    let _ = conn.execute(
        &format!(
            "DELETE FROM summaries WHERE id NOT IN (SELECT id FROM summaries ORDER BY timestamp DESC, id DESC LIMIT {})",
            crate::constants::MAX_SUMMARIES
        ),
        [],
    );
}

pub(crate) fn search_memory(conn: &Connection, query: &str, limit: usize) -> Vec<(String, String, i64)> {
    let sanitized: String = query
        .chars()
        .map(|c| if c.is_alphanumeric() || c == ' ' { c } else { ' ' })
        .collect();
    let terms: Vec<&str> = sanitized.split_whitespace().collect();
    if terms.is_empty() {
        return Vec::new();
    }
    let fts_query = terms.join(" OR ");
    let sql = "SELECT m.role, m.content, m.timestamp FROM messages m
               JOIN messages_fts f ON m.id = f.rowid
               WHERE messages_fts MATCH ?1
               ORDER BY f.rank
               LIMIT ?2";
    let Ok(mut stmt) = conn.prepare(sql) else {
        return Vec::new();
    };
    let rows = stmt
        .query_map(rusqlite::params![fts_query, limit as i64], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, i64>(2)?,
            ))
        })
        .ok();
    match rows {
        Some(iter) => iter.filter_map(|r| r.ok()).collect(),
        None => Vec::new(),
    }
}

pub(crate) fn get_recent_summaries(conn: &Connection, limit: usize) -> Vec<(String, i64)> {
    let sql = "SELECT summary, timestamp FROM summaries ORDER BY timestamp DESC, id DESC LIMIT ?1";
    let Ok(mut stmt) = conn.prepare(sql) else {
        return Vec::new();
    };
    let rows = stmt
        .query_map(rusqlite::params![limit as i64], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
        })
        .ok();
    match rows {
        Some(iter) => iter.filter_map(|r| r.ok()).collect(),
        None => Vec::new(),
    }
}

pub(crate) fn get_message_count(conn: &Connection) -> i64 {
    conn.query_row("SELECT COUNT(*) FROM messages", [], |row| row.get(0))
        .unwrap_or(0)
}

pub(crate) fn get_summary_count(conn: &Connection) -> i64 {
    conn.query_row("SELECT COUNT(*) FROM summaries", [], |row| row.get(0))
        .unwrap_or(0)
}

pub(crate) fn get_oldest_message_ts(conn: &Connection) -> Option<i64> {
    conn.query_row(
        "SELECT MIN(timestamp) FROM messages",
        [],
        |row| row.get::<_, Option<i64>>(0),
    )
    .ok()
    .flatten()
}

pub(crate) fn save_model_preference(conn: &Connection, model: &str) {
    let timestamp = crate::helpers::epoch_now();
    if let Err(e) = conn.execute(
        "INSERT INTO model_preferences (id, model, updated_at) VALUES (1, ?1, ?2)
         ON CONFLICT(id) DO UPDATE SET model = excluded.model, updated_at = excluded.updated_at",
        rusqlite::params![model, timestamp],
    ) {
        error!("Failed to save model preference: {e}");
    }
}

pub(crate) fn load_model_preference(conn: &Connection) -> Option<String> {
    conn.query_row(
        "SELECT model FROM model_preferences WHERE id = 1",
        [],
        |row| row.get(0),
    )
    .ok()
}

pub(crate) fn purge_old_messages(conn: &Connection, days: u32) {
    let cutoff = crate::helpers::epoch_now() - (days as i64 * crate::constants::SECS_PER_DAY);
    let _ = conn.execute(
        "DELETE FROM messages WHERE timestamp < ?1",
        rusqlite::params![cutoff],
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::{delete_memory, schema::open_memory_db};

    #[test]
    fn test_store_and_retrieve_messages() {
        let sender = format!("msg_test_{}", std::process::id());
        let conn = open_memory_db(&sender).unwrap();
        store_message(&conn, "user", "What is Rust?", "sess1");
        store_message(&conn, "assistant", "Rust is a systems programming language.", "sess1");
        assert_eq!(get_message_count(&conn), 2);
        delete_memory(&sender);
    }

    #[test]
    fn test_fts5_search() {
        let sender = format!("fts_test_{}", std::process::id());
        let conn = open_memory_db(&sender).unwrap();
        store_message(&conn, "user", "How do I write async Rust code?", "sess1");
        store_message(&conn, "assistant", "Use tokio with async/await syntax.", "sess1");
        store_message(&conn, "user", "What is the best pizza in NYC?", "sess2");
        store_message(&conn, "assistant", "Try Di Fara Pizza in Brooklyn.", "sess2");
        let results = search_memory(&conn, "async Rust", 5);
        assert!(!results.is_empty());
        assert!(results.iter().any(|(_, content, _)| content.contains("async")));
        let results = search_memory(&conn, "pizza", 5);
        assert!(results.iter().any(|(_, content, _)| content.contains("pizza") || content.contains("Pizza")));
        delete_memory(&sender);
    }

    #[test]
    fn test_store_summaries() {
        let sender = format!("sum_test_{}", std::process::id());
        let conn = open_memory_db(&sender).unwrap();
        store_summary(&conn, "Discussed Rust testing patterns.");
        store_summary(&conn, "Talked about Tokyo travel plans.");
        let summaries = get_recent_summaries(&conn, 5);
        assert_eq!(summaries.len(), 2);
        assert!(summaries[0].0.contains("Tokyo"));
        assert!(summaries[1].0.contains("Rust"));
        delete_memory(&sender);
    }

    #[test]
    fn test_summaries_max_5() {
        let sender = format!("sum_max_{}", std::process::id());
        let conn = open_memory_db(&sender).unwrap();
        for i in 1..=7 {
            store_summary(&conn, &format!("Summary {i}"));
        }
        let summaries = get_recent_summaries(&conn, 10);
        assert_eq!(summaries.len(), 5);
        assert!(summaries[0].0.contains("7"));
        assert!(summaries[4].0.contains("3"));
        delete_memory(&sender);
    }

    #[test]
    fn test_purge_old_messages() {
        use std::time::{SystemTime, UNIX_EPOCH};
        let sender = format!("purge_test_{}", std::process::id());
        let conn = open_memory_db(&sender).unwrap();
        let old_ts = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64
            - (60 * 86400);
        conn.execute(
            "INSERT INTO messages (role, content, timestamp, session_id) VALUES ('user', 'old msg', ?1, 'old')",
            rusqlite::params![old_ts],
        )
        .unwrap();
        store_message(&conn, "user", "recent msg", "new");
        assert_eq!(get_message_count(&conn), 2);
        purge_old_messages(&conn, 30);
        assert_eq!(get_message_count(&conn), 1);
        delete_memory(&sender);
    }

    #[test]
    fn test_get_oldest_message_ts_empty() {
        let sender = format!("oldest_empty_{}", std::process::id());
        let conn = open_memory_db(&sender).unwrap();
        assert_eq!(get_oldest_message_ts(&conn), None);
        delete_memory(&sender);
    }

    #[test]
    fn test_get_oldest_message_ts_with_data() {
        let sender = format!("oldest_data_{}", std::process::id());
        let conn = open_memory_db(&sender).unwrap();
        conn.execute(
            "INSERT INTO messages (role, content, timestamp, session_id) VALUES ('user', 'old', 100, 'a')",
            [],
        ).unwrap();
        conn.execute(
            "INSERT INTO messages (role, content, timestamp, session_id) VALUES ('user', 'new', 200, 'a')",
            [],
        ).unwrap();
        assert_eq!(get_oldest_message_ts(&conn), Some(100));
        delete_memory(&sender);
    }

    #[test]
    fn test_get_summary_count_empty() {
        let sender = format!("sumcnt_empty_{}", std::process::id());
        let conn = open_memory_db(&sender).unwrap();
        assert_eq!(get_summary_count(&conn), 0);
        delete_memory(&sender);
    }

    #[test]
    fn test_get_summary_count_with_data() {
        let sender = format!("sumcnt_data_{}", std::process::id());
        let conn = open_memory_db(&sender).unwrap();
        store_summary(&conn, "Summary 1");
        store_summary(&conn, "Summary 2");
        assert_eq!(get_summary_count(&conn), 2);
        delete_memory(&sender);
    }

    #[test]
    fn test_persist_model_preference_round_trip() {
        let sender = format!("modelpref_rt_{}", std::process::id());
        let conn = open_memory_db(&sender).unwrap();
        assert_eq!(load_model_preference(&conn), None);
        save_model_preference(&conn, "claude-3-haiku");
        assert_eq!(load_model_preference(&conn).as_deref(), Some("claude-3-haiku"));
        delete_memory(&sender);
    }

    #[test]
    fn test_persist_model_preference_updates_existing() {
        let sender = format!("modelpref_upd_{}", std::process::id());
        let conn = open_memory_db(&sender).unwrap();
        save_model_preference(&conn, "claude-3-haiku");
        save_model_preference(&conn, "claude-3-opus");
        assert_eq!(load_model_preference(&conn).as_deref(), Some("claude-3-opus"));
        delete_memory(&sender);
    }

    #[test]
    fn test_load_model_preference_none_when_absent() {
        let sender = format!("modelpref_none_{}", std::process::id());
        let conn = open_memory_db(&sender).unwrap();
        assert_eq!(load_model_preference(&conn), None);
        delete_memory(&sender);
    }
}
