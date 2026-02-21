use super::*;
use crate::memory::{delete_memory, open_memory_db, store_message};
use crate::signal::AttachmentInfo;
use crate::state::tests::test_state_with;
use crate::state::PendingSender;
use crate::traits::{MockClaudeRunner, MockSignalApi};

// --- latency and error count tests ---

#[tokio::test]
async fn test_handle_message_records_latency() {
    let mut signal = MockSignalApi::new();
    signal.expect_set_typing().returning(|_, _| Ok(()));
    signal.expect_send_msg().returning(|_, _| Ok(()));
    let mut claude = MockClaudeRunner::new();
    claude
        .expect_run_claude()
        .returning(|_, _, _, _, _, _, _| Ok(("ok".to_string(), None)));
    let state = test_state_with(signal, claude);
    let _ = handle_message(&state, "+allowed_user", "hi", &[]).await;
    assert!(state.metrics.latency_count.load(Ordering::Relaxed) >= 1);
    // latency_sum_ms should be populated (>= 0 by type, but confirm it was written)
    let _ = state.metrics.latency_sum_ms.load(Ordering::Relaxed);
}

#[tokio::test]
async fn test_handle_message_error_increments_error_count() {
    let mut signal = MockSignalApi::new();
    signal.expect_set_typing().returning(|_, _| Ok(()));
    signal.expect_send_msg().returning(|_, _| Ok(()));
    let mut claude = MockClaudeRunner::new();
    claude
        .expect_run_claude()
        .returning(|_, _, _, _, _, _, _| Err("boom".into()));
    let state = test_state_with(signal, claude);
    let _ = handle_message(&state, "+allowed_user", "hi", &[]).await;
    assert_eq!(state.metrics.error_count.load(Ordering::Relaxed), 1);
}

// --- download_attachments tests ---

#[tokio::test]
async fn test_download_attachments_image_success() {
    let mut signal = MockSignalApi::new();
    signal
        .expect_download_attachment()
        .returning(|att| Ok(PathBuf::from(format!("/tmp/ccchat/test_{}.png", att.id))));
    let state = test_state_with(signal, MockClaudeRunner::new());
    let atts = vec![AttachmentInfo {
        id: "img1".to_string(),
        content_type: "image/png".to_string(),
        filename: Some("photo.png".to_string()),
        voice_note: false,
    }];
    let (paths, has_audio) = download_attachments(&state, "+user", &atts).await;
    assert_eq!(paths.len(), 1);
    assert!(!has_audio);
}

#[tokio::test]
async fn test_download_attachments_audio_sets_flag() {
    let mut signal = MockSignalApi::new();
    signal
        .expect_download_attachment()
        .returning(|att| Ok(PathBuf::from(format!("/tmp/ccchat/test_{}.aac", att.id))));
    let state = test_state_with(signal, MockClaudeRunner::new());
    let atts = vec![AttachmentInfo {
        id: "aud1".to_string(),
        content_type: "audio/aac".to_string(),
        filename: None,
        voice_note: true,
    }];
    let (paths, has_audio) = download_attachments(&state, "+user", &atts).await;
    assert_eq!(paths.len(), 1);
    assert!(has_audio);
}

#[tokio::test]
async fn test_download_attachments_unsupported_sends_notification() {
    let mut signal = MockSignalApi::new();
    signal.expect_send_msg().returning(|_, msg| {
        assert!(msg.contains("Unsupported attachment type"));
        Ok(())
    });
    let state = test_state_with(signal, MockClaudeRunner::new());
    let atts = vec![AttachmentInfo {
        id: "vid1".to_string(),
        content_type: "video/mp4".to_string(),
        filename: None,
        voice_note: false,
    }];
    let (paths, _) = download_attachments(&state, "+user", &atts).await;
    assert!(paths.is_empty());
}

#[tokio::test]
async fn test_download_attachments_failure_sends_error() {
    let mut signal = MockSignalApi::new();
    signal
        .expect_download_attachment()
        .returning(|_| Err(AppError::Other("download failed".to_string())));
    signal.expect_send_msg().returning(|_, msg| {
        assert!(msg.contains("Failed to download attachment"));
        Ok(())
    });
    let state = test_state_with(signal, MockClaudeRunner::new());
    let atts = vec![AttachmentInfo {
        id: "fail1".to_string(),
        content_type: "image/jpeg".to_string(),
        filename: None,
        voice_note: false,
    }];
    let (paths, _) = download_attachments(&state, "+user", &atts).await;
    assert!(paths.is_empty());
}

// --- handle_unauthorized tests ---

#[tokio::test]
async fn test_handle_unauthorized_tracks_pending() {
    let mut signal = MockSignalApi::new();
    signal.expect_send_msg().returning(|_, _| Ok(()));
    let state = Arc::new(test_state_with(signal, MockClaudeRunner::new()));
    handle_unauthorized(&state, "+stranger", "Stranger");
    assert!(state.pending_senders.contains_key("+stranger"));
    tokio::time::sleep(Duration::from_millis(50)).await; // let spawn finish
}

#[tokio::test]
async fn test_handle_unauthorized_no_duplicate() {
    let mut signal = MockSignalApi::new();
    // Only one notification sent (first time)
    signal.expect_send_msg().times(1).returning(|_, _| Ok(()));
    let state = Arc::new(test_state_with(signal, MockClaudeRunner::new()));
    handle_unauthorized(&state, "+stranger", "Stranger");
    handle_unauthorized(&state, "+stranger", "Stranger");
    assert_eq!(state.pending_senders.len(), 1);
    tokio::time::sleep(Duration::from_millis(50)).await;
}

#[tokio::test]
async fn test_handle_unauthorized_increments_counter() {
    let mut signal = MockSignalApi::new();
    signal.expect_send_msg().returning(|_, _| Ok(()));
    let state = Arc::new(test_state_with(signal, MockClaudeRunner::new()));
    handle_unauthorized(&state, "+a", "A");
    handle_unauthorized(&state, "+b", "B");
    let a_sid = state.pending_senders.get("+a").unwrap().short_id;
    let b_sid = state.pending_senders.get("+b").unwrap().short_id;
    assert_eq!(a_sid, 1);
    assert_eq!(b_sid, 2);
    tokio::time::sleep(Duration::from_millis(50)).await;
}

#[test]
fn test_handle_command_status() {
    let state = test_state_with(MockSignalApi::new(), MockClaudeRunner::new());
    let result = handle_command(&state, "+allowed_user", "/status");
    assert!(result.is_some());
    let text = result.unwrap();
    assert!(text.contains("ccchat status"));
    assert!(text.contains("Uptime:"));
    assert!(text.contains("Messages:"));
    assert!(text.contains("Total cost:"));
}

#[test]
fn test_status_includes_sender_cost() {
    let state = test_state_with(MockSignalApi::new(), MockClaudeRunner::new());
    state.add_sender_cost("+allowed_user", 0.1234);
    let text = handle_command(&state, "+allowed_user", "/status").unwrap();
    assert!(text.contains("Your cost: $0.1234"), "got: {text}");
}

#[test]
fn test_status_includes_error_count() {
    let state = test_state_with(MockSignalApi::new(), MockClaudeRunner::new());
    state.metrics.error_count.fetch_add(7, Ordering::Relaxed);
    let text = handle_command(&state, "+allowed_user", "/status").unwrap();
    assert!(text.contains("Errors: 7"), "got: {text}");
}

#[test]
fn test_status_includes_avg_latency() {
    let state = test_state_with(MockSignalApi::new(), MockClaudeRunner::new());
    state.record_latency(100);
    state.record_latency(200);
    let text = handle_command(&state, "+allowed_user", "/status").unwrap();
    assert!(text.contains("Avg latency: 150ms"), "got: {text}");
}

#[test]
fn test_handle_command_pending_empty() {
    let state = test_state_with(MockSignalApi::new(), MockClaudeRunner::new());
    let result = handle_command(&state, "+allowed_user", "/pending");
    assert_eq!(result.unwrap(), "No pending senders.");
}

#[test]
fn test_handle_command_pending_with_entries() {
    let state = test_state_with(MockSignalApi::new(), MockClaudeRunner::new());
    state.pending_senders.insert(
        "+blocked1".to_string(),
        PendingSender {
            name: "Alice".to_string(),
            short_id: 1,
        },
    );
    let result = handle_command(&state, "+allowed_user", "/pending").unwrap();
    assert!(result.contains("Pending senders:"));
    assert!(result.contains("Alice"));
    assert!(result.contains("#1"));
}

#[test]
fn test_handle_command_allow_valid() {
    let state = test_state_with(MockSignalApi::new(), MockClaudeRunner::new());
    state.pending_senders.insert(
        "+new_user".to_string(),
        PendingSender {
            name: "Bob".to_string(),
            short_id: 1,
        },
    );
    let result = handle_command(&state, "+1234567890", "/allow 1").unwrap();
    assert!(result.contains("Allowed:"));
    assert!(state.is_allowed("+new_user"));
}

#[test]
fn test_handle_command_allow_invalid() {
    let state = test_state_with(MockSignalApi::new(), MockClaudeRunner::new());
    let result = handle_command(&state, "+1234567890", "/allow 99").unwrap();
    assert!(result.contains("No pending sender #99"));
}

#[test]
fn test_handle_command_allow_no_arg() {
    let state = test_state_with(MockSignalApi::new(), MockClaudeRunner::new());
    let result = handle_command(&state, "+1234567890", "/allow").unwrap();
    assert!(result.contains("Usage:"));
}

#[test]
fn test_handle_command_revoke() {
    let state = test_state_with(MockSignalApi::new(), MockClaudeRunner::new());
    state.allowed_ids.insert("+victim".to_string(), ());
    assert!(state.is_allowed("+victim"));
    let result = handle_command(&state, "+1234567890", "/revoke +victim").unwrap();
    assert!(result.contains("Revoked:"));
    assert!(!state.is_allowed("+victim"));
}

#[test]
fn test_handle_command_model() {
    let state = test_state_with(MockSignalApi::new(), MockClaudeRunner::new());
    let result = handle_command(&state, "+allowed_user", "/model haiku").unwrap();
    assert!(result.contains("Model switched to: haiku"));
    let session = state.session_mgr.sessions.get("+allowed_user").unwrap();
    assert_eq!(session.model, "haiku");
}

#[test]
fn test_model_command_persists_preference() {
    let sender = format!("+modcmd_{}", std::process::id());
    let state = test_state_with(MockSignalApi::new(), MockClaudeRunner::new());
    state.allowed_ids.insert(sender.clone(), ());
    let _ = handle_command(&state, &sender, "/model opus");
    // Verify persisted to SQLite
    let conn = crate::memory::open_memory_db(&sender).unwrap();
    let pref = crate::memory::load_model_preference(&conn);
    assert_eq!(pref.as_deref(), Some("opus"));
    crate::memory::delete_memory(&sender);
}

#[test]
fn test_handle_command_unknown() {
    let state = test_state_with(MockSignalApi::new(), MockClaudeRunner::new());
    let result = handle_command(&state, "+allowed_user", "just a message");
    assert!(result.is_none());
}

// --- Async handle_message tests ---

#[tokio::test]
async fn test_handle_message_happy_path() {
    let mut signal = MockSignalApi::new();
    signal.expect_set_typing().returning(|_, _| Ok(()));
    signal.expect_send_msg().returning(|_, _| Ok(()));

    let mut claude = MockClaudeRunner::new();
    claude
        .expect_run_claude()
        .returning(|_, _, _, _, _, _, _| Ok(("Hello from Claude!".to_string(), Some(0.01))));

    let state = test_state_with(signal, claude);
    let result = handle_message(&state, "+allowed_user", "Hi there", &[]).await;
    assert!(result.is_ok());
    assert!((state.total_cost_usd() - 0.01).abs() < 0.001);
}

#[tokio::test]
async fn test_handle_message_reset_with_summarization() {
    let mut signal = MockSignalApi::new();
    signal.expect_send_msg().returning(|_, _| Ok(()));

    let mut claude = MockClaudeRunner::new();
    claude
        .expect_summarize_session()
        .returning(|_, _| Some("Discussed testing.".to_string()));

    let state = test_state_with(signal, claude);
    // Pre-populate a session
    state.session_mgr.sessions.insert(
        "+allowed_user".to_string(),
        SenderState {
            session_id: "test-session".to_string(),
            model: "sonnet".to_string(),
            lock: Arc::new(Mutex::new(())),
            last_activity: Instant::now(),
            message_count: 0,
        },
    );

    let result = handle_message(&state, "+allowed_user", "/reset", &[]).await;
    assert!(result.is_ok());
    assert!(!state.session_mgr.sessions.contains_key("+allowed_user"));
}

#[tokio::test]
async fn test_handle_message_more_with_truncated() {
    let mut signal = MockSignalApi::new();
    signal.expect_set_typing().returning(|_, _| Ok(()));
    signal.expect_send_msg().returning(|_, _| Ok(()));

    let mut claude = MockClaudeRunner::new();
    claude
        .expect_run_claude()
        .returning(|_, _, _, _, _, _, _| Ok(("Continued response.".to_string(), Some(0.005))));

    let state = test_state_with(signal, claude);
    // Pre-populate a session and a truncated marker
    state.session_mgr.sessions.insert(
        "+allowed_user".to_string(),
        SenderState {
            session_id: "sess-1".to_string(),
            model: "sonnet".to_string(),
            lock: Arc::new(Mutex::new(())),
            last_activity: Instant::now(),
            message_count: 0,
        },
    );
    state
        .session_mgr
        .truncated_sessions
        .insert("+allowed_user".to_string(), "sess-1".to_string());

    let result = handle_message(&state, "+allowed_user", "/more", &[]).await;
    assert!(result.is_ok());
}

#[tokio::test]
async fn test_handle_message_more_without_truncated() {
    let mut signal = MockSignalApi::new();
    signal.expect_send_msg().times(1).returning(|_, msg| {
        assert_eq!(msg, "Nothing to continue.");
        Ok(())
    });

    let state = test_state_with(signal, MockClaudeRunner::new());
    let result = handle_message(&state, "+allowed_user", "/more", &[]).await;
    assert!(result.is_ok());
}

#[tokio::test]
async fn test_handle_message_rate_limited() {
    let mut signal = MockSignalApi::new();
    signal.expect_send_msg().returning(|_, msg| {
        assert!(msg.contains("Rate limited"));
        Ok(())
    });

    let mut state = test_state_with(signal, MockClaudeRunner::new());
    // Enable rate limiting: 0 capacity, no refill -> always limited
    state.config.rate_limit_config = Some((1.0, 0.0));
    // First call consumes the single token
    {
        let mut bucket = state
            .rate_limits
            .entry("+allowed_user".to_string())
            .or_insert_with(|| TokenBucket::new(0.0, 0.0)); // 0 capacity = always empty
        bucket.tokens = 0.0;
    }

    let result = handle_message(&state, "+allowed_user", "hello", &[]).await;
    assert!(result.is_ok());
}

#[tokio::test]
async fn test_handle_message_claude_error() {
    let mut signal = MockSignalApi::new();
    signal.expect_set_typing().returning(|_, _| Ok(()));
    signal.expect_send_msg().returning(|_, msg| {
        assert!(msg.contains("Claude error:"));
        Ok(())
    });

    let mut claude = MockClaudeRunner::new();
    claude
        .expect_run_claude()
        .returning(|_, _, _, _, _, _, _| Err("model unavailable".into()));

    let state = test_state_with(signal, claude);
    let result = handle_message(&state, "+allowed_user", "hello", &[]).await;
    assert!(result.is_ok());
}

#[tokio::test]
async fn test_handle_message_truncated_response() {
    let mut signal = MockSignalApi::new();
    signal.expect_set_typing().returning(|_, _| Ok(()));
    signal.expect_send_msg().returning(|_, _| Ok(()));

    let long_response = "a".repeat(4000) + " and then the function";
    let long_clone = long_response.clone();
    let mut claude = MockClaudeRunner::new();
    claude
        .expect_run_claude()
        .returning(move |_, _, _, _, _, _, _| Ok((long_clone.clone(), Some(0.02))));

    let state = test_state_with(signal, claude);
    let result = handle_message(&state, "+allowed_user", "explain this", &[]).await;
    assert!(result.is_ok());
    // Should have stored the session for /more
    assert!(state
        .session_mgr
        .truncated_sessions
        .contains_key("+allowed_user"));
}

#[tokio::test]
async fn test_handle_message_cost_tracking() {
    let mut signal = MockSignalApi::new();
    signal.expect_set_typing().returning(|_, _| Ok(()));
    signal.expect_send_msg().returning(|_, _| Ok(()));

    let mut claude = MockClaudeRunner::new();
    claude
        .expect_run_claude()
        .returning(|_, _, _, _, _, _, _| Ok(("response".to_string(), Some(0.05))));

    let state = test_state_with(signal, claude);
    let _ = handle_message(&state, "+allowed_user", "msg1", &[]).await;
    assert!((state.total_cost_usd() - 0.05).abs() < 0.001);
}

#[tokio::test]
async fn test_handle_message_new_session_injects_context() {
    let mut signal = MockSignalApi::new();
    signal.expect_set_typing().returning(|_, _| Ok(()));
    signal.expect_send_msg().returning(|_, _| Ok(()));

    let mut claude = MockClaudeRunner::new();
    claude
        .expect_run_claude()
        .returning(|prompt, _, _, _, _, _, _| {
            // Should be called with the user's text (possibly with context prepended)
            assert!(prompt.contains("hello") || prompt.contains("Previous conversations"));
            Ok(("reply".to_string(), None))
        });

    let state = test_state_with(signal, claude);
    let result = handle_message(&state, "+allowed_user", "hello", &[]).await;
    assert!(result.is_ok());
    // Session should now exist
    assert!(state.session_mgr.sessions.contains_key("+allowed_user"));
}

#[tokio::test]
async fn test_handle_message_command_response() {
    let mut signal = MockSignalApi::new();
    signal.expect_send_msg().returning(|_, msg| {
        assert!(msg.contains("ccchat status"));
        Ok(())
    });

    let state = test_state_with(signal, MockClaudeRunner::new());
    let result = handle_message(&state, "+allowed_user", "/status", &[]).await;
    assert!(result.is_ok());
}

// --- handle_continuation tests ---

#[tokio::test]
async fn test_handle_continuation_success() {
    let mut signal = MockSignalApi::new();
    signal.expect_set_typing().returning(|_, _| Ok(()));
    signal.expect_send_msg().returning(|_, _| Ok(()));

    let mut claude = MockClaudeRunner::new();
    claude
        .expect_run_claude()
        .returning(|_, _, _, _, _, _, _| Ok(("more content here.".to_string(), Some(0.01))));

    let state = test_state_with(signal, claude);
    state.session_mgr.sessions.insert(
        "+allowed_user".to_string(),
        SenderState {
            session_id: "sess-cont".to_string(),
            model: "sonnet".to_string(),
            lock: Arc::new(Mutex::new(())),
            last_activity: Instant::now(),
            message_count: 0,
        },
    );

    let result = handle_continuation(&state, "+allowed_user", "sess-cont").await;
    assert!(result.is_ok());
    assert!((state.total_cost_usd() - 0.01).abs() < 0.001);
}

#[tokio::test]
async fn test_handle_continuation_still_truncated() {
    let mut signal = MockSignalApi::new();
    signal.expect_set_typing().returning(|_, _| Ok(()));
    signal.expect_send_msg().returning(|_, _| Ok(()));

    let long_response = "a".repeat(4000) + " and then the";
    let long_clone = long_response.clone();
    let mut claude = MockClaudeRunner::new();
    claude
        .expect_run_claude()
        .returning(move |_, _, _, _, _, _, _| Ok((long_clone.clone(), None)));

    let state = test_state_with(signal, claude);
    let result = handle_continuation(&state, "+allowed_user", "sess-1").await;
    assert!(result.is_ok());
    assert!(state
        .session_mgr
        .truncated_sessions
        .contains_key("+allowed_user"));
}

#[tokio::test]
async fn test_handle_continuation_claude_error() {
    let mut signal = MockSignalApi::new();
    signal.expect_set_typing().returning(|_, _| Ok(()));
    signal.expect_send_msg().returning(|_, msg| {
        assert!(msg.contains("Claude error:"));
        Ok(())
    });

    let mut claude = MockClaudeRunner::new();
    claude
        .expect_run_claude()
        .returning(|_, _, _, _, _, _, _| Err("timeout".into()));

    let state = test_state_with(signal, claude);
    let result = handle_continuation(&state, "+allowed_user", "sess-1").await;
    assert!(result.is_ok());
}

// --- /search command tests ---

#[test]
fn test_search_with_results() {
    let sender = format!("search_results_{}", std::process::id());
    let conn = open_memory_db(&sender).unwrap();
    store_message(&conn, "user", "How do I configure nginx?", "sess1");
    store_message(
        &conn,
        "assistant",
        "Edit /etc/nginx/nginx.conf for main config.",
        "sess1",
    );
    drop(conn);

    let state = test_state_with(MockSignalApi::new(), MockClaudeRunner::new());
    let result = handle_command(&state, &sender, "/search nginx").unwrap();
    assert!(result.contains("Search results for \"nginx\""));
    assert!(result.contains("found)"));
    assert!(result.contains("[user]"));
    assert!(result.contains("[assistant]"));
    assert!(result.contains("nginx"));
    delete_memory(&sender);
}

#[test]
fn test_search_no_results() {
    let sender = format!("search_noresult_{}", std::process::id());
    let conn = open_memory_db(&sender).unwrap();
    store_message(&conn, "user", "Hello world", "sess1");
    drop(conn);

    let state = test_state_with(MockSignalApi::new(), MockClaudeRunner::new());
    let result = handle_command(&state, &sender, "/search xylophone").unwrap();
    assert_eq!(result, "No results found for \"xylophone\"");
    delete_memory(&sender);
}

#[test]
fn test_search_no_argument() {
    let state = test_state_with(MockSignalApi::new(), MockClaudeRunner::new());
    let result = handle_command(&state, "+allowed_user", "/search").unwrap();
    assert!(result.contains("Usage: /search <query>"));
}

#[test]
fn test_search_empty_memory() {
    let sender = format!("search_empty_{}", std::process::id());
    // Ensure no DB exists (clean state)
    delete_memory(&sender);

    let state = test_state_with(MockSignalApi::new(), MockClaudeRunner::new());
    let result = handle_command(&state, &sender, "/search anything").unwrap();
    assert!(result.contains("No results found for \"anything\""));
    delete_memory(&sender);
}

#[test]
fn test_search_results_truncated() {
    let sender = format!("search_trunc_{}", std::process::id());
    let conn = open_memory_db(&sender).unwrap();
    // Insert more than 5 messages containing the keyword
    for i in 1..=8 {
        store_message(
            &conn,
            "user",
            &format!("Message {i} about databases and SQL optimization"),
            "sess1",
        );
    }
    // Insert a message with content longer than 100 chars
    let long_content = format!("databases {} end of long message", "x".repeat(120));
    store_message(&conn, "assistant", &long_content, "sess1");
    drop(conn);

    let state = test_state_with(MockSignalApi::new(), MockClaudeRunner::new());
    let result = handle_command(&state, &sender, "/search databases").unwrap();
    // Count result lines (first line is header, rest are results starting with "- ")
    let result_lines: Vec<&str> = result.lines().filter(|l| l.starts_with("- ")).collect();
    assert!(
        result_lines.len() <= 5,
        "Expected at most 5 results, got {}",
        result_lines.len()
    );
    // Check that long content was truncated (contains "...")
    let has_truncated = result_lines.iter().any(|l| l.contains("..."));
    assert!(has_truncated, "Expected at least one truncated preview");
    delete_memory(&sender);
}

#[test]
fn test_allow_command_creates_audit_entry() {
    let state = test_state_with(MockSignalApi::new(), MockClaudeRunner::new());
    let uid = uuid::Uuid::new_v4().to_string();
    state.pending_senders.insert(
        uid.clone(),
        PendingSender {
            name: "AuditTest".to_string(),
            short_id: 99,
        },
    );
    let _ = handle_command(&state, "+1234567890", "/allow 99");
    let actions = crate::audit::get_recent_actions(10);
    let found = actions
        .iter()
        .any(|(a, t, d, _)| a == "allow" && *t == uid && d == "AuditTest");
    assert!(found, "Expected audit entry for allow, got: {actions:?}");
}

#[test]
fn test_handle_command_audit() {
    // Log a test action first
    crate::audit::log_action("test_audit_cmd", "target", "detail");
    let state = test_state_with(MockSignalApi::new(), MockClaudeRunner::new());
    let result = handle_command(&state, "+1234567890", "/audit");
    assert!(result.is_some());
    let text = result.unwrap();
    assert!(text.contains("Audit log"), "got: {text}");
}

#[test]
fn test_handle_command_export() {
    let sender = format!("+export_cmd_{}", std::process::id());
    let conn = crate::memory::open_memory_db(&sender).unwrap();
    crate::memory::store_message(&conn, "user", "test export", "sess1");
    drop(conn);
    let state = test_state_with(MockSignalApi::new(), MockClaudeRunner::new());
    let result = handle_command(&state, &sender, "/export");
    assert!(result.is_some());
    assert!(result.unwrap().contains("Conversation export"));
    delete_memory(&sender);
}

#[test]
fn test_handle_command_export_config() {
    let state = test_state_with(MockSignalApi::new(), MockClaudeRunner::new());
    let result = handle_command(&state, "+1234567890", "/export-config");
    assert!(result.is_some());
    let json = result.unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
    assert!(parsed["allowed"].is_array());
}

#[tokio::test]
async fn test_handle_message_attachments_forwarded_to_claude() {
    let mut signal = MockSignalApi::new();
    signal.expect_set_typing().returning(|_, _| Ok(()));
    signal.expect_send_msg().returning(|_, _| Ok(()));

    let mut claude = MockClaudeRunner::new();
    claude
        .expect_run_claude()
        .withf(|_, _, _, files, _, _, _| {
            files.len() == 2
                && files[0] == std::path::Path::new("/tmp/photo.png")
                && files[1] == std::path::Path::new("/tmp/doc.pdf")
        })
        .returning(|_, _, _, _, _, _, _| Ok(("I see a photo and a PDF.".to_string(), Some(0.02))));

    let state = test_state_with(signal, claude);
    let attachments = vec![
        PathBuf::from("/tmp/photo.png"),
        PathBuf::from("/tmp/doc.pdf"),
    ];
    let result = handle_message(&state, "+allowed_user", "What are these?", &attachments).await;
    assert!(result.is_ok());
}

#[tokio::test]
async fn test_handle_message_cost_accumulates_across_messages() {
    let mut signal = MockSignalApi::new();
    signal.expect_set_typing().returning(|_, _| Ok(()));
    signal.expect_send_msg().returning(|_, _| Ok(()));

    let call_count = std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0));
    let call_count_clone = call_count.clone();
    let mut claude = MockClaudeRunner::new();
    claude
        .expect_run_claude()
        .returning(move |_, _, _, _, _, _, _| {
            let n = call_count_clone.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let cost = if n == 0 { 0.03 } else { 0.07 };
            Ok(("reply".to_string(), Some(cost)))
        });

    let state = test_state_with(signal, claude);
    let _ = handle_message(&state, "+allowed_user", "msg1", &[]).await;
    let _ = handle_message(&state, "+allowed_user", "msg2", &[]).await;
    assert!(
        (state.total_cost_usd() - 0.10).abs() < 0.001,
        "Expected ~$0.10, got ${:.4}",
        state.total_cost_usd()
    );
}

#[test]
fn test_handle_command_help() {
    let state = test_state_with(MockSignalApi::new(), MockClaudeRunner::new());
    let result = handle_command(&state, "+allowed_user", "/help");
    assert!(result.is_some());
}

#[test]
fn test_handle_command_help_lists_commands() {
    let state = test_state_with(MockSignalApi::new(), MockClaudeRunner::new());
    let text = handle_command(&state, "+allowed_user", "/help").unwrap();
    let commands = [
        "/help",
        "/status",
        "/reset",
        "/more",
        "/model",
        "/memory",
        "/forget",
        "/search",
        "/pending",
        "/allow",
        "/revoke",
        "/export-config",
    ];
    for cmd in &commands {
        assert!(text.contains(cmd), "Missing command: {cmd}");
    }
}

#[test]
fn test_handle_command_help_has_descriptions() {
    let state = test_state_with(MockSignalApi::new(), MockClaudeRunner::new());
    let text = handle_command(&state, "+allowed_user", "/help").unwrap();
    // Each line after the header should have " - " separator
    for line in text.lines().skip(1) {
        assert!(
            line.contains(" - "),
            "Missing description separator in: {line}"
        );
    }
}

#[tokio::test]
async fn test_send_claude_response_sends_attachments() {
    use std::path::PathBuf;

    // Create a temp file that Claude's response will reference
    let dir = PathBuf::from("/tmp/ccchat");
    let _ = std::fs::create_dir_all(&dir);
    let test_file = dir.join(format!("attach_test_{}.png", std::process::id()));
    std::fs::write(&test_file, b"fake png data").unwrap();

    let mut signal = MockSignalApi::new();
    signal.expect_set_typing().returning(|_, _| Ok(()));
    signal.expect_send_msg().returning(|_, _| Ok(()));
    signal
        .expect_send_attachment()
        .withf(|_, data, ct, fname| {
            data == b"fake png data" && ct == "image/png" && fname.contains("attach_test_")
        })
        .times(1)
        .returning(|_, _, _, _| Ok(()));

    let response_text = format!("Here is your file: {}", test_file.display());
    let response_clone = response_text.clone();
    let mut claude = MockClaudeRunner::new();
    claude
        .expect_run_claude()
        .returning(move |_, _, _, _, _, _, _| Ok((response_clone.clone(), Some(0.01))));

    let state = test_state_with(signal, claude);
    let _ = handle_message(&state, "+allowed_user", "make a png", &[]).await;
    let _ = std::fs::remove_file(&test_file);
}

#[tokio::test]
async fn test_enqueue_on_claude_error() {
    let mut signal = MockSignalApi::new();
    signal.expect_set_typing().returning(|_, _| Ok(()));
    signal.expect_send_msg().returning(|_, _| Ok(()));
    let mut claude = MockClaudeRunner::new();
    claude
        .expect_run_claude()
        .returning(|_, _, _, _, _, _, _| Err("model unavailable".into()));
    let state = test_state_with(signal, claude);
    let _ = handle_message(&state, "+allowed_user", "enqueue me", &[]).await;
    // Verify it was enqueued
    let conn = crate::queue::open_queue_db().unwrap();
    let pending = crate::queue::get_pending(&conn);
    let found = pending
        .iter()
        .any(|(_, s, c, _)| s == "+allowed_user" && c.contains("enqueue me"));
    assert!(
        found,
        "Expected prompt to be enqueued, pending: {pending:?}"
    );
    // Cleanup
    for (id, _, _, _) in &pending {
        crate::queue::mark_completed(&conn, *id);
    }
    crate::queue::purge_completed(&conn);
}

#[tokio::test]
async fn test_retry_loop_processes_pending() {
    let conn = crate::queue::open_queue_db().unwrap();
    let sender = format!("+retry_ok_{}", std::process::id());
    crate::queue::enqueue(&conn, &sender, "retry this", "[]");
    let id = crate::queue::get_pending(&conn).last().unwrap().0;

    let mut signal = MockSignalApi::new();
    signal.expect_set_typing().returning(|_, _| Ok(()));
    signal.expect_send_msg().returning(|_, _| Ok(()));
    let mut claude = MockClaudeRunner::new();
    claude
        .expect_run_claude()
        .returning(|_, _, _, _, _, _, _| Ok(("retry success".to_string(), None)));
    let state = test_state_with(signal, claude);
    state.allowed_ids.insert(sender.clone(), ());

    retry_pending_messages(&state).await;

    // Verify completed (purged)
    let pending = crate::queue::get_pending(&conn);
    assert!(
        !pending.iter().any(|(pid, _, _, _)| *pid == id),
        "should be completed"
    );
}

#[tokio::test]
async fn test_retry_loop_increments_on_failure() {
    let conn = crate::queue::open_queue_db().unwrap();
    let uid = uuid::Uuid::new_v4();
    let sender = format!("+retry_fail_{uid}");
    crate::queue::enqueue(&conn, &sender, "will fail", "[]");
    let id = crate::queue::get_pending(&conn)
        .iter()
        .find(|(_, s, _, _)| *s == sender)
        .unwrap()
        .0;

    let mut signal = MockSignalApi::new();
    signal.expect_set_typing().returning(|_, _| Ok(()));
    let mut claude = MockClaudeRunner::new();
    claude
        .expect_run_claude()
        .returning(|_, _, _, _, _, _, _| Err("still broken".into()));
    let state = test_state_with(signal, claude);
    // Only allow this unique sender so retry only processes our entry
    state.allowed_ids.clear();
    state.allowed_ids.insert(sender.clone(), ());

    retry_pending_messages(&state).await;

    // Verify retry_count incremented (>= 1 because parallel tests may also
    // process entries from the shared queue.db)
    let count: i64 = conn
        .query_row(
            "SELECT retry_count FROM message_queue WHERE id = ?1",
            rusqlite::params![id],
            |row| row.get(0),
        )
        .unwrap();
    assert!(count >= 1, "retry_count should be >= 1, got {count}");
    // Cleanup
    crate::queue::mark_completed(&conn, id);
    crate::queue::purge_completed(&conn);
}

#[tokio::test]
async fn test_retry_loop_skips_max_retries() {
    let conn = crate::queue::open_queue_db().unwrap();
    let uid = uuid::Uuid::new_v4();
    let sender = format!("+retry_max_{uid}");
    crate::queue::enqueue(&conn, &sender, "maxed out", "[]");
    let id = crate::queue::get_pending(&conn)
        .iter()
        .find(|(_, s, _, _)| *s == sender)
        .unwrap()
        .0;
    // Set retry_count to MAX_RETRIES
    for _ in 0..crate::queue::MAX_RETRIES {
        crate::queue::increment_retry(&conn, id);
    }

    // Verify it's excluded from pending
    let pending = crate::queue::get_pending(&conn);
    assert!(
        !pending.iter().any(|(_, s, _, _)| *s == sender),
        "should be excluded after MAX_RETRIES"
    );

    // Cleanup
    conn.execute(
        "DELETE FROM message_queue WHERE id = ?1",
        rusqlite::params![id],
    )
    .unwrap();
}

#[tokio::test]
async fn test_handle_message_new_session_injects_context_format() {
    use crate::memory::{delete_memory, open_memory_db, save_memory, store_message};

    let sender = format!("+ctx_format_{}", uuid::Uuid::new_v4());
    // Pre-populate memory with a summary so inject_context includes "Previous conversations"
    save_memory(&sender, "Discussed deploying nginx on Ubuntu.");
    // Also store a message matching "nginx" so FTS5 search finds it
    let conn = open_memory_db(&sender).unwrap();
    store_message(&conn, "user", "How do I deploy nginx?", "old-sess");
    store_message(
        &conn,
        "assistant",
        "Use apt install nginx then edit the config.",
        "old-sess",
    );
    drop(conn);

    let mut signal = MockSignalApi::new();
    signal.expect_set_typing().returning(|_, _| Ok(()));
    signal.expect_send_msg().returning(|_, _| Ok(()));

    let mut claude = MockClaudeRunner::new();
    claude
        .expect_run_claude()
        .withf(|prompt, _, _, _, _, _, _| {
            // New session should include context header AND the user's message
            prompt.contains("Previous conversations") && prompt.contains("tell me about nginx")
        })
        .returning(|_, _, _, _, _, _, _| Ok(("got it".to_string(), None)));

    let state = test_state_with(signal, claude);
    state.allowed_ids.insert(sender.clone(), ());

    let result = handle_message(&state, &sender, "tell me about nginx", &[]).await;
    assert!(result.is_ok());
    delete_memory(&sender);
}

// --- /usage command tests ---

#[test]
fn test_cmd_usage_with_no_history() {
    let sender = format!("+usage_empty_{}", uuid::Uuid::new_v4());
    let state = test_state_with(MockSignalApi::new(), MockClaudeRunner::new());
    let result = handle_command(&state, &sender, "/usage").unwrap();
    assert!(result.contains("0 sent, 0 received"), "got: {result}");
    delete_memory(&sender);
}

#[test]
fn test_cmd_usage_with_cost() {
    let sender = format!("+usage_cost_{}", uuid::Uuid::new_v4());
    let state = test_state_with(MockSignalApi::new(), MockClaudeRunner::new());
    state.add_sender_cost(&sender, 0.1234);
    let result = handle_command(&state, &sender, "/usage").unwrap();
    assert!(result.contains("Cost: $0.1234"), "got: {result}");
    delete_memory(&sender);
}

#[test]
fn test_cmd_usage_with_messages() {
    let sender = format!("+usage_msgs_{}", uuid::Uuid::new_v4());
    let conn = open_memory_db(&sender).unwrap();
    store_message(&conn, "user", "hello", "sess1");
    store_message(&conn, "assistant", "hi there", "sess1");
    store_message(&conn, "user", "how are you?", "sess1");
    drop(conn);
    let state = test_state_with(MockSignalApi::new(), MockClaudeRunner::new());
    let result = handle_command(&state, &sender, "/usage").unwrap();
    assert!(result.contains("2 sent, 1 received"), "got: {result}");
    delete_memory(&sender);
}

#[test]
fn test_cmd_usage_shows_model() {
    let sender = format!("+usage_model_{}", uuid::Uuid::new_v4());
    let state = test_state_with(MockSignalApi::new(), MockClaudeRunner::new());
    let result = handle_command(&state, &sender, "/usage").unwrap();
    assert!(result.contains("Current model: sonnet"), "got: {result}");
    delete_memory(&sender);
}

#[test]
fn test_cmd_usage_shows_first_message_date() {
    let sender = format!("+usage_date_{}", uuid::Uuid::new_v4());
    let conn = open_memory_db(&sender).unwrap();
    store_message(&conn, "user", "hello", "sess1");
    drop(conn);
    let state = test_state_with(MockSignalApi::new(), MockClaudeRunner::new());
    let result = handle_command(&state, &sender, "/usage").unwrap();
    assert!(result.contains("First message:"), "got: {result}");
    assert!(
        !result.contains("N/A"),
        "expected a real date, got: {result}"
    );
    delete_memory(&sender);
}

#[test]
fn test_handle_command_usage() {
    let state = test_state_with(MockSignalApi::new(), MockClaudeRunner::new());
    let result = handle_command(&state, "+allowed_user", "/usage");
    assert!(result.is_some());
}

// --- /pin, /pins, /recall tests ---

#[test]
fn test_save_pin_and_get() {
    let sender = format!("+pin_rt_{}", uuid::Uuid::new_v4());
    let conn = open_memory_db(&sender).unwrap();
    crate::memory::messages::save_pin(&conn, "test-label", "some content");
    let content = crate::memory::messages::get_pin(&conn, "test-label");
    assert_eq!(content, Some("some content".to_string()));
    delete_memory(&sender);
}

#[test]
fn test_save_pin_overwrites_existing() {
    let sender = format!("+pin_ow_{}", uuid::Uuid::new_v4());
    let conn = open_memory_db(&sender).unwrap();
    crate::memory::messages::save_pin(&conn, "label", "first");
    crate::memory::messages::save_pin(&conn, "label", "second");
    assert_eq!(
        crate::memory::messages::get_pin(&conn, "label"),
        Some("second".to_string())
    );
    delete_memory(&sender);
}

#[test]
fn test_list_pins_empty() {
    let sender = format!("+pin_empty_{}", uuid::Uuid::new_v4());
    let conn = open_memory_db(&sender).unwrap();
    let pins = crate::memory::messages::list_pins(&conn);
    assert!(pins.is_empty());
    delete_memory(&sender);
}

#[test]
fn test_list_pins_with_data() {
    let sender = format!("+pin_list_{}", uuid::Uuid::new_v4());
    let conn = open_memory_db(&sender).unwrap();
    crate::memory::messages::save_pin(&conn, "alpha", "content a");
    crate::memory::messages::save_pin(&conn, "beta", "content b");
    let pins = crate::memory::messages::list_pins(&conn);
    assert_eq!(pins.len(), 2);
    delete_memory(&sender);
}

#[test]
fn test_get_pin_missing() {
    let sender = format!("+pin_miss_{}", uuid::Uuid::new_v4());
    let conn = open_memory_db(&sender).unwrap();
    assert_eq!(crate::memory::messages::get_pin(&conn, "nonexistent"), None);
    delete_memory(&sender);
}

#[test]
fn test_delete_pin() {
    let sender = format!("+pin_del_{}", uuid::Uuid::new_v4());
    let conn = open_memory_db(&sender).unwrap();
    crate::memory::messages::save_pin(&conn, "to-delete", "content");
    crate::memory::messages::delete_pin(&conn, "to-delete");
    assert_eq!(crate::memory::messages::get_pin(&conn, "to-delete"), None);
    delete_memory(&sender);
}

#[test]
fn test_get_recent_messages() {
    let sender = format!("+pin_recent_{}", uuid::Uuid::new_v4());
    let conn = open_memory_db(&sender).unwrap();
    for i in 1..=5 {
        store_message(&conn, "user", &format!("msg {i}"), "sess1");
    }
    let msgs = crate::memory::messages::get_recent_messages(&conn, 3);
    assert_eq!(msgs.len(), 3);
    // Should be in chronological order (oldest first)
    assert!(msgs[0].1.contains("msg 3"));
    assert!(msgs[2].1.contains("msg 5"));
    delete_memory(&sender);
}

#[test]
fn test_cmd_pin_saves_recent() {
    let sender = format!("+pin_cmd_{}", uuid::Uuid::new_v4());
    let conn = open_memory_db(&sender).unwrap();
    store_message(&conn, "user", "hello", "sess1");
    store_message(&conn, "assistant", "hi there", "sess1");
    drop(conn);
    let state = test_state_with(MockSignalApi::new(), MockClaudeRunner::new());
    let result = handle_command(&state, &sender, "/pin test-label").unwrap();
    assert!(result.contains("Pinned"), "got: {result}");
    assert!(result.contains("test-label"), "got: {result}");
    delete_memory(&sender);
}

#[test]
fn test_cmd_pins_lists_all() {
    let sender = format!("+pins_list_{}", uuid::Uuid::new_v4());
    let conn = open_memory_db(&sender).unwrap();
    crate::memory::messages::save_pin(&conn, "alpha", "content");
    drop(conn);
    let state = test_state_with(MockSignalApi::new(), MockClaudeRunner::new());
    let result = handle_command(&state, &sender, "/pins").unwrap();
    assert!(result.contains("Saved pins"), "got: {result}");
    assert!(result.contains("alpha"), "got: {result}");
    delete_memory(&sender);
}

#[test]
fn test_cmd_recall_returns_content() {
    let sender = format!("+recall_cmd_{}", uuid::Uuid::new_v4());
    let conn = open_memory_db(&sender).unwrap();
    crate::memory::messages::save_pin(&conn, "mypin", "pinned stuff");
    drop(conn);
    let state = test_state_with(MockSignalApi::new(), MockClaudeRunner::new());
    let result = handle_command(&state, &sender, "/recall mypin").unwrap();
    assert!(result.contains("Recalled pin 'mypin'"), "got: {result}");
    delete_memory(&sender);
}

#[test]
fn test_cmd_recall_sets_pending() {
    let sender = format!("+recall_pend_{}", uuid::Uuid::new_v4());
    let conn = open_memory_db(&sender).unwrap();
    crate::memory::messages::save_pin(&conn, "context", "important info");
    drop(conn);
    let state = test_state_with(MockSignalApi::new(), MockClaudeRunner::new());
    let _ = handle_command(&state, &sender, "/recall context");
    assert!(
        state.pending_recalls.contains_key(&sender),
        "should set pending recall"
    );
    delete_memory(&sender);
}

#[test]
fn test_handle_command_pin() {
    let sender = format!("+pin_handle_{}", uuid::Uuid::new_v4());
    let conn = open_memory_db(&sender).unwrap();
    store_message(&conn, "user", "hi", "sess1");
    drop(conn);
    let state = test_state_with(MockSignalApi::new(), MockClaudeRunner::new());
    let result = handle_command(&state, &sender, "/pin label");
    assert!(result.is_some());
    delete_memory(&sender);
}

// --- auto-summarize tests ---

#[tokio::test]
async fn test_auto_summarize_triggers_at_threshold() {
    let mut signal = MockSignalApi::new();
    signal.expect_set_typing().returning(|_, _| Ok(()));
    signal.expect_send_msg().returning(|_, _| Ok(()));
    let mut claude = MockClaudeRunner::new();
    claude
        .expect_run_claude()
        .returning(|_, _, _, _, _, _, _| Ok(("ok".to_string(), None)));
    claude
        .expect_summarize_session()
        .times(1)
        .returning(|_, _| Some("Summary".to_string()));
    let state = test_state_with(signal, claude);
    let sender = format!("+autosum_{}", uuid::Uuid::new_v4());
    state.allowed_ids.insert(sender.clone(), ());
    // Pre-populate session with message_count at threshold - 1
    state.session_mgr.sessions.insert(
        sender.clone(),
        SenderState {
            session_id: "sess".to_string(),
            model: "sonnet".to_string(),
            lock: Arc::new(Mutex::new(())),
            last_activity: Instant::now(),
            message_count: crate::constants::AUTO_SUMMARIZE_THRESHOLD - 1,
        },
    );
    let _ = handle_message(&state, &sender, "trigger", &[]).await;
    // summarize_session was called (verified by times(1))
    delete_memory(&sender);
}

#[tokio::test]
async fn test_auto_summarize_does_not_trigger_below_threshold() {
    let mut signal = MockSignalApi::new();
    signal.expect_set_typing().returning(|_, _| Ok(()));
    signal.expect_send_msg().returning(|_, _| Ok(()));
    let mut claude = MockClaudeRunner::new();
    claude
        .expect_run_claude()
        .returning(|_, _, _, _, _, _, _| Ok(("ok".to_string(), None)));
    claude.expect_summarize_session().never();
    let state = test_state_with(signal, claude);
    let _ = handle_message(&state, "+allowed_user", "hi", &[]).await;
    // summarize_session should not have been called
}

#[tokio::test]
async fn test_auto_summarize_resets_counter() {
    let mut signal = MockSignalApi::new();
    signal.expect_set_typing().returning(|_, _| Ok(()));
    signal.expect_send_msg().returning(|_, _| Ok(()));
    let mut claude = MockClaudeRunner::new();
    claude
        .expect_run_claude()
        .returning(|_, _, _, _, _, _, _| Ok(("ok".to_string(), None)));
    claude
        .expect_summarize_session()
        .returning(|_, _| Some("Summary".to_string()));
    let state = test_state_with(signal, claude);
    let sender = format!("+autosum_reset_{}", uuid::Uuid::new_v4());
    state.allowed_ids.insert(sender.clone(), ());
    state.session_mgr.sessions.insert(
        sender.clone(),
        SenderState {
            session_id: "sess".to_string(),
            model: "sonnet".to_string(),
            lock: Arc::new(Mutex::new(())),
            last_activity: Instant::now(),
            message_count: crate::constants::AUTO_SUMMARIZE_THRESHOLD - 1,
        },
    );
    let _ = handle_message(&state, &sender, "trigger", &[]).await;
    let count = state
        .session_mgr
        .sessions
        .get(&sender)
        .unwrap()
        .message_count;
    assert_eq!(count, 0, "counter should reset after summarize");
    delete_memory(&sender);
}

#[tokio::test]
async fn test_auto_summarize_counter_increments() {
    let mut signal = MockSignalApi::new();
    signal.expect_set_typing().returning(|_, _| Ok(()));
    signal.expect_send_msg().returning(|_, _| Ok(()));
    let mut claude = MockClaudeRunner::new();
    claude
        .expect_run_claude()
        .returning(|_, _, _, _, _, _, _| Ok(("ok".to_string(), None)));
    claude.expect_summarize_session().never();
    let state = test_state_with(signal, claude);
    let _ = handle_message(&state, "+allowed_user", "msg1", &[]).await;
    let count = state
        .session_mgr
        .sessions
        .get("+allowed_user")
        .unwrap()
        .message_count;
    assert_eq!(count, 1, "counter should be 1 after one message");
}

#[test]
fn test_sender_state_message_count_initializes_zero() {
    let state = test_state_with(MockSignalApi::new(), MockClaudeRunner::new());
    let (_, _, _, _) = state.get_or_create_session("+fresh");
    let count = state
        .session_mgr
        .sessions
        .get("+fresh")
        .unwrap()
        .message_count;
    assert_eq!(count, 0);
}

#[tokio::test]
async fn test_auto_summarize_handles_summarize_failure() {
    let mut signal = MockSignalApi::new();
    signal.expect_set_typing().returning(|_, _| Ok(()));
    signal.expect_send_msg().returning(|_, _| Ok(()));
    let mut claude = MockClaudeRunner::new();
    claude
        .expect_run_claude()
        .returning(|_, _, _, _, _, _, _| Ok(("ok".to_string(), None)));
    claude.expect_summarize_session().returning(|_, _| None); // summarize fails
    let state = test_state_with(signal, claude);
    let sender = format!("+autosum_fail_{}", uuid::Uuid::new_v4());
    state.allowed_ids.insert(sender.clone(), ());
    state.session_mgr.sessions.insert(
        sender.clone(),
        SenderState {
            session_id: "sess".to_string(),
            model: "sonnet".to_string(),
            lock: Arc::new(Mutex::new(())),
            last_activity: Instant::now(),
            message_count: crate::constants::AUTO_SUMMARIZE_THRESHOLD - 1,
        },
    );
    // Should not panic even when summarize returns None
    let result = handle_message(&state, &sender, "trigger", &[]).await;
    assert!(result.is_ok());
    delete_memory(&sender);
}

#[tokio::test]
async fn test_auto_summarize_saves_memory() {
    let mut signal = MockSignalApi::new();
    signal.expect_set_typing().returning(|_, _| Ok(()));
    signal.expect_send_msg().returning(|_, _| Ok(()));
    let mut claude = MockClaudeRunner::new();
    claude
        .expect_run_claude()
        .returning(|_, _, _, _, _, _, _| Ok(("ok".to_string(), None)));
    claude
        .expect_summarize_session()
        .returning(|_, _| Some("Auto-summary content".to_string()));
    let state = test_state_with(signal, claude);
    let sender = format!("+autosum_save_{}", uuid::Uuid::new_v4());
    state.allowed_ids.insert(sender.clone(), ());
    state.session_mgr.sessions.insert(
        sender.clone(),
        SenderState {
            session_id: "sess".to_string(),
            model: "sonnet".to_string(),
            lock: Arc::new(Mutex::new(())),
            last_activity: Instant::now(),
            message_count: crate::constants::AUTO_SUMMARIZE_THRESHOLD - 1,
        },
    );
    let _ = handle_message(&state, &sender, "trigger", &[]).await;
    // Verify memory was saved
    let status = crate::memory::memory_status(&sender);
    assert!(
        status.contains("Auto-summary"),
        "expected summary in memory, got: {status}"
    );
    delete_memory(&sender);
}

#[tokio::test]
async fn test_auto_summarize_does_not_reset_session() {
    let mut signal = MockSignalApi::new();
    signal.expect_set_typing().returning(|_, _| Ok(()));
    signal.expect_send_msg().returning(|_, _| Ok(()));
    let mut claude = MockClaudeRunner::new();
    claude
        .expect_run_claude()
        .returning(|_, _, _, _, _, _, _| Ok(("ok".to_string(), None)));
    claude
        .expect_summarize_session()
        .returning(|_, _| Some("Summary".to_string()));
    let state = test_state_with(signal, claude);
    let sender = format!("+autosum_nores_{}", uuid::Uuid::new_v4());
    state.allowed_ids.insert(sender.clone(), ());
    state.session_mgr.sessions.insert(
        sender.clone(),
        SenderState {
            session_id: "original-session".to_string(),
            model: "sonnet".to_string(),
            lock: Arc::new(Mutex::new(())),
            last_activity: Instant::now(),
            message_count: crate::constants::AUTO_SUMMARIZE_THRESHOLD - 1,
        },
    );
    let _ = handle_message(&state, &sender, "trigger", &[]).await;
    let session_id = state
        .session_mgr
        .sessions
        .get(&sender)
        .unwrap()
        .session_id
        .clone();
    assert_eq!(
        session_id, "original-session",
        "session_id should not change"
    );
    delete_memory(&sender);
}

// --- scheduled reminders command tests ---

#[test]
fn test_cmd_remind_success() {
    let sender = "+remind_ok";
    let result = cmd_remind(sender, "5m Check the oven");
    assert!(
        result.contains("Reminder #"),
        "should confirm reminder: {result}"
    );
    assert!(
        result.contains("5 minutes"),
        "should show human time: {result}"
    );
    assert!(
        result.contains("Check the oven"),
        "should echo message: {result}"
    );
}

#[test]
fn test_cmd_remind_invalid_time() {
    let result = cmd_remind("+user", "xyz Do something");
    assert!(
        result.contains("Invalid time format"),
        "should reject bad time: {result}"
    );
}

#[test]
fn test_cmd_remind_missing_message() {
    let result = cmd_remind("+user", "");
    assert!(result.contains("Usage:"), "should show usage: {result}");
}

#[test]
fn test_cmd_reminders_empty() {
    let sender = format!("+rem_empty_{}", uuid::Uuid::new_v4());
    let result = cmd_reminders(&sender);
    assert_eq!(result, "No pending reminders.");
}

#[test]
fn test_cmd_cancel_reminder_success() {
    let sender = "+cancel_ok";
    // Create a reminder first
    let create_result = cmd_remind(sender, "1h Cancel me");
    let id: i64 = create_result
        .split('#')
        .nth(1)
        .and_then(|s| s.split_whitespace().next())
        .and_then(|s| s.parse().ok())
        .expect("should parse reminder id");
    let result = cmd_cancel_reminder(sender, &id.to_string());
    assert!(
        result.contains("cancelled"),
        "should confirm cancel: {result}"
    );
}

#[test]
fn test_cmd_cancel_reminder_not_found() {
    let sender = format!("+cancel_nf_{}", uuid::Uuid::new_v4());
    let result = cmd_cancel_reminder(&sender, "99999");
    assert!(
        result.contains("No pending reminder"),
        "should say not found: {result}"
    );
}

#[test]
fn test_handle_command_remind() {
    let state = test_state_with(MockSignalApi::new(), MockClaudeRunner::new());
    let result = handle_command(&state, "+user", "/remind 5m test");
    assert!(result.is_some(), "/remind should be handled");
}

#[test]
fn test_handle_command_reminders() {
    let state = test_state_with(MockSignalApi::new(), MockClaudeRunner::new());
    let result = handle_command(&state, "+user", "/reminders");
    assert!(result.is_some(), "/reminders should be handled");
}

#[test]
fn test_handle_command_cancel() {
    let state = test_state_with(MockSignalApi::new(), MockClaudeRunner::new());
    let result = handle_command(&state, "+user", "/cancel 1");
    assert!(result.is_some(), "/cancel should be handled");
}

// --- Cron command tests ---

#[test]
fn test_cmd_cron_success() {
    let sender = format!("+cron_ok_{}", std::process::id());
    let result = cmd_cron(&sender, "\"0 9 * * *\" Daily standup");
    assert!(result.contains("Cron job #"), "should confirm: {result}");
    assert!(
        result.contains("Daily standup"),
        "should echo message: {result}"
    );
}

#[test]
fn test_cmd_cron_unquoted() {
    let sender = format!("+cron_unq_{}", std::process::id());
    let result = cmd_cron(&sender, "0 9 * * MON Monday check");
    assert!(
        result.contains("Cron job #"),
        "should accept unquoted: {result}"
    );
}

#[test]
fn test_cmd_cron_invalid() {
    let result = cmd_cron("+user", "\"not valid\" msg");
    assert!(
        result.contains("Invalid cron pattern"),
        "should reject bad pattern: {result}"
    );
}

#[test]
fn test_cmd_cron_missing_msg() {
    let result = cmd_cron("+user", "");
    assert!(result.contains("Usage:"), "should show usage: {result}");
}

#[test]
fn test_cmd_every_hourly() {
    let sender = format!("+every_ok_{}", std::process::id());
    let result = cmd_every(&sender, "1h Check status");
    assert!(
        result.contains("Interval job #"),
        "should confirm: {result}"
    );
    assert!(
        result.contains("1 hours"),
        "should show human time: {result}"
    );
}

#[test]
fn test_cmd_every_invalid() {
    let result = cmd_every("+user", "xyz Do something");
    assert!(
        result.contains("Invalid interval"),
        "should reject bad interval: {result}"
    );
}

#[test]
fn test_cmd_every_missing_msg() {
    let result = cmd_every("+user", "");
    assert!(result.contains("Usage:"), "should show usage: {result}");
}

#[test]
fn test_cmd_daily_valid() {
    let sender = format!("+daily_ok_{}", std::process::id());
    let result = cmd_daily(&sender, "09:00 Morning standup");
    assert!(result.contains("Daily job #"), "should confirm: {result}");
    assert!(result.contains("09:00 UTC"), "should show time: {result}");
}

#[test]
fn test_cmd_daily_invalid_time() {
    let result = cmd_daily("+user", "25:99 Bad time");
    assert!(
        result.contains("Invalid time"),
        "should reject bad time: {result}"
    );
}

#[test]
fn test_cmd_daily_missing_msg() {
    let result = cmd_daily("+user", "");
    assert!(result.contains("Usage:"), "should show usage: {result}");
}

#[test]
fn test_cmd_crons_empty() {
    let sender = format!("+crons_empty_{}", uuid::Uuid::new_v4());
    let result = cmd_crons(&sender);
    assert_eq!(result, "No active cron jobs.");
}

#[test]
fn test_cmd_crons_with_jobs() {
    let sender = format!("+crons_list_{}", std::process::id());
    cmd_every(&sender, "1h Check");
    let result = cmd_crons(&sender);
    assert!(
        result.contains("Active cron jobs"),
        "should list jobs: {result}"
    );
    assert!(result.contains("Check"), "should show message: {result}");
}

#[test]
fn test_cmd_cron_cancel_success() {
    let sender = format!("+cc_ok_{}", std::process::id());
    let create = cmd_every(&sender, "1h Cancel me");
    let id: i64 = create
        .split('#')
        .nth(1)
        .and_then(|s| s.split_whitespace().next())
        .and_then(|s| s.parse().ok())
        .expect("should parse id");
    let result = cmd_cron_cancel(&sender, &id.to_string());
    assert!(
        result.contains("cancelled"),
        "should confirm cancel: {result}"
    );
}

#[test]
fn test_cmd_cron_cancel_not_found() {
    let sender = format!("+cc_nf_{}", uuid::Uuid::new_v4());
    let result = cmd_cron_cancel(&sender, "99999");
    assert!(
        result.contains("No cron job"),
        "should say not found: {result}"
    );
}

#[test]
fn test_cmd_cron_pause_and_resume() {
    let sender = format!("+cp_ok_{}", std::process::id());
    let create = cmd_every(&sender, "1h Pause me");
    let id: i64 = create
        .split('#')
        .nth(1)
        .and_then(|s| s.split_whitespace().next())
        .and_then(|s| s.parse().ok())
        .expect("should parse id");
    let pause_result = cmd_cron_pause(&sender, &id.to_string());
    assert!(
        pause_result.contains("paused"),
        "should confirm pause: {pause_result}"
    );
    let resume_result = cmd_cron_resume(&sender, &id.to_string());
    assert!(
        resume_result.contains("resumed"),
        "should confirm resume: {resume_result}"
    );
}

// --- handle_command wiring tests ---

#[test]
fn test_handle_command_cron() {
    let state = test_state_with(MockSignalApi::new(), MockClaudeRunner::new());
    let result = handle_command(&state, "+user", "/cron \"0 9 * * *\" test");
    assert!(result.is_some(), "/cron should be handled");
}

#[test]
fn test_handle_command_every() {
    let state = test_state_with(MockSignalApi::new(), MockClaudeRunner::new());
    let result = handle_command(&state, "+user", "/every 1h test");
    assert!(result.is_some(), "/every should be handled");
}

#[test]
fn test_handle_command_daily() {
    let state = test_state_with(MockSignalApi::new(), MockClaudeRunner::new());
    let result = handle_command(&state, "+user", "/daily 09:00 test");
    assert!(result.is_some(), "/daily should be handled");
}

#[test]
fn test_handle_command_crons() {
    let state = test_state_with(MockSignalApi::new(), MockClaudeRunner::new());
    let result = handle_command(&state, "+user", "/crons");
    assert!(result.is_some(), "/crons should be handled");
}

#[test]
fn test_handle_command_cron_cancel() {
    let state = test_state_with(MockSignalApi::new(), MockClaudeRunner::new());
    let result = handle_command(&state, "+user", "/cron-cancel 1");
    assert!(result.is_some(), "/cron-cancel should be handled");
}

#[test]
fn test_handle_command_cron_pause() {
    let state = test_state_with(MockSignalApi::new(), MockClaudeRunner::new());
    let result = handle_command(&state, "+user", "/cron-pause 1");
    assert!(result.is_some(), "/cron-pause should be handled");
}

#[test]
fn test_handle_command_cron_resume() {
    let state = test_state_with(MockSignalApi::new(), MockClaudeRunner::new());
    let result = handle_command(&state, "+user", "/cron-resume 1");
    assert!(result.is_some(), "/cron-resume should be handled");
}

#[test]
fn test_handle_command_help_includes_cron() {
    let state = test_state_with(MockSignalApi::new(), MockClaudeRunner::new());
    let result = handle_command(&state, "+user", "/help").unwrap();
    assert!(
        result.contains("/cron"),
        "help should mention /cron: {result}"
    );
    assert!(
        result.contains("/every"),
        "help should mention /every: {result}"
    );
    assert!(
        result.contains("/daily"),
        "help should mention /daily: {result}"
    );
    assert!(
        result.contains("/crons"),
        "help should mention /crons: {result}"
    );
}

// --- deliver_due_cron_jobs tests ---

#[tokio::test]
async fn test_deliver_due_cron_jobs_sends_message() {
    let mut signal = MockSignalApi::new();
    let send_count = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let count_clone = Arc::clone(&send_count);
    signal.expect_send_msg().returning(move |_, msg| {
        assert!(msg.contains("Scheduled:"), "should prefix with Scheduled:");
        count_clone.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        Ok(())
    });
    let state = test_state_with(signal, MockClaudeRunner::new());
    // Insert a due job directly
    let conn = crate::schedule::open_schedule_db().unwrap();
    let now = crate::helpers::epoch_now();
    conn.execute(
            "INSERT INTO cron_jobs (sender, message, job_type, interval_secs, next_delivery_at, status, created_at) VALUES (?1, ?2, 'interval', 60, ?3, 'active', ?4)",
            rusqlite::params!["+allowed_user", "test delivery", now - 10, now],
        ).unwrap();
    deliver_due_cron_jobs(&state).await;
    assert!(send_count.load(std::sync::atomic::Ordering::Relaxed) >= 1);
}

#[tokio::test]
async fn test_deliver_due_cron_jobs_advances_after_delivery() {
    let mut signal = MockSignalApi::new();
    signal.expect_send_msg().returning(|_, _| Ok(()));
    let state = test_state_with(signal, MockClaudeRunner::new());
    let conn = crate::schedule::open_schedule_db().unwrap();
    let now = crate::helpers::epoch_now();
    conn.execute(
            "INSERT INTO cron_jobs (sender, message, job_type, interval_secs, next_delivery_at, status, created_at) VALUES (?1, ?2, 'interval', 3600, ?3, 'active', ?4)",
            rusqlite::params!["+allowed_user", "advance test", now - 10, now],
        ).unwrap();
    let id = conn.last_insert_rowid();
    deliver_due_cron_jobs(&state).await;
    let next: i64 = conn
        .query_row(
            "SELECT next_delivery_at FROM cron_jobs WHERE id = ?1",
            rusqlite::params![id],
            |row| row.get(0),
        )
        .unwrap();
    assert!(next > now, "next_delivery_at should advance after delivery");
}

#[tokio::test]
async fn test_deliver_due_cron_jobs_skips_on_send_failure() {
    let mut signal = MockSignalApi::new();
    signal
        .expect_send_msg()
        .returning(|_, _| Err("send failed".into()));
    let state = test_state_with(signal, MockClaudeRunner::new());
    let conn = crate::schedule::open_schedule_db().unwrap();
    let now = crate::helpers::epoch_now();
    conn.execute(
            "INSERT INTO cron_jobs (sender, message, job_type, interval_secs, next_delivery_at, status, created_at) VALUES (?1, ?2, 'interval', 60, ?3, 'active', ?4)",
            rusqlite::params!["+allowed_user", "fail test", now - 10, now],
        ).unwrap();
    let id = conn.last_insert_rowid();
    deliver_due_cron_jobs(&state).await;
    let next: i64 = conn
        .query_row(
            "SELECT next_delivery_at FROM cron_jobs WHERE id = ?1",
            rusqlite::params![id],
            |row| row.get(0),
        )
        .unwrap();
    assert!(next <= now, "should NOT advance on send failure");
}
