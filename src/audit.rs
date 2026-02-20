use rusqlite::Connection;
use tracing::error;

use crate::error::AppError;

fn config_dir() -> std::path::PathBuf {
    let dir = std::env::var("HOME")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| std::path::PathBuf::from("."))
        .join(".config")
        .join("ccchat");
    let _ = std::fs::create_dir_all(&dir);
    dir
}

pub(crate) fn open_audit_db() -> Result<Connection, AppError> {
    let path = config_dir().join("audit.db");
    let conn = Connection::open(&path)?;
    conn.execute_batch("PRAGMA journal_mode=WAL;")?;
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS audit_log (
            id INTEGER PRIMARY KEY,
            action TEXT NOT NULL,
            target_id TEXT NOT NULL DEFAULT '',
            detail TEXT NOT NULL DEFAULT '',
            timestamp INTEGER NOT NULL
        );",
    )?;
    Ok(conn)
}

pub(crate) fn log_action(action: &str, target_id: &str, detail: &str) {
    let Ok(conn) = open_audit_db() else { return };
    let ts = crate::helpers::epoch_now();
    if let Err(e) = conn.execute(
        "INSERT INTO audit_log (action, target_id, detail, timestamp) VALUES (?1, ?2, ?3, ?4)",
        rusqlite::params![action, target_id, detail, ts],
    ) {
        error!("Audit log write failed: {e}");
    }
}

pub(crate) fn get_recent_actions(limit: usize) -> Vec<(String, String, String, i64)> {
    let Ok(conn) = open_audit_db() else {
        return Vec::new();
    };
    let sql = "SELECT action, target_id, detail, timestamp FROM audit_log ORDER BY timestamp DESC, id DESC LIMIT ?1";
    let Ok(mut stmt) = conn.prepare(sql) else {
        return Vec::new();
    };
    let rows = stmt
        .query_map(rusqlite::params![limit as i64], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, i64>(3)?,
            ))
        })
        .ok();
    match rows {
        Some(iter) => iter.filter_map(|r| r.ok()).collect(),
        None => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_log_action_and_retrieve() {
        log_action("test_action", "target_1", "test detail");
        let actions = get_recent_actions(10);
        let found = actions
            .iter()
            .any(|(a, t, d, _)| a == "test_action" && t == "target_1" && d == "test detail");
        assert!(found, "Expected to find logged action, got: {actions:?}");
    }

    #[test]
    fn test_get_recent_actions_ordered() {
        let uid = uuid::Uuid::new_v4().to_string();
        let a1 = format!("first_{uid}");
        let a2 = format!("second_{uid}");
        log_action(&a1, "", "");
        std::thread::sleep(std::time::Duration::from_millis(10));
        log_action(&a2, "", "");
        let actions = get_recent_actions(10);
        let pos1 = actions.iter().position(|(a, _, _, _)| *a == a1);
        let pos2 = actions.iter().position(|(a, _, _, _)| *a == a2);
        if let (Some(p1), Some(p2)) = (pos1, pos2) {
            assert!(p2 < p1, "second action should appear first (newest first)");
        }
    }

    #[test]
    fn test_get_recent_actions_limit() {
        let uid = uuid::Uuid::new_v4().to_string();
        for i in 0..5 {
            log_action(&format!("limit_{uid}_{i}"), "", "");
        }
        let actions = get_recent_actions(3);
        assert!(
            actions.len() <= 3,
            "Expected at most 3, got {}",
            actions.len()
        );
    }
}
