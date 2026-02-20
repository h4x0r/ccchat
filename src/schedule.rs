use std::str::FromStr;

use chrono::Utc;
use croner::Cron;
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

pub(crate) fn open_schedule_db() -> Result<Connection, AppError> {
    let path = config_dir().join("schedule.db");
    let conn = Connection::open(&path)?;
    conn.execute_batch("PRAGMA journal_mode=WAL;")?;
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS reminders (
            id INTEGER PRIMARY KEY,
            sender TEXT NOT NULL,
            message TEXT NOT NULL,
            deliver_at INTEGER NOT NULL,
            status TEXT NOT NULL DEFAULT 'pending',
            created_at INTEGER NOT NULL
        );",
    )?;
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS cron_jobs (
            id INTEGER PRIMARY KEY,
            sender TEXT NOT NULL,
            message TEXT NOT NULL,
            job_type TEXT NOT NULL DEFAULT 'cron',
            cron_pattern TEXT,
            interval_secs INTEGER,
            next_delivery_at INTEGER NOT NULL,
            last_delivered_at INTEGER,
            status TEXT NOT NULL DEFAULT 'active',
            created_at INTEGER NOT NULL
        );",
    )?;
    Ok(conn)
}

pub(crate) fn add_reminder(conn: &Connection, sender: &str, message: &str, deliver_at: i64) -> i64 {
    let now = crate::helpers::epoch_now();
    if let Err(e) = conn.execute(
        "INSERT INTO reminders (sender, message, deliver_at, created_at) VALUES (?1, ?2, ?3, ?4)",
        rusqlite::params![sender, message, deliver_at, now],
    ) {
        error!("Failed to add reminder: {e}");
        return 0;
    }
    conn.last_insert_rowid()
}

pub(crate) fn get_due_reminders(conn: &Connection) -> Vec<(i64, String, String)> {
    let now = crate::helpers::epoch_now();
    let sql = "SELECT id, sender, message FROM reminders WHERE status = 'pending' AND deliver_at <= ?1";
    let Ok(mut stmt) = conn.prepare(sql) else {
        return Vec::new();
    };
    stmt.query_map(rusqlite::params![now], |row| {
        Ok((
            row.get::<_, i64>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
        ))
    })
    .ok()
    .map(|iter| iter.filter_map(|r| r.ok()).collect())
    .unwrap_or_default()
}

pub(crate) fn mark_delivered(conn: &Connection, id: i64) {
    let _ = conn.execute(
        "UPDATE reminders SET status = 'delivered' WHERE id = ?1",
        rusqlite::params![id],
    );
}

pub(crate) fn get_pending_reminders(conn: &Connection, sender: &str) -> Vec<(i64, String, i64)> {
    let sql = "SELECT id, message, deliver_at FROM reminders WHERE sender = ?1 AND status = 'pending' ORDER BY deliver_at";
    let Ok(mut stmt) = conn.prepare(sql) else {
        return Vec::new();
    };
    stmt.query_map(rusqlite::params![sender], |row| {
        Ok((
            row.get::<_, i64>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, i64>(2)?,
        ))
    })
    .ok()
    .map(|iter| iter.filter_map(|r| r.ok()).collect())
    .unwrap_or_default()
}

pub(crate) fn cancel_reminder(conn: &Connection, id: i64, sender: &str) -> bool {
    let rows = conn
        .execute(
            "DELETE FROM reminders WHERE id = ?1 AND sender = ?2 AND status = 'pending'",
            rusqlite::params![id, sender],
        )
        .unwrap_or(0);
    rows > 0
}

pub(crate) fn purge_delivered(conn: &Connection) {
    let _ = conn.execute("DELETE FROM reminders WHERE status = 'delivered'", []);
}

// --- Cron jobs ---

fn compute_next_cron_delivery(cron_pattern: &str) -> Option<i64> {
    let cron = Cron::from_str(cron_pattern).ok()?;
    let next = cron.find_next_occurrence(&Utc::now(), false).ok()?;
    Some(next.timestamp())
}

pub(crate) fn add_cron_job(conn: &Connection, sender: &str, message: &str, cron_pattern: &str) -> i64 {
    let next = match compute_next_cron_delivery(cron_pattern) {
        Some(ts) => ts,
        None => return 0,
    };
    let now = crate::helpers::epoch_now();
    if let Err(e) = conn.execute(
        "INSERT INTO cron_jobs (sender, message, job_type, cron_pattern, next_delivery_at, created_at) VALUES (?1, ?2, 'cron', ?3, ?4, ?5)",
        rusqlite::params![sender, message, cron_pattern, next, now],
    ) {
        error!("Failed to add cron job: {e}");
        return 0;
    }
    conn.last_insert_rowid()
}

pub(crate) fn add_interval_job(conn: &Connection, sender: &str, message: &str, interval_secs: i64) -> i64 {
    let now = crate::helpers::epoch_now();
    let next = now + interval_secs;
    if let Err(e) = conn.execute(
        "INSERT INTO cron_jobs (sender, message, job_type, interval_secs, next_delivery_at, created_at) VALUES (?1, ?2, 'interval', ?3, ?4, ?5)",
        rusqlite::params![sender, message, interval_secs, next, now],
    ) {
        error!("Failed to add interval job: {e}");
        return 0;
    }
    conn.last_insert_rowid()
}

/// Returns due cron jobs: (id, sender, message, cron_pattern, interval_secs).
pub(crate) fn get_due_cron_jobs(conn: &Connection) -> Vec<(i64, String, String, Option<String>, Option<i64>)> {
    let now = crate::helpers::epoch_now();
    let sql = "SELECT id, sender, message, cron_pattern, interval_secs FROM cron_jobs WHERE status = 'active' AND next_delivery_at <= ?1";
    let Ok(mut stmt) = conn.prepare(sql) else {
        return Vec::new();
    };
    stmt.query_map(rusqlite::params![now], |row| {
        Ok((
            row.get::<_, i64>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, Option<String>>(3)?,
            row.get::<_, Option<i64>>(4)?,
        ))
    })
    .ok()
    .map(|iter| iter.filter_map(|r| r.ok()).collect())
    .unwrap_or_default()
}

pub(crate) fn advance_cron_job(conn: &Connection, id: i64, cron_pattern: Option<&str>, interval_secs: Option<i64>) {
    let now = crate::helpers::epoch_now();
    let next = if let Some(pattern) = cron_pattern {
        compute_next_cron_delivery(pattern).unwrap_or(now + 3600)
    } else if let Some(secs) = interval_secs {
        now + secs
    } else {
        now + 3600
    };
    let _ = conn.execute(
        "UPDATE cron_jobs SET next_delivery_at = ?1, last_delivered_at = ?2 WHERE id = ?3",
        rusqlite::params![next, now, id],
    );
}

/// Returns active cron jobs for a sender: (id, message, job_type, cron_pattern, interval_secs, next_delivery_at).
pub(crate) fn get_active_cron_jobs(conn: &Connection, sender: &str) -> Vec<(i64, String, String, Option<String>, Option<i64>, i64)> {
    let sql = "SELECT id, message, job_type, cron_pattern, interval_secs, next_delivery_at FROM cron_jobs WHERE sender = ?1 AND status = 'active' ORDER BY next_delivery_at";
    let Ok(mut stmt) = conn.prepare(sql) else {
        return Vec::new();
    };
    stmt.query_map(rusqlite::params![sender], |row| {
        Ok((
            row.get::<_, i64>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, Option<String>>(3)?,
            row.get::<_, Option<i64>>(4)?,
            row.get::<_, i64>(5)?,
        ))
    })
    .ok()
    .map(|iter| iter.filter_map(|r| r.ok()).collect())
    .unwrap_or_default()
}

pub(crate) fn cancel_cron_job(conn: &Connection, id: i64, sender: &str) -> bool {
    let rows = conn
        .execute(
            "DELETE FROM cron_jobs WHERE id = ?1 AND sender = ?2",
            rusqlite::params![id, sender],
        )
        .unwrap_or(0);
    rows > 0
}

pub(crate) fn pause_cron_job(conn: &Connection, id: i64, sender: &str) -> bool {
    let rows = conn
        .execute(
            "UPDATE cron_jobs SET status = 'paused' WHERE id = ?1 AND sender = ?2 AND status = 'active'",
            rusqlite::params![id, sender],
        )
        .unwrap_or(0);
    rows > 0
}

pub(crate) fn resume_cron_job(conn: &Connection, id: i64, sender: &str) -> bool {
    // Fetch the job's pattern/interval to recompute next delivery
    let job: Option<(Option<String>, Option<i64>)> = conn
        .query_row(
            "SELECT cron_pattern, interval_secs FROM cron_jobs WHERE id = ?1 AND sender = ?2 AND status = 'paused'",
            rusqlite::params![id, sender],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .ok();
    let Some((pattern, interval)) = job else {
        return false;
    };
    let now = crate::helpers::epoch_now();
    let next = if let Some(ref p) = pattern {
        compute_next_cron_delivery(p).unwrap_or(now + 3600)
    } else if let Some(secs) = interval {
        now + secs
    } else {
        now + 3600
    };
    let rows = conn
        .execute(
            "UPDATE cron_jobs SET status = 'active', next_delivery_at = ?1 WHERE id = ?2",
            rusqlite::params![next, id],
        )
        .unwrap_or(0);
    rows > 0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_schedule_db() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE reminders (
                id INTEGER PRIMARY KEY,
                sender TEXT NOT NULL,
                message TEXT NOT NULL,
                deliver_at INTEGER NOT NULL,
                status TEXT NOT NULL DEFAULT 'pending',
                created_at INTEGER NOT NULL
            );",
        )
        .unwrap();
        conn.execute_batch(
            "CREATE TABLE cron_jobs (
                id INTEGER PRIMARY KEY,
                sender TEXT NOT NULL,
                message TEXT NOT NULL,
                job_type TEXT NOT NULL DEFAULT 'cron',
                cron_pattern TEXT,
                interval_secs INTEGER,
                next_delivery_at INTEGER NOT NULL,
                last_delivered_at INTEGER,
                status TEXT NOT NULL DEFAULT 'active',
                created_at INTEGER NOT NULL
            );",
        )
        .unwrap();
        conn
    }

    #[test]
    fn test_add_reminder_and_get_pending() {
        let conn = test_schedule_db();
        let now = crate::helpers::epoch_now();
        let id = add_reminder(&conn, "+user", "check oven", now + 300);
        assert!(id > 0);
        let pending = get_pending_reminders(&conn, "+user");
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].1, "check oven");
    }

    #[test]
    fn test_get_due_reminders_only_past() {
        let conn = test_schedule_db();
        let now = crate::helpers::epoch_now();
        // One due (past), one future
        add_reminder(&conn, "+user", "due now", now - 10);
        add_reminder(&conn, "+user", "later", now + 3600);
        let due = get_due_reminders(&conn);
        assert_eq!(due.len(), 1);
        assert_eq!(due[0].2, "due now");
    }

    #[test]
    fn test_get_due_reminders_excludes_delivered() {
        let conn = test_schedule_db();
        let now = crate::helpers::epoch_now();
        let id = add_reminder(&conn, "+user", "done", now - 10);
        mark_delivered(&conn, id);
        let due = get_due_reminders(&conn);
        assert!(due.is_empty());
    }

    #[test]
    fn test_mark_delivered() {
        let conn = test_schedule_db();
        let now = crate::helpers::epoch_now();
        let id = add_reminder(&conn, "+user", "msg", now - 10);
        mark_delivered(&conn, id);
        let status: String = conn
            .query_row(
                "SELECT status FROM reminders WHERE id = ?1",
                rusqlite::params![id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(status, "delivered");
    }

    #[test]
    fn test_cancel_reminder_removes() {
        let conn = test_schedule_db();
        let now = crate::helpers::epoch_now();
        let id = add_reminder(&conn, "+user", "cancel me", now + 300);
        assert!(cancel_reminder(&conn, id, "+user"));
        let pending = get_pending_reminders(&conn, "+user");
        assert!(pending.is_empty());
    }

    #[test]
    fn test_cancel_reminder_wrong_sender() {
        let conn = test_schedule_db();
        let now = crate::helpers::epoch_now();
        let id = add_reminder(&conn, "+owner", "private", now + 300);
        assert!(!cancel_reminder(&conn, id, "+other"));
        let pending = get_pending_reminders(&conn, "+owner");
        assert_eq!(pending.len(), 1);
    }

    #[test]
    fn test_purge_delivered_cleans_up() {
        let conn = test_schedule_db();
        let now = crate::helpers::epoch_now();
        let id = add_reminder(&conn, "+user", "done", now - 10);
        mark_delivered(&conn, id);
        add_reminder(&conn, "+user", "still pending", now + 300);
        purge_delivered(&conn);
        let total: i64 = conn
            .query_row("SELECT COUNT(*) FROM reminders", [], |row| row.get(0))
            .unwrap();
        assert_eq!(total, 1);
    }

    #[test]
    fn test_get_pending_reminders_for_sender() {
        let conn = test_schedule_db();
        let now = crate::helpers::epoch_now();
        add_reminder(&conn, "+alice", "alice's reminder", now + 300);
        add_reminder(&conn, "+bob", "bob's reminder", now + 600);
        let alice_pending = get_pending_reminders(&conn, "+alice");
        assert_eq!(alice_pending.len(), 1);
        assert_eq!(alice_pending[0].1, "alice's reminder");
    }

    // --- Cron job tests ---

    #[test]
    fn test_add_cron_job_and_get_active() {
        let conn = test_schedule_db();
        let id = add_cron_job(&conn, "+user", "standup", "0 9 * * *");
        assert!(id > 0);
        let jobs = get_active_cron_jobs(&conn, "+user");
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].1, "standup");
        assert_eq!(jobs[0].2, "cron");
        assert_eq!(jobs[0].3.as_deref(), Some("0 9 * * *"));
    }

    #[test]
    fn test_add_cron_job_invalid_pattern_returns_zero() {
        let conn = test_schedule_db();
        let id = add_cron_job(&conn, "+user", "bad", "not a cron");
        assert_eq!(id, 0);
    }

    #[test]
    fn test_add_interval_job_and_get_active() {
        let conn = test_schedule_db();
        let id = add_interval_job(&conn, "+user", "ping", 3600);
        assert!(id > 0);
        let jobs = get_active_cron_jobs(&conn, "+user");
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].1, "ping");
        assert_eq!(jobs[0].2, "interval");
        assert_eq!(jobs[0].4, Some(3600));
    }

    #[test]
    fn test_get_due_cron_jobs_only_active_and_past() {
        let conn = test_schedule_db();
        let now = crate::helpers::epoch_now();
        // Insert a job with next_delivery_at in the past
        conn.execute(
            "INSERT INTO cron_jobs (sender, message, job_type, interval_secs, next_delivery_at, status, created_at) VALUES (?1, ?2, 'interval', 60, ?3, 'active', ?4)",
            rusqlite::params!["+user", "due", now - 10, now],
        ).unwrap();
        // Insert a future job
        conn.execute(
            "INSERT INTO cron_jobs (sender, message, job_type, interval_secs, next_delivery_at, status, created_at) VALUES (?1, ?2, 'interval', 60, ?3, 'active', ?4)",
            rusqlite::params!["+user", "future", now + 3600, now],
        ).unwrap();
        let due = get_due_cron_jobs(&conn);
        assert_eq!(due.len(), 1);
        assert_eq!(due[0].2, "due");
    }

    #[test]
    fn test_get_due_cron_jobs_excludes_paused() {
        let conn = test_schedule_db();
        let now = crate::helpers::epoch_now();
        conn.execute(
            "INSERT INTO cron_jobs (sender, message, job_type, interval_secs, next_delivery_at, status, created_at) VALUES (?1, ?2, 'interval', 60, ?3, 'paused', ?4)",
            rusqlite::params!["+user", "paused job", now - 10, now],
        ).unwrap();
        let due = get_due_cron_jobs(&conn);
        assert!(due.is_empty());
    }

    #[test]
    fn test_get_due_cron_jobs_excludes_cancelled() {
        let conn = test_schedule_db();
        let now = crate::helpers::epoch_now();
        conn.execute(
            "INSERT INTO cron_jobs (sender, message, job_type, interval_secs, next_delivery_at, status, created_at) VALUES (?1, ?2, 'interval', 60, ?3, 'cancelled', ?4)",
            rusqlite::params!["+user", "cancelled job", now - 10, now],
        ).unwrap();
        let due = get_due_cron_jobs(&conn);
        assert!(due.is_empty());
    }

    #[test]
    fn test_advance_cron_job_updates_next_delivery() {
        let conn = test_schedule_db();
        let id = add_cron_job(&conn, "+user", "daily", "0 9 * * *");
        let before: i64 = conn.query_row(
            "SELECT next_delivery_at FROM cron_jobs WHERE id = ?1",
            rusqlite::params![id], |row| row.get(0),
        ).unwrap();
        advance_cron_job(&conn, id, Some("0 9 * * *"), None);
        let after: i64 = conn.query_row(
            "SELECT next_delivery_at FROM cron_jobs WHERE id = ?1",
            rusqlite::params![id], |row| row.get(0),
        ).unwrap();
        assert!(after >= before, "next_delivery_at should advance");
        let delivered: Option<i64> = conn.query_row(
            "SELECT last_delivered_at FROM cron_jobs WHERE id = ?1",
            rusqlite::params![id], |row| row.get(0),
        ).unwrap();
        assert!(delivered.is_some(), "last_delivered_at should be set");
    }

    #[test]
    fn test_advance_interval_job_updates_next_delivery() {
        let conn = test_schedule_db();
        let id = add_interval_job(&conn, "+user", "ping", 60);
        let before: i64 = conn.query_row(
            "SELECT next_delivery_at FROM cron_jobs WHERE id = ?1",
            rusqlite::params![id], |row| row.get(0),
        ).unwrap();
        advance_cron_job(&conn, id, None, Some(60));
        let after: i64 = conn.query_row(
            "SELECT next_delivery_at FROM cron_jobs WHERE id = ?1",
            rusqlite::params![id], |row| row.get(0),
        ).unwrap();
        assert!(after >= before, "next_delivery_at should advance for interval job");
    }

    #[test]
    fn test_cancel_cron_job_removes() {
        let conn = test_schedule_db();
        let id = add_interval_job(&conn, "+user", "remove me", 60);
        assert!(cancel_cron_job(&conn, id, "+user"));
        let jobs = get_active_cron_jobs(&conn, "+user");
        assert!(jobs.is_empty());
    }

    #[test]
    fn test_cancel_cron_job_wrong_sender() {
        let conn = test_schedule_db();
        let id = add_interval_job(&conn, "+owner", "private", 60);
        assert!(!cancel_cron_job(&conn, id, "+other"));
        let jobs = get_active_cron_jobs(&conn, "+owner");
        assert_eq!(jobs.len(), 1);
    }

    #[test]
    fn test_pause_cron_job() {
        let conn = test_schedule_db();
        let id = add_interval_job(&conn, "+user", "pause me", 60);
        assert!(pause_cron_job(&conn, id, "+user"));
        let jobs = get_active_cron_jobs(&conn, "+user");
        assert!(jobs.is_empty(), "paused job should not appear in active list");
    }

    #[test]
    fn test_resume_cron_job_recomputes_next() {
        let conn = test_schedule_db();
        let id = add_interval_job(&conn, "+user", "resume me", 3600);
        assert!(pause_cron_job(&conn, id, "+user"));
        assert!(resume_cron_job(&conn, id, "+user"));
        let jobs = get_active_cron_jobs(&conn, "+user");
        assert_eq!(jobs.len(), 1);
        let now = crate::helpers::epoch_now();
        assert!(jobs[0].5 > now, "next_delivery_at should be in the future after resume");
    }

    #[test]
    fn test_get_active_cron_jobs_for_sender() {
        let conn = test_schedule_db();
        add_interval_job(&conn, "+alice", "alice job", 60);
        add_interval_job(&conn, "+bob", "bob job", 120);
        let alice_jobs = get_active_cron_jobs(&conn, "+alice");
        assert_eq!(alice_jobs.len(), 1);
        assert_eq!(alice_jobs[0].1, "alice job");
    }
}
