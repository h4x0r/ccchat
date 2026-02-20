use dashmap::DashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};
use tokio::sync::Mutex;

use crate::error::AppError;
use crate::helpers::{hash_message, split_message};
use crate::signal::AttachmentInfo;
use crate::traits::{ClaudeRunner, SignalApi};

pub(crate) struct PendingSender {
    pub(crate) name: String,
    pub(crate) short_id: u64,
}

pub(crate) struct SenderState {
    pub(crate) session_id: String,
    pub(crate) model: String,
    pub(crate) lock: Arc<Mutex<()>>,
    pub(crate) last_activity: Instant,
}

pub(crate) struct TokenBucket {
    pub(crate) tokens: f64,
    pub(crate) last_refill: Instant,
    pub(crate) capacity: f64,
    pub(crate) rate_per_sec: f64,
}

impl TokenBucket {
    pub(crate) fn new(capacity: f64, rate_per_sec: f64) -> Self {
        Self {
            tokens: capacity,
            last_refill: Instant::now(),
            capacity,
            rate_per_sec,
        }
    }

    pub(crate) fn try_consume(&mut self) -> bool {
        let now = Instant::now();
        let elapsed = now.duration_since(self.last_refill).as_secs_f64();
        self.tokens = (self.tokens + elapsed * self.rate_per_sec).min(self.capacity);
        self.last_refill = now;
        if self.tokens >= 1.0 {
            self.tokens -= 1.0;
            true
        } else {
            false
        }
    }
}

/// Immutable configuration set at startup from CLI args.
pub(crate) struct Config {
    pub(crate) model: String,
    pub(crate) max_budget: f64,
    pub(crate) rate_limit_config: Option<(f64, f64)>,
    pub(crate) session_ttl: Option<Duration>,
    pub(crate) debounce_ms: u64,
    pub(crate) account: String,
    pub(crate) api_url: String,
    pub(crate) config_path: Option<String>,
    pub(crate) system_prompt: Option<String>,
}

/// Runtime metrics (atomic counters).
pub(crate) struct Metrics {
    pub(crate) start_time: Instant,
    pub(crate) message_count: AtomicU64,
    pub(crate) total_cost: AtomicU64, // stored as microdollars
    pub(crate) error_count: AtomicU64,
    pub(crate) latency_sum_ms: AtomicU64,
    pub(crate) latency_count: AtomicU64,
}

/// Per-sender session tracking.
pub(crate) struct SessionManager {
    pub(crate) sessions: DashMap<String, SenderState>,
    pub(crate) truncated_sessions: DashMap<String, String>,
}

/// Debounce state for merging burst messages.
pub(crate) struct DebounceState {
    pub(crate) buffers: DashMap<String, (Vec<String>, Instant)>,
    pub(crate) active: DashMap<String, ()>,
}

pub(crate) struct State {
    pub(crate) config: Config,
    pub(crate) metrics: Metrics,
    pub(crate) session_mgr: SessionManager,
    pub(crate) debounce: DebounceState,
    pub(crate) allowed_ids: DashMap<String, ()>,
    pub(crate) pending_senders: DashMap<String, PendingSender>,
    pub(crate) pending_counter: AtomicU64,
    pub(crate) sent_hashes: Arc<DashMap<u64, ()>>,
    pub(crate) rate_limits: DashMap<String, TokenBucket>,
    pub(crate) sender_costs: DashMap<String, AtomicU64>,
    pub(crate) sender_prompts: DashMap<String, String>,
    pub(crate) runtime_system_prompt: RwLock<Option<String>>,
    pub(crate) signal_api: Box<dyn SignalApi>,
    pub(crate) claude_runner: Box<dyn ClaudeRunner>,
}

impl State {
    pub(crate) fn is_allowed(&self, sender: &str) -> bool {
        !sender.is_empty() && self.allowed_ids.contains_key(sender)
    }

    pub(crate) fn add_cost(&self, cost: f64) {
        let micros = (cost * 1_000_000.0) as u64;
        self.metrics.total_cost.fetch_add(micros, Ordering::Relaxed);
    }

    pub(crate) fn total_cost_usd(&self) -> f64 {
        self.metrics.total_cost.load(Ordering::Relaxed) as f64 / 1_000_000.0
    }

    pub(crate) fn add_sender_cost(&self, sender: &str, cost: f64) {
        let micros = (cost * 1_000_000.0) as u64;
        self.sender_costs
            .entry(sender.to_string())
            .or_insert_with(|| AtomicU64::new(0))
            .fetch_add(micros, Ordering::Relaxed);
    }

    pub(crate) fn sender_cost_usd(&self, sender: &str) -> f64 {
        self.sender_costs
            .get(sender)
            .map(|v| v.load(Ordering::Relaxed) as f64 / 1_000_000.0)
            .unwrap_or(0.0)
    }

    pub(crate) fn record_latency(&self, duration_ms: u64) {
        self.metrics.latency_sum_ms.fetch_add(duration_ms, Ordering::Relaxed);
        self.metrics.latency_count.fetch_add(1, Ordering::Relaxed);
    }

    /// Get the system prompt for a sender. Priority: per-sender > runtime global > config global > default.
    /// Always appends the NO_MEMORY_PROMPT safety directive.
    pub(crate) fn get_system_prompt(&self, sender: &str) -> String {
        let base = if let Some(per_sender) = self.sender_prompts.get(sender) {
            per_sender.clone()
        } else if let Some(runtime) = self.runtime_system_prompt.read().ok().and_then(|g| g.clone()) {
            runtime
        } else if let Some(ref global) = self.config.system_prompt {
            global.clone()
        } else {
            return crate::NO_MEMORY_PROMPT.to_string();
        };
        format!("{base}\n\n{}", crate::NO_MEMORY_PROMPT)
    }

    pub(crate) fn avg_latency_ms(&self) -> f64 {
        let count = self.metrics.latency_count.load(Ordering::Relaxed);
        if count == 0 {
            return 0.0;
        }
        self.metrics.latency_sum_ms.load(Ordering::Relaxed) as f64 / count as f64
    }

    pub(crate) async fn send_message(&self, recipient: &str, message: &str) -> Result<(), AppError> {
        self.sent_hashes.insert(hash_message(message), ());
        self.signal_api.send_msg(recipient, message).await
    }

    pub(crate) async fn send_long_message(&self, recipient: &str, message: &str) -> Result<(), AppError> {
        let parts = split_message(message, crate::constants::MAX_SIGNAL_MSG_LEN);
        for (i, part) in parts.iter().enumerate() {
            if i > 0 {
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
            self.send_message(recipient, part).await?;
        }
        Ok(())
    }

    pub(crate) async fn set_typing(&self, recipient: &str, typing: bool) -> Result<(), AppError> {
        self.signal_api.set_typing(recipient, typing).await
    }

    pub(crate) async fn download_attachment(&self, attachment: &AttachmentInfo) -> Result<PathBuf, AppError> {
        self.signal_api.download_attachment(attachment).await
    }

    /// Summarize and save all active sessions on shutdown.
    pub(crate) async fn shutdown_save_sessions(&self) {
        let entries: Vec<(String, String, String)> = self
            .session_mgr
            .sessions
            .iter()
            .map(|e| (e.key().clone(), e.session_id.clone(), e.model.clone()))
            .collect();

        for (sender, session_id, model) in &entries {
            match self
                .claude_runner
                .summarize_session(session_id, model)
                .await
            {
                Some(summary) => {
                    crate::memory::save_memory(sender, &summary);
                    tracing::info!(sender = %sender, "Saved memory on shutdown");
                }
                None => {
                    tracing::warn!(sender = %sender, "No summary produced on shutdown");
                }
            }
        }

        self.session_mgr.sessions.clear();
    }

    /// Get or create a session for a sender. Returns (session_id, model, lock, is_new).
    pub(crate) fn get_or_create_session(&self, sender: &str) -> (String, String, Arc<Mutex<()>>, bool) {
        let is_new = !self.session_mgr.sessions.contains_key(sender);
        let default_model = self.config.model.clone();
        let mut entry = self
            .session_mgr
            .sessions
            .entry(sender.to_string())
            .or_insert_with(|| {
                let session_id = uuid::Uuid::new_v4().to_string();
                tracing::info!(sender = %sender, session_id = %session_id, "New session created");
                // Check for persisted model preference
                let model = crate::memory::open_memory_db(sender)
                    .ok()
                    .and_then(|conn| crate::memory::load_model_preference(&conn))
                    .unwrap_or(default_model);
                SenderState {
                    session_id,
                    model,
                    lock: Arc::new(Mutex::new(())),
                    last_activity: Instant::now(),
                }
            });
        entry.last_activity = Instant::now();
        (
            entry.session_id.clone(),
            entry.model.clone(),
            entry.lock.clone(),
            is_new,
        )
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::traits::{MockClaudeRunner, MockSignalApi};

    pub(crate) fn test_state_with(signal: MockSignalApi, claude: MockClaudeRunner) -> State {
        State {
            config: Config {
                model: "sonnet".to_string(),
                max_budget: 5.0,
                rate_limit_config: None,
                session_ttl: None,
                debounce_ms: 0,
                account: "+1234567890".to_string(),
                api_url: "http://127.0.0.1:9999".to_string(),
                config_path: None,
                system_prompt: None,
            },
            metrics: Metrics {
                start_time: Instant::now(),
                message_count: AtomicU64::new(0),
                total_cost: AtomicU64::new(0),
                error_count: AtomicU64::new(0),
                latency_sum_ms: AtomicU64::new(0),
                latency_count: AtomicU64::new(0),
            },
            session_mgr: SessionManager {
                sessions: DashMap::new(),
                truncated_sessions: DashMap::new(),
            },
            debounce: DebounceState {
                buffers: DashMap::new(),
                active: DashMap::new(),
            },
            allowed_ids: {
                let m = DashMap::new();
                m.insert("+1234567890".to_string(), ());
                m.insert("+allowed_user".to_string(), ());
                m
            },
            pending_senders: DashMap::new(),
            pending_counter: AtomicU64::new(0),
            sent_hashes: Arc::new(DashMap::new()),
            rate_limits: DashMap::new(),
            sender_costs: DashMap::new(),
            sender_prompts: DashMap::new(),
            runtime_system_prompt: RwLock::new(None),
            signal_api: Box::new(signal),
            claude_runner: Box::new(claude),
        }
    }

    #[test]
    fn test_token_bucket_basic() {
        let mut bucket = TokenBucket::new(3.0, 1.0);
        assert!(bucket.try_consume());
        assert!(bucket.try_consume());
        assert!(bucket.try_consume());
        assert!(!bucket.try_consume());
    }

    #[test]
    fn test_token_bucket_refill() {
        let mut bucket = TokenBucket::new(2.0, 1000.0);
        assert!(bucket.try_consume());
        assert!(bucket.try_consume());
        assert!(!bucket.try_consume());
        std::thread::sleep(std::time::Duration::from_millis(10));
        assert!(bucket.try_consume());
    }

    #[test]
    fn test_rate_limit_blocks() {
        let mut bucket = TokenBucket::new(1.0, 0.0);
        assert!(bucket.try_consume());
        assert!(!bucket.try_consume());
        assert!(!bucket.try_consume());
        assert!(!bucket.try_consume());
    }

    #[test]
    fn test_session_expiry_logic() {
        let ttl = Duration::from_millis(50);
        let before = Instant::now();
        std::thread::sleep(Duration::from_millis(60));
        let elapsed = before.elapsed();
        assert!(elapsed > ttl, "elapsed time should exceed TTL");
    }

    #[test]
    fn test_state_is_allowed_true() {
        let state = test_state_with(MockSignalApi::new(), MockClaudeRunner::new());
        assert!(state.is_allowed("+allowed_user"));
    }

    #[test]
    fn test_state_is_allowed_false() {
        let state = test_state_with(MockSignalApi::new(), MockClaudeRunner::new());
        assert!(!state.is_allowed("+unknown_user"));
    }

    #[test]
    fn test_state_is_allowed_empty() {
        let state = test_state_with(MockSignalApi::new(), MockClaudeRunner::new());
        assert!(!state.is_allowed(""));
    }

    #[test]
    fn test_get_or_create_session_new() {
        let state = test_state_with(MockSignalApi::new(), MockClaudeRunner::new());
        let (session_id, model, _lock, is_new) = state.get_or_create_session("+fresh_user");
        assert!(is_new);
        assert!(!session_id.is_empty());
        assert_eq!(model, "sonnet");
    }

    #[test]
    fn test_get_or_create_session_existing_returns_same() {
        let state = test_state_with(MockSignalApi::new(), MockClaudeRunner::new());
        let (id1, _, _, new1) = state.get_or_create_session("+repeat_user");
        assert!(new1);
        let (id2, _, _, new2) = state.get_or_create_session("+repeat_user");
        assert!(!new2);
        assert_eq!(id1, id2, "same sender should get same session");
    }

    #[test]
    fn test_get_or_create_session_uses_persisted_model() {
        let sender = format!("+sessmodel_{}", std::process::id());
        // Pre-persist a model preference
        let conn = crate::memory::open_memory_db(&sender).unwrap();
        crate::memory::save_model_preference(&conn, "opus");
        drop(conn);

        let state = test_state_with(MockSignalApi::new(), MockClaudeRunner::new());
        let (_, model, _, _) = state.get_or_create_session(&sender);
        assert_eq!(model, "opus", "should use persisted model preference");
        crate::memory::delete_memory(&sender);
    }

    #[test]
    fn test_add_cost_and_total_cost_usd() {
        let state = test_state_with(MockSignalApi::new(), MockClaudeRunner::new());
        assert!((state.total_cost_usd() - 0.0).abs() < f64::EPSILON);
        state.add_cost(0.5);
        assert!((state.total_cost_usd() - 0.5).abs() < 0.001);
        state.add_cost(1.25);
        assert!((state.total_cost_usd() - 1.75).abs() < 0.001);
    }

    #[test]
    fn test_add_cost_fractional() {
        let state = test_state_with(MockSignalApi::new(), MockClaudeRunner::new());
        state.add_cost(0.000001);
        assert!(state.total_cost_usd() > 0.0);
        assert!(state.total_cost_usd() < 0.001);
    }

    // --- system prompt tests ---

    #[test]
    fn test_get_system_prompt_default() {
        let state = test_state_with(MockSignalApi::new(), MockClaudeRunner::new());
        let prompt = state.get_system_prompt("+user");
        assert_eq!(prompt, crate::NO_MEMORY_PROMPT);
    }

    #[test]
    fn test_get_system_prompt_custom_global() {
        let mut state = test_state_with(MockSignalApi::new(), MockClaudeRunner::new());
        state.config.system_prompt = Some("You are a helpful math tutor.".to_string());
        let prompt = state.get_system_prompt("+user");
        assert!(prompt.contains("You are a helpful math tutor."));
        assert!(prompt.contains(crate::NO_MEMORY_PROMPT));
    }

    #[test]
    fn test_get_system_prompt_per_sender_override() {
        let mut state = test_state_with(MockSignalApi::new(), MockClaudeRunner::new());
        state.config.system_prompt = Some("Global prompt.".to_string());
        state.sender_prompts.insert("+special".to_string(), "VIP prompt.".to_string());
        // Per-sender should override global
        let prompt = state.get_system_prompt("+special");
        assert!(prompt.contains("VIP prompt."));
        assert!(!prompt.contains("Global prompt."));
        assert!(prompt.contains(crate::NO_MEMORY_PROMPT));
        // Other senders still use global
        let prompt2 = state.get_system_prompt("+normal");
        assert!(prompt2.contains("Global prompt."));
    }

    #[test]
    fn test_metrics_error_count_initial_zero() {
        let state = test_state_with(MockSignalApi::new(), MockClaudeRunner::new());
        assert_eq!(state.metrics.error_count.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn test_metrics_error_count_increment() {
        let state = test_state_with(MockSignalApi::new(), MockClaudeRunner::new());
        state.metrics.error_count.fetch_add(1, Ordering::Relaxed);
        state.metrics.error_count.fetch_add(1, Ordering::Relaxed);
        assert_eq!(state.metrics.error_count.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn test_per_sender_cost_tracking() {
        let state = test_state_with(MockSignalApi::new(), MockClaudeRunner::new());
        state.add_sender_cost("+user_a", 0.05);
        state.add_sender_cost("+user_b", 0.10);
        state.add_sender_cost("+user_a", 0.03);
        assert!((state.sender_cost_usd("+user_a") - 0.08).abs() < 0.001);
        assert!((state.sender_cost_usd("+user_b") - 0.10).abs() < 0.001);
        assert!((state.sender_cost_usd("+unknown") - 0.0).abs() < f64::EPSILON);
    }

    #[tokio::test]
    async fn test_state_send_long_message_splits() {
        let mut signal = MockSignalApi::new();
        let call_count = Arc::new(AtomicU64::new(0));
        let count_clone = Arc::clone(&call_count);
        signal.expect_send_msg().returning(move |_, _| {
            count_clone.fetch_add(1, Ordering::Relaxed);
            Ok(())
        });

        let state = test_state_with(signal, MockClaudeRunner::new());
        let long_msg = "a".repeat(6000);
        let result = state.send_long_message("+allowed_user", &long_msg).await;
        assert!(result.is_ok());
        assert_eq!(call_count.load(Ordering::Relaxed), 2);
    }

    // --- shutdown tests ---

    #[tokio::test]
    async fn test_shutdown_save_sessions_calls_summarize_for_each() {
        let mut claude = MockClaudeRunner::new();
        let summarize_count = Arc::new(AtomicU64::new(0));
        let count_clone = Arc::clone(&summarize_count);
        claude.expect_summarize_session().returning(move |_, _| {
            count_clone.fetch_add(1, Ordering::Relaxed);
            Some("Summary".to_string())
        });

        let state = test_state_with(MockSignalApi::new(), claude);
        // Create 2 sessions
        state.session_mgr.sessions.insert(
            "+user1".to_string(),
            SenderState {
                session_id: "s1".to_string(),
                model: "sonnet".to_string(),
                lock: Arc::new(Mutex::new(())),
                last_activity: Instant::now(),
            },
        );
        state.session_mgr.sessions.insert(
            "+user2".to_string(),
            SenderState {
                session_id: "s2".to_string(),
                model: "sonnet".to_string(),
                lock: Arc::new(Mutex::new(())),
                last_activity: Instant::now(),
            },
        );

        state.shutdown_save_sessions().await;
        assert_eq!(summarize_count.load(Ordering::Relaxed), 2);
        assert!(state.session_mgr.sessions.is_empty());
    }

    #[tokio::test]
    async fn test_shutdown_save_sessions_empty_is_noop() {
        let mut claude = MockClaudeRunner::new();
        claude.expect_summarize_session().never();

        let state = test_state_with(MockSignalApi::new(), claude);
        state.shutdown_save_sessions().await;
        assert!(state.session_mgr.sessions.is_empty());
    }

    #[tokio::test]
    async fn test_shutdown_save_sessions_handles_summarize_failure() {
        let mut claude = MockClaudeRunner::new();
        claude
            .expect_summarize_session()
            .returning(|_, _| None); // summarize fails

        let state = test_state_with(MockSignalApi::new(), claude);
        state.session_mgr.sessions.insert(
            "+user1".to_string(),
            SenderState {
                session_id: "s1".to_string(),
                model: "sonnet".to_string(),
                lock: Arc::new(Mutex::new(())),
                last_activity: Instant::now(),
            },
        );

        state.shutdown_save_sessions().await;
        // Sessions still cleaned up even when summarize returns None
        assert!(state.session_mgr.sessions.is_empty());
    }
}
