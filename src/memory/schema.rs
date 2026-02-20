use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use tracing::info;

use super::config::config_dir;
use crate::error::AppError;
use crate::helpers::hash_message;

pub(crate) fn memory_dir() -> PathBuf {
    let dir = config_dir().join("memories");
    let _ = std::fs::create_dir_all(&dir);
    dir
}

pub(crate) fn hash_sender(sender: &str) -> String {
    format!("{:x}", hash_message(sender))
}

pub(crate) fn memory_db_path(sender: &str) -> PathBuf {
    memory_dir().join(format!("{}.db", hash_sender(sender)))
}

pub(crate) fn memory_json_path(sender: &str) -> PathBuf {
    memory_dir().join(format!("{}.json", hash_sender(sender)))
}

pub(crate) fn open_memory_db(sender: &str) -> Result<Connection, AppError> {
    let path = memory_db_path(sender);
    let conn = Connection::open(&path)?;
    conn.execute_batch("PRAGMA journal_mode=WAL;")?;
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS messages (
            id INTEGER PRIMARY KEY,
            role TEXT NOT NULL,
            content TEXT NOT NULL,
            timestamp INTEGER NOT NULL,
            session_id TEXT
        );
        CREATE TABLE IF NOT EXISTS summaries (
            id INTEGER PRIMARY KEY,
            summary TEXT NOT NULL,
            timestamp INTEGER NOT NULL
        );
        CREATE VIRTUAL TABLE IF NOT EXISTS messages_fts USING fts5(
            content,
            content=messages,
            content_rowid=id
        );
        CREATE TRIGGER IF NOT EXISTS messages_ai AFTER INSERT ON messages BEGIN
            INSERT INTO messages_fts(rowid, content) VALUES (new.id, new.content);
        END;
        CREATE TRIGGER IF NOT EXISTS messages_ad AFTER DELETE ON messages BEGIN
            INSERT INTO messages_fts(messages_fts, rowid, content) VALUES('delete', old.id, old.content);
        END;
        CREATE TABLE IF NOT EXISTS model_preferences (
            id INTEGER PRIMARY KEY CHECK (id = 1),
            model TEXT NOT NULL,
            updated_at INTEGER NOT NULL
        );
        CREATE TABLE IF NOT EXISTS pins (
            id INTEGER PRIMARY KEY,
            label TEXT NOT NULL UNIQUE,
            content TEXT NOT NULL,
            timestamp INTEGER NOT NULL
        );",
    )?;
    migrate_json_to_sqlite(&conn, sender);
    Ok(conn)
}

#[derive(Serialize, Deserialize)]
struct LegacyMemory {
    summaries: Vec<LegacySummary>,
}

#[derive(Serialize, Deserialize)]
struct LegacySummary {
    summary: String,
    timestamp: u64,
}

fn migrate_json_to_sqlite(conn: &Connection, sender: &str) {
    let json_path = memory_json_path(sender);
    if !json_path.exists() {
        return;
    }
    let Ok(contents) = std::fs::read_to_string(&json_path) else {
        return;
    };
    let Ok(legacy) = serde_json::from_str::<LegacyMemory>(&contents) else {
        let _ = std::fs::remove_file(&json_path);
        return;
    };
    for s in &legacy.summaries {
        let _ = conn.execute(
            "INSERT INTO summaries (summary, timestamp) VALUES (?1, ?2)",
            rusqlite::params![s.summary, s.timestamp as i64],
        );
    }
    let _ = std::fs::remove_file(&json_path);
    info!("Migrated JSON memory to SQLite for sender");
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::{delete_memory, messages::get_recent_summaries};

    #[test]
    fn test_hash_sender_deterministic() {
        let h1 = hash_sender("+1234567890");
        let h2 = hash_sender("+1234567890");
        assert_eq!(h1, h2);
        let h3 = hash_sender("+9876543210");
        assert_ne!(h1, h3);
    }

    #[test]
    fn test_memory_db_path_valid() {
        let path = memory_db_path("+1234567890");
        assert!(path.to_str().unwrap().contains("memories"));
        assert!(path.to_str().unwrap().ends_with(".db"));
    }

    #[test]
    fn test_open_memory_db_creates_schema() {
        let sender = format!("schema_test_{}", std::process::id());
        let conn = open_memory_db(&sender).unwrap();
        conn.execute(
            "INSERT INTO messages (role, content, timestamp, session_id) VALUES ('user', 'hello', 1, 'test')",
            [],
        )
        .unwrap();
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM messages", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 1);
        delete_memory(&sender);
    }

    #[test]
    fn test_json_migration_to_sqlite() {
        let sender = format!("migrate_test_{}", std::process::id());
        let legacy_json = serde_json::json!({
            "summaries": [
                {"summary": "First session summary", "timestamp": 1700000000u64},
                {"summary": "Second session summary", "timestamp": 1700100000u64}
            ]
        });
        let json_path = memory_json_path(&sender);
        let _ = std::fs::create_dir_all(json_path.parent().unwrap());
        std::fs::write(&json_path, legacy_json.to_string()).unwrap();
        let conn = open_memory_db(&sender).unwrap();
        let summaries = get_recent_summaries(&conn, 5);
        assert_eq!(summaries.len(), 2);
        assert!(!json_path.exists());
        delete_memory(&sender);
    }

    #[test]
    fn test_json_migration_old_format_removed() {
        let sender = format!("migrate_old_{}", std::process::id());
        let old_json = serde_json::json!({
            "sender": sender,
            "summary": "Old single summary",
            "updated_at": "2026-01-01T00:00:00Z"
        });
        let json_path = memory_json_path(&sender);
        let _ = std::fs::create_dir_all(json_path.parent().unwrap());
        std::fs::write(&json_path, old_json.to_string()).unwrap();
        let conn = open_memory_db(&sender).unwrap();
        let summaries = get_recent_summaries(&conn, 5);
        assert_eq!(summaries.len(), 0);
        assert!(!json_path.exists());
        drop(conn);
        delete_memory(&sender);
    }
}
