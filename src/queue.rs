use rusqlite::Connection;
use tracing::error;

use crate::error::AppError;

pub(crate) const MAX_RETRIES: i64 = 5;

fn config_dir() -> std::path::PathBuf {
    let dir = std::env::var("HOME")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| std::path::PathBuf::from("."))
        .join(".config")
        .join("ccchat");
    let _ = std::fs::create_dir_all(&dir);
    dir
}

pub(crate) fn open_queue_db() -> Result<Connection, AppError> {
    let path = config_dir().join("queue.db");
    let conn = Connection::open(&path)?;
    conn.execute_batch("PRAGMA journal_mode=WAL;")?;
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS message_queue (
            id INTEGER PRIMARY KEY,
            sender TEXT NOT NULL,
            content TEXT NOT NULL,
            attachments_json TEXT NOT NULL DEFAULT '[]',
            timestamp INTEGER NOT NULL,
            status TEXT NOT NULL DEFAULT 'pending',
            retry_count INTEGER NOT NULL DEFAULT 0
        );",
    )?;
    Ok(conn)
}

pub(crate) fn enqueue(conn: &Connection, sender: &str, content: &str, attachments_json: &str) {
    let timestamp = crate::helpers::epoch_now();
    if let Err(e) = conn.execute(
        "INSERT INTO message_queue (sender, content, attachments_json, timestamp) VALUES (?1, ?2, ?3, ?4)",
        rusqlite::params![sender, content, attachments_json, timestamp],
    ) {
        error!("Failed to enqueue message: {e}");
    }
}

pub(crate) fn mark_completed(conn: &Connection, id: i64) {
    if let Err(e) = conn.execute(
        "UPDATE message_queue SET status = 'completed' WHERE id = ?1",
        rusqlite::params![id],
    ) {
        error!("Failed to mark message completed: {e}");
    }
}

pub(crate) fn increment_retry(conn: &Connection, id: i64) {
    if let Err(e) = conn.execute(
        "UPDATE message_queue SET retry_count = retry_count + 1 WHERE id = ?1",
        rusqlite::params![id],
    ) {
        error!("Failed to increment retry count: {e}");
    }
}

/// Returns pending messages: (id, sender, content, attachments_json).
/// Excludes messages that have reached MAX_RETRIES.
pub(crate) fn get_pending(conn: &Connection) -> Vec<(i64, String, String, String)> {
    let sql = "SELECT id, sender, content, attachments_json FROM message_queue
               WHERE status = 'pending' AND retry_count < ?1
               ORDER BY timestamp ASC";
    let Ok(mut stmt) = conn.prepare(sql) else {
        return Vec::new();
    };
    let rows = stmt
        .query_map(rusqlite::params![MAX_RETRIES], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
            ))
        })
        .ok();
    match rows {
        Some(iter) => iter.filter_map(|r| r.ok()).collect(),
        None => Vec::new(),
    }
}

pub(crate) fn purge_completed(conn: &Connection) {
    let _ = conn.execute("DELETE FROM message_queue WHERE status = 'completed'", []);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_queue_db() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE message_queue (
                id INTEGER PRIMARY KEY,
                sender TEXT NOT NULL,
                content TEXT NOT NULL,
                attachments_json TEXT NOT NULL DEFAULT '[]',
                timestamp INTEGER NOT NULL,
                status TEXT NOT NULL DEFAULT 'pending',
                retry_count INTEGER NOT NULL DEFAULT 0
            );",
        )
        .unwrap();
        conn
    }

    #[test]
    fn test_enqueue_message() {
        let conn = test_queue_db();
        enqueue(&conn, "+sender", "hello", "[]");
        let pending = get_pending(&conn);
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].1, "+sender");
        assert_eq!(pending[0].2, "hello");
    }

    #[test]
    fn test_mark_completed() {
        let conn = test_queue_db();
        enqueue(&conn, "+sender", "hello", "[]");
        let pending = get_pending(&conn);
        assert_eq!(pending.len(), 1);
        mark_completed(&conn, pending[0].0);
        let pending = get_pending(&conn);
        assert!(pending.is_empty());
    }

    #[test]
    fn test_increment_retry_count() {
        let conn = test_queue_db();
        enqueue(&conn, "+sender", "hello", "[]");
        let pending = get_pending(&conn);
        let id = pending[0].0;
        increment_retry(&conn, id);
        increment_retry(&conn, id);
        let count: i64 = conn
            .query_row(
                "SELECT retry_count FROM message_queue WHERE id = ?1",
                rusqlite::params![id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, 2);
    }

    #[test]
    fn test_max_retries_excludes_from_pending() {
        let conn = test_queue_db();
        enqueue(&conn, "+sender", "hello", "[]");
        let id = get_pending(&conn)[0].0;
        for _ in 0..MAX_RETRIES {
            increment_retry(&conn, id);
        }
        let pending = get_pending(&conn);
        assert!(pending.is_empty(), "should be excluded after MAX_RETRIES");
    }

    #[test]
    fn test_get_pending_ordered_by_timestamp() {
        let conn = test_queue_db();
        // Insert with explicit timestamps to control order
        conn.execute(
            "INSERT INTO message_queue (sender, content, attachments_json, timestamp) VALUES (?1, ?2, ?3, ?4)",
            rusqlite::params!["+a", "second", "[]", 200],
        ).unwrap();
        conn.execute(
            "INSERT INTO message_queue (sender, content, attachments_json, timestamp) VALUES (?1, ?2, ?3, ?4)",
            rusqlite::params!["+b", "first", "[]", 100],
        ).unwrap();
        let pending = get_pending(&conn);
        assert_eq!(pending.len(), 2);
        assert_eq!(pending[0].2, "first");
        assert_eq!(pending[1].2, "second");
    }

    #[test]
    fn test_enqueue_with_attachments_json() {
        let conn = test_queue_db();
        let attachments = r#"[{"id":"abc","type":"image/png"}]"#;
        enqueue(&conn, "+sender", "pic", attachments);
        let pending = get_pending(&conn);
        assert_eq!(pending[0].3, attachments);
    }

    #[test]
    fn test_purge_completed_messages() {
        let conn = test_queue_db();
        enqueue(&conn, "+a", "msg1", "[]");
        enqueue(&conn, "+b", "msg2", "[]");
        let pending = get_pending(&conn);
        mark_completed(&conn, pending[0].0);
        purge_completed(&conn);
        // Only one row should remain (the pending one)
        let total: i64 = conn
            .query_row("SELECT COUNT(*) FROM message_queue", [], |row| row.get(0))
            .unwrap();
        assert_eq!(total, 1);
    }
}
