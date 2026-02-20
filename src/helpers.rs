use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::net::TcpListener;
use std::path::PathBuf;
use std::time::Duration;

pub(crate) fn hash_message(text: &str) -> u64 {
    let mut h = DefaultHasher::new();
    text.hash(&mut h);
    h.finish()
}

pub(crate) fn split_message(text: &str, max_len: usize) -> Vec<String> {
    if text.len() <= max_len {
        return vec![text.to_string()];
    }

    let mut parts = Vec::new();
    let mut remaining = text;

    while !remaining.is_empty() {
        if remaining.len() <= max_len {
            parts.push(remaining.to_string());
            break;
        }

        let chunk = &remaining[..max_len];
        let split_at = chunk
            .rfind("\n\n")
            .or_else(|| chunk.rfind('\n'))
            .unwrap_or(max_len);

        let (part, rest) = remaining.split_at(split_at);
        parts.push(part.to_string());
        remaining = rest.trim_start_matches('\n');
    }

    parts
}

pub(crate) fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        format!("{}...", &s[..max])
    }
}

pub(crate) fn parse_rate_limit(s: &str) -> Option<(f64, f64)> {
    let parts: Vec<&str> = s.splitn(2, '/').collect();
    if parts.len() != 2 {
        return None;
    }
    let count: f64 = parts[0].trim().parse().ok()?;
    let period_secs: f64 = match parts[1].trim() {
        "sec" | "second" | "s" => 1.0,
        "min" | "minute" | "m" => 60.0,
        "hour" | "hr" | "h" => 3600.0,
        "day" | "d" => 86400.0,
        _ => return None,
    };
    if count <= 0.0 {
        return None;
    }
    Some((count, count / period_secs))
}

pub(crate) fn parse_duration(s: &str) -> Option<Duration> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    let (num_str, suffix) = if s.ends_with(|c: char| c.is_ascii_alphabetic()) {
        let split = s.len() - 1;
        (&s[..split], &s[split..])
    } else {
        (s, "")
    };
    let num: u64 = num_str.parse().ok()?;
    if num == 0 {
        return None;
    }
    let secs = match suffix {
        "s" => num,
        "m" => num * 60,
        "h" => num * 3600,
        "d" => num * 86400,
        _ => return None,
    };
    Some(Duration::from_secs(secs))
}

pub(crate) fn format_duration_human(secs: u64) -> String {
    if secs < 60 {
        format!("{secs} seconds")
    } else if secs < 3600 {
        format!("{} minutes", secs / 60)
    } else if secs < 86400 {
        format!("{} hours", secs / 3600)
    } else {
        format!("{} days", secs / 86400)
    }
}

pub(crate) fn merge_messages(msgs: &[String]) -> String {
    msgs.join("\n")
}

pub(crate) fn voice_prompt(text: &str) -> String {
    if text.is_empty() || text == "Describe this attachment." {
        "The user sent a voice message. Please transcribe and respond to it.".to_string()
    } else {
        format!(
            "The user sent a voice message along with this text: {text}\n\n\
             Please transcribe the voice message and respond to both."
        )
    }
}

pub(crate) fn looks_truncated(response: &str) -> bool {
    response.len() > crate::constants::TRUNCATION_THRESHOLD
        && !response
            .trim_end()
            .ends_with(['.', '!', '?', '`', '"', ')'])
}

pub(crate) fn epoch_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

pub(crate) fn is_command(text: &str) -> bool {
    text.trim().starts_with('/')
}

pub(crate) fn find_free_port(preferred: u16) -> u16 {
    if preferred == 0 {
        let listener = TcpListener::bind("127.0.0.1:0").expect("failed to bind ephemeral port");
        return listener.local_addr().unwrap().port();
    }
    for port in preferred..=preferred.saturating_add(100) {
        if TcpListener::bind(("127.0.0.1", port)).is_ok() {
            return port;
        }
    }
    let listener = TcpListener::bind("127.0.0.1:0").expect("failed to bind ephemeral port");
    listener.local_addr().unwrap().port()
}

/// Map file extension to MIME content type for attachment sending.
pub(crate) fn content_type_from_extension(path: &std::path::Path) -> &'static str {
    match path.extension().and_then(|e| e.to_str()) {
        Some("png") => "image/png",
        Some("jpg" | "jpeg") => "image/jpeg",
        Some("gif") => "image/gif",
        Some("pdf") => "application/pdf",
        Some("txt" | "md" | "rs" | "py" | "js") => "text/plain",
        Some("json") => "application/json",
        Some("csv") => "text/csv",
        Some("svg") => "image/svg+xml",
        _ => "application/octet-stream",
    }
}

/// Extract file paths from text that point to existing files under /tmp/ccchat/.
pub(crate) fn extract_file_references(text: &str) -> Vec<PathBuf> {
    let prefix = "/tmp/ccchat/";
    let mut paths = Vec::new();
    for word in text.split_whitespace() {
        // Strip common surrounding punctuation
        let cleaned =
            word.trim_matches(|c: char| c == '`' || c == '"' || c == '\'' || c == '(' || c == ')');
        if cleaned.starts_with(prefix) {
            let path = PathBuf::from(cleaned);
            if path.is_file() {
                paths.push(path);
            }
        }
    }
    paths
}

pub(crate) fn parse_interval_secs(s: &str) -> Option<i64> {
    parse_duration(s).map(|d| d.as_secs() as i64)
}

pub(crate) fn parse_daily_time(input: &str) -> Option<String> {
    let parts: Vec<&str> = input.split(':').collect();
    if parts.len() != 2 {
        return None;
    }
    let hour: u8 = parts[0].parse().ok()?;
    let minute: u8 = parts[1].parse().ok()?;
    if hour > 23 || minute > 59 {
        return None;
    }
    Some(format!("{minute} {hour} * * *"))
}

pub(crate) fn format_cron_human(pattern: &str) -> String {
    use std::str::FromStr;
    match croner::Cron::from_str(pattern) {
        Ok(cron) => cron.pattern.to_string(),
        Err(_) => pattern.to_string(),
    }
}

pub(crate) fn isolated_workdir(sender: &str) -> PathBuf {
    std::env::temp_dir()
        .join("ccchat")
        .join(crate::memory::hash_sender(sender))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_hash_message_deterministic() {
        let h1 = hash_message("hello world");
        let h2 = hash_message("hello world");
        assert_eq!(h1, h2);
    }

    #[test]
    fn test_hash_message_different_inputs() {
        let h1 = hash_message("hello");
        let h2 = hash_message("world");
        assert_ne!(h1, h2);
    }

    #[test]
    fn test_split_message_short() {
        let parts = split_message("short msg", 4000);
        assert_eq!(parts.len(), 1);
        assert_eq!(parts[0], "short msg");
    }

    #[test]
    fn test_split_message_exact_max() {
        let msg = "a".repeat(4000);
        let parts = split_message(&msg, 4000);
        assert_eq!(parts.len(), 1);
        assert_eq!(parts[0].len(), 4000);
    }

    #[test]
    fn test_split_message_at_newline() {
        let mut msg = "a".repeat(3990);
        msg.push('\n');
        msg.push_str(&"b".repeat(100));
        let parts = split_message(&msg, 4000);
        assert_eq!(parts.len(), 2);
        assert!(parts[0].ends_with('a'));
        assert!(parts[1].starts_with('b'));
    }

    #[test]
    fn test_split_message_at_double_newline() {
        let mut msg = "a".repeat(3980);
        msg.push_str("\n\n");
        msg.push_str(&"b".repeat(100));
        let parts = split_message(&msg, 4000);
        assert_eq!(parts.len(), 2);
        assert!(parts[0].ends_with('a'));
    }

    #[test]
    fn test_split_message_hard_split() {
        let msg = "a".repeat(8000);
        let parts = split_message(&msg, 4000);
        assert_eq!(parts.len(), 2);
        assert_eq!(parts[0].len(), 4000);
        assert_eq!(parts[1].len(), 4000);
    }

    #[test]
    fn test_truncate_short() {
        assert_eq!(truncate("hello", 10), "hello");
    }

    #[test]
    fn test_truncate_exact() {
        assert_eq!(truncate("hello", 5), "hello");
    }

    #[test]
    fn test_truncate_long() {
        assert_eq!(truncate("hello world", 5), "hello...");
    }

    #[test]
    fn test_parse_rate_limit() {
        let (cap, rate) = parse_rate_limit("5/min").expect("should parse 5/min");
        assert!((cap - 5.0).abs() < f64::EPSILON);
        assert!((rate - 5.0 / 60.0).abs() < 0.001);

        let (cap, rate) = parse_rate_limit("10/hour").expect("should parse 10/hour");
        assert!((cap - 10.0).abs() < f64::EPSILON);
        assert!((rate - 10.0 / 3600.0).abs() < 0.0001);

        let (cap, rate) = parse_rate_limit("1/sec").expect("should parse 1/sec");
        assert!((cap - 1.0).abs() < f64::EPSILON);
        assert!((rate - 1.0).abs() < f64::EPSILON);

        assert!(parse_rate_limit("garbage").is_none());
        assert!(parse_rate_limit("").is_none());
        assert!(parse_rate_limit("5/century").is_none());
    }

    #[test]
    fn test_parse_duration_seconds() {
        assert_eq!(
            parse_duration("30s"),
            Some(std::time::Duration::from_secs(30))
        );
        assert_eq!(
            parse_duration("1s"),
            Some(std::time::Duration::from_secs(1))
        );
    }

    #[test]
    fn test_parse_duration_minutes() {
        assert_eq!(
            parse_duration("5m"),
            Some(std::time::Duration::from_secs(300))
        );
        assert_eq!(
            parse_duration("1m"),
            Some(std::time::Duration::from_secs(60))
        );
    }

    #[test]
    fn test_parse_duration_hours() {
        assert_eq!(
            parse_duration("4h"),
            Some(std::time::Duration::from_secs(14400))
        );
        assert_eq!(
            parse_duration("1h"),
            Some(std::time::Duration::from_secs(3600))
        );
    }

    #[test]
    fn test_parse_duration_days() {
        assert_eq!(
            parse_duration("1d"),
            Some(std::time::Duration::from_secs(86400))
        );
        assert_eq!(
            parse_duration("2d"),
            Some(std::time::Duration::from_secs(172800))
        );
    }

    #[test]
    fn test_parse_duration_invalid() {
        assert_eq!(parse_duration("0s"), None);
        assert_eq!(parse_duration("0"), None);
        assert_eq!(parse_duration(""), None);
        assert_eq!(parse_duration("abc"), None);
        assert_eq!(parse_duration("5x"), None);
        assert_eq!(parse_duration("s"), None);
    }

    #[test]
    fn test_merge_messages_single() {
        let msgs = vec!["hello world".to_string()];
        assert_eq!(merge_messages(&msgs), "hello world");
    }

    #[test]
    fn test_merge_messages_multiple() {
        let msgs = vec![
            "Hey".to_string(),
            "can you help me".to_string(),
            "with this problem?".to_string(),
        ];
        assert_eq!(
            merge_messages(&msgs),
            "Hey\ncan you help me\nwith this problem?"
        );
    }

    #[test]
    fn test_voice_prompt_with_text() {
        let prompt = voice_prompt("What did they say?");
        assert!(prompt.contains("voice message"));
        assert!(prompt.contains("What did they say?"));
    }

    #[test]
    fn test_voice_prompt_without_text() {
        let prompt = voice_prompt("");
        assert!(prompt.contains("voice message"));
        assert!(!prompt.contains("along with"));
    }

    #[test]
    fn test_find_free_port_returns_valid() {
        let port = find_free_port(0);
        assert!(port > 0);
        assert!(TcpListener::bind(("127.0.0.1", port)).is_ok());
    }

    #[test]
    fn test_find_free_port_preferred() {
        let free = find_free_port(0);
        let result = find_free_port(free);
        assert_eq!(result, free);
    }

    #[test]
    fn test_isolated_workdir_unique_per_sender() {
        let dir_a = isolated_workdir("+1234567890");
        let dir_b = isolated_workdir("+9876543210");
        assert_ne!(dir_a, dir_b);
        assert_eq!(dir_a, isolated_workdir("+1234567890"));
        assert!(dir_a.starts_with(std::env::temp_dir().join("ccchat")));
    }

    #[test]
    fn test_is_command() {
        assert!(is_command("/status"));
        assert!(is_command("/reset"));
        assert!(is_command("  /allow 1"));
        assert!(!is_command("hello"));
        assert!(!is_command("not a /command"));
    }

    #[test]
    fn test_looks_truncated_short() {
        assert!(!looks_truncated("Short response."));
        assert!(!looks_truncated(""));
    }

    #[test]
    fn test_looks_truncated_ends_with_period() {
        let response = format!("{}.", "a".repeat(3600));
        assert!(!looks_truncated(&response));
    }

    #[test]
    fn test_looks_truncated_ends_without_punctuation() {
        let response = format!("{} and then the function", "a".repeat(3600));
        assert!(looks_truncated(&response));
    }

    #[test]
    fn test_looks_truncated_ends_with_question() {
        let response = format!("{}?", "a".repeat(3600));
        assert!(!looks_truncated(&response));
    }

    #[test]
    fn test_looks_truncated_ends_with_backtick() {
        let response = format!("{}`", "a".repeat(3600));
        assert!(!looks_truncated(&response));
    }

    #[test]
    fn test_content_type_from_extension_png() {
        assert_eq!(
            content_type_from_extension(std::path::Path::new("image.png")),
            "image/png"
        );
        assert_eq!(
            content_type_from_extension(std::path::Path::new("photo.jpg")),
            "image/jpeg"
        );
        assert_eq!(
            content_type_from_extension(std::path::Path::new("doc.pdf")),
            "application/pdf"
        );
    }

    #[test]
    fn test_content_type_from_extension_unknown() {
        assert_eq!(
            content_type_from_extension(std::path::Path::new("file.xyz")),
            "application/octet-stream"
        );
        assert_eq!(
            content_type_from_extension(std::path::Path::new("noext")),
            "application/octet-stream"
        );
    }

    #[test]
    fn test_extract_file_references_none() {
        let text = "Here is a regular response with no file paths.";
        let refs = extract_file_references(text);
        assert!(refs.is_empty());
    }

    #[test]
    fn test_extract_file_references_single_file() {
        let dir = PathBuf::from("/tmp/ccchat");
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join(format!("test_extract_{}.txt", std::process::id()));
        std::fs::write(&path, "test content").unwrap();

        let text = format!("I created the file at {}", path.display());
        let refs = extract_file_references(&text);
        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0], path);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn test_extract_file_references_multiple() {
        let dir = PathBuf::from("/tmp/ccchat");
        let _ = std::fs::create_dir_all(&dir);
        let path1 = dir.join(format!("multi_a_{}.txt", std::process::id()));
        let path2 = dir.join(format!("multi_b_{}.txt", std::process::id()));
        std::fs::write(&path1, "a").unwrap();
        std::fs::write(&path2, "b").unwrap();

        let text = format!("Files: {} and {}", path1.display(), path2.display());
        let refs = extract_file_references(&text);
        assert_eq!(refs.len(), 2);
        let _ = std::fs::remove_file(&path1);
        let _ = std::fs::remove_file(&path2);
    }

    #[test]
    fn test_extract_file_references_ignores_nonexistent() {
        let text = "Check /tmp/ccchat/nonexistent_file_xyz.txt for details.";
        let refs = extract_file_references(text);
        assert!(refs.is_empty());
    }

    // --- Cron helper tests ---

    #[test]
    fn test_parse_interval_secs_hourly() {
        assert_eq!(parse_interval_secs("1h"), Some(3600));
    }

    #[test]
    fn test_parse_interval_secs_minutes() {
        assert_eq!(parse_interval_secs("30m"), Some(1800));
    }

    #[test]
    fn test_parse_interval_secs_daily() {
        assert_eq!(parse_interval_secs("1d"), Some(86400));
    }

    #[test]
    fn test_parse_interval_secs_invalid() {
        assert_eq!(parse_interval_secs("abc"), None);
        assert_eq!(parse_interval_secs(""), None);
    }

    #[test]
    fn test_parse_daily_time_valid() {
        assert_eq!(parse_daily_time("09:00"), Some("0 9 * * *".to_string()));
    }

    #[test]
    fn test_parse_daily_time_afternoon() {
        assert_eq!(parse_daily_time("14:30"), Some("30 14 * * *".to_string()));
    }

    #[test]
    fn test_parse_daily_time_invalid_hour() {
        assert_eq!(parse_daily_time("25:00"), None);
    }

    #[test]
    fn test_parse_daily_time_invalid_format() {
        assert_eq!(parse_daily_time("nine"), None);
        assert_eq!(parse_daily_time(""), None);
    }

    #[test]
    fn test_format_cron_human_valid() {
        let result = format_cron_human("0 9 * * *");
        assert!(!result.is_empty());
    }

    #[test]
    fn test_format_cron_human_invalid_falls_back() {
        let result = format_cron_human("not valid cron");
        assert_eq!(result, "not valid cron");
    }
}
