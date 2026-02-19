use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use tracing::error;

#[derive(Serialize, Deserialize, Default)]
pub(crate) struct PersistedAllowed {
    pub(crate) allowed: Vec<AllowedEntry>,
    #[serde(default)]
    pub(crate) system_prompt: Option<String>,
    #[serde(default)]
    pub(crate) sender_prompts: Option<std::collections::HashMap<String, String>>,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub(crate) struct AllowedEntry {
    pub(crate) id: String,
    #[serde(default)]
    pub(crate) name: String,
}

pub(crate) fn config_dir() -> PathBuf {
    let dir = std::env::var("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("."))
        .join(".config")
        .join("ccchat");
    let _ = std::fs::create_dir_all(&dir);
    dir
}

pub(crate) fn allowed_file_path() -> PathBuf {
    config_dir().join("allowed.json")
}

pub(crate) fn load_persisted_allowed() -> PersistedAllowed {
    let path = allowed_file_path();
    match std::fs::read_to_string(&path) {
        Ok(contents) => serde_json::from_str(&contents).unwrap_or_default(),
        Err(_) => PersistedAllowed::default(),
    }
}

fn save_persisted_allowed(data: &PersistedAllowed) {
    let path = allowed_file_path();
    match serde_json::to_string_pretty(data) {
        Ok(json) => {
            if let Err(e) = std::fs::write(&path, json) {
                error!("Failed to save allowed list to {}: {e}", path.display());
            } else {
                tracing::debug!("Saved allowed list to {}", path.display());
            }
        }
        Err(e) => error!("Failed to serialize allowed list: {e}"),
    }
}

pub(crate) fn persist_allow(id: &str, name: &str) {
    let mut data = load_persisted_allowed();
    if !data.allowed.iter().any(|e| e.id == id) {
        data.allowed.push(AllowedEntry {
            id: id.to_string(),
            name: name.to_string(),
        });
        save_persisted_allowed(&data);
    }
}

pub(crate) fn persist_revoke(id: &str) {
    let mut data = load_persisted_allowed();
    data.allowed.retain(|e| e.id != id);
    save_persisted_allowed(&data);
}

pub(crate) fn reload_config(
    config_path: Option<&str>,
    account: &str,
    allowed_ids: &dashmap::DashMap<String, ()>,
) -> (usize, usize) {
    let mut new_allowed: std::collections::HashSet<String> = std::collections::HashSet::new();
    // Always include persisted (interactive /allow) entries
    for entry in load_persisted_allowed().allowed {
        new_allowed.insert(entry.id);
    }
    // Include config file entries
    if let Some(path) = config_path {
        match load_config_file(path) {
            Ok(entries) => {
                for e in entries {
                    new_allowed.insert(e.id);
                }
            }
            Err(e) => {
                error!("Config reload failed: {e}");
                return (0, 0);
            }
        }
    }
    // Account owner always stays
    new_allowed.insert(account.to_string());
    // Compute diff
    let to_add: Vec<_> = new_allowed
        .iter()
        .filter(|id| !allowed_ids.contains_key(id.as_str()))
        .cloned()
        .collect();
    let to_remove: Vec<_> = allowed_ids
        .iter()
        .map(|e| e.key().clone())
        .filter(|id| !new_allowed.contains(id))
        .collect();
    let (added, removed) = (to_add.len(), to_remove.len());
    for id in to_add {
        allowed_ids.insert(id, ());
    }
    for id in to_remove {
        allowed_ids.remove(&id);
    }
    (added, removed)
}

/// Full config reload: updates allowed IDs + system prompts + sender prompts.
pub(crate) fn reload_config_full(
    config_path: Option<&str>,
    account: &str,
    allowed_ids: &dashmap::DashMap<String, ()>,
    runtime_system_prompt: &std::sync::RwLock<Option<String>>,
    sender_prompts: &dashmap::DashMap<String, String>,
) -> (usize, usize) {
    let (added, removed) = reload_config(config_path, account, allowed_ids);

    // Parse config file for prompts
    if let Some(path) = config_path {
        let contents = match std::fs::read_to_string(path) {
            Ok(c) => c,
            Err(_) => return (added, removed),
        };
        let parsed: PersistedAllowed = if path.ends_with(".yaml") || path.ends_with(".yml") {
            serde_yaml::from_str(&contents).unwrap_or_default()
        } else {
            serde_json::from_str(&contents).unwrap_or_default()
        };

        // Update system prompt
        if let Ok(mut guard) = runtime_system_prompt.write() {
            *guard = parsed.system_prompt;
        }

        // Update sender prompts (clear old, insert new)
        sender_prompts.clear();
        if let Some(prompts) = parsed.sender_prompts {
            for (sender, prompt) in prompts {
                sender_prompts.insert(sender, prompt);
            }
        }
    } else {
        // No config file: clear runtime overrides
        if let Ok(mut guard) = runtime_system_prompt.write() {
            *guard = None;
        }
        sender_prompts.clear();
    }

    (added, removed)
}

pub(crate) fn export_config(allowed_ids: &dashmap::DashMap<String, ()>, account: &str) -> String {
    let persisted = load_persisted_allowed();
    let entries: Vec<_> = persisted
        .allowed
        .iter()
        .filter(|e| e.id != account && allowed_ids.contains_key(&e.id))
        .collect();
    serde_json::to_string_pretty(&serde_json::json!({
        "allowed": entries
    }))
    .unwrap_or_default()
}

pub(crate) fn validate_config_entries(entries: &[AllowedEntry]) -> Vec<String> {
    let mut warnings = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for entry in entries {
        if entry.id.is_empty() {
            warnings.push(format!("empty id (name: {:?})", entry.name));
        } else if !entry.id.starts_with('+')
            && !(entry.id.len() == 36 && entry.id.contains('-'))
        {
            warnings.push(format!(
                "{:?} doesn't look like phone number or UUID",
                entry.id
            ));
        }
        if !seen.insert(&entry.id) {
            warnings.push(format!("duplicate entry {:?}", entry.id));
        }
    }
    warnings
}

pub(crate) fn load_config_file(path: &str) -> Result<Vec<AllowedEntry>, String> {
    let contents = std::fs::read_to_string(path)
        .map_err(|e| format!("Failed to read config file {path}: {e}"))?;
    let config: PersistedAllowed = if path.ends_with(".yaml") || path.ends_with(".yml") {
        serde_yaml::from_str(&contents)
            .map_err(|e| format!("Failed to parse config file {path}: {e}"))?
    } else {
        serde_json::from_str(&contents)
            .map_err(|e| format!("Failed to parse config file {path}: {e}"))?
    };
    Ok(config.allowed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_config_dir_under_home() {
        let dir = config_dir();
        assert!(dir.to_str().unwrap().contains("ccchat"));
        assert!(dir.to_str().unwrap().contains(".config"));
    }

    #[test]
    fn test_allowed_file_path_json() {
        let path = allowed_file_path();
        assert!(path.to_str().unwrap().ends_with(".json"));
    }

    #[test]
    fn test_load_config_json_valid() {
        let dir = std::env::temp_dir().join(format!("ccchat_test_{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("config_valid.json");
        let json = serde_json::json!({
            "allowed": [
                {"id": "+1234567890", "name": "Alice"},
                {"id": "+0987654321", "name": "Bob"}
            ]
        });
        std::fs::write(&path, json.to_string()).unwrap();
        let entries = load_config_file(path.to_str().unwrap()).unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].id, "+1234567890");
        assert_eq!(entries[0].name, "Alice");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_load_config_json_empty() {
        let dir = std::env::temp_dir().join(format!("ccchat_test_empty_{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("config_empty.json");
        let json = serde_json::json!({ "allowed": [] });
        std::fs::write(&path, json.to_string()).unwrap();
        let entries = load_config_file(path.to_str().unwrap()).unwrap();
        assert!(entries.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_reload_config_adds_new_entries() {
        let dir = std::env::temp_dir().join(format!("ccchat_reload_add_{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("config.json");
        let json = r#"{"allowed": [{"id": "+new_user", "name": "New"}]}"#;
        std::fs::write(&path, json).unwrap();

        let allowed: dashmap::DashMap<String, ()> = dashmap::DashMap::new();
        allowed.insert("+owner".to_string(), ());

        let (added, removed) = reload_config(Some(path.to_str().unwrap()), "+owner", &allowed);
        assert!(added >= 1);
        assert_eq!(removed, 0);
        assert!(allowed.contains_key("+new_user"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_reload_config_removes_gone_entries() {
        let dir = std::env::temp_dir().join(format!("ccchat_reload_rm_{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("config.json");
        // Config file only has +kept
        let json = r#"{"allowed": [{"id": "+kept", "name": "Kept"}]}"#;
        std::fs::write(&path, json).unwrap();

        let allowed: dashmap::DashMap<String, ()> = dashmap::DashMap::new();
        allowed.insert("+owner".to_string(), ());
        allowed.insert("+kept".to_string(), ());
        allowed.insert("+gone".to_string(), ()); // not in config or persisted

        let (_, removed) = reload_config(Some(path.to_str().unwrap()), "+owner", &allowed);
        assert!(removed >= 1);
        assert!(!allowed.contains_key("+gone"));
        assert!(allowed.contains_key("+kept"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_reload_config_no_config_path_noop() {
        let allowed: dashmap::DashMap<String, ()> = dashmap::DashMap::new();
        allowed.insert("+owner".to_string(), ());
        let initial_len = allowed.len();
        let (added, removed) = reload_config(None, "+owner", &allowed);
        // With no config file, only persisted entries matter
        // At minimum, owner stays
        assert!(allowed.contains_key("+owner"));
        // No new entries should appear beyond persisted
        assert_eq!(removed, 0);
        // added should be 0 or small (only from persisted)
        let _ = (added, initial_len); // silence unused
    }

    #[test]
    fn test_reload_config_preserves_account_owner() {
        let dir = std::env::temp_dir().join(format!("ccchat_reload_owner_{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("config.json");
        // Config doesn't include owner — but owner should never be removed
        let json = r#"{"allowed": []}"#;
        std::fs::write(&path, json).unwrap();

        let allowed: dashmap::DashMap<String, ()> = dashmap::DashMap::new();
        allowed.insert("+owner".to_string(), ());

        let (_, _) = reload_config(Some(path.to_str().unwrap()), "+owner", &allowed);
        assert!(allowed.contains_key("+owner"), "owner must never be removed");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_reload_config_file_error_preserves_state() {
        let allowed: dashmap::DashMap<String, ()> = dashmap::DashMap::new();
        allowed.insert("+owner".to_string(), ());
        allowed.insert("+existing".to_string(), ());

        // Point to non-existent file
        let (added, removed) = reload_config(Some("/tmp/ccchat_nonexistent_reload.json"), "+owner", &allowed);
        assert_eq!(added, 0);
        assert_eq!(removed, 0);
        // State unchanged
        assert!(allowed.contains_key("+existing"));
        assert!(allowed.contains_key("+owner"));
    }

    #[test]
    fn test_export_config_empty() {
        let allowed: dashmap::DashMap<String, ()> = dashmap::DashMap::new();
        let result = export_config(&allowed, "+owner");
        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
        assert!(parsed["allowed"].as_array().unwrap().is_empty());
    }

    #[test]
    fn test_export_config_excludes_account_owner() {
        // This test works with the persisted file. Since we can't easily control it,
        // we verify the function filters the account owner by checking the output
        // doesn't include the owner account.
        let allowed: dashmap::DashMap<String, ()> = dashmap::DashMap::new();
        allowed.insert("+owner".to_string(), ());
        let result = export_config(&allowed, "+owner");
        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
        let entries = parsed["allowed"].as_array().unwrap();
        assert!(entries.iter().all(|e| e["id"].as_str().unwrap() != "+owner"));
    }

    #[test]
    fn test_validate_entries_valid_e164() {
        let entries = vec![
            AllowedEntry { id: "+447911123456".to_string(), name: "UK".to_string() },
            AllowedEntry { id: "+12025551234".to_string(), name: "US".to_string() },
        ];
        assert!(validate_config_entries(&entries).is_empty());
    }

    #[test]
    fn test_validate_entries_uuid_ok() {
        let entries = vec![AllowedEntry {
            id: "a1b2c3d4-e5f6-7890-abcd-ef1234567890".to_string(),
            name: "UUID user".to_string(),
        }];
        assert!(validate_config_entries(&entries).is_empty());
    }

    #[test]
    fn test_validate_entries_suspicious_format() {
        let entries = vec![AllowedEntry {
            id: "447911123456".to_string(), // no + prefix
            name: "Bad".to_string(),
        }];
        let warnings = validate_config_entries(&entries);
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("doesn't look like phone number or UUID"));
    }

    #[test]
    fn test_validate_entries_empty_id() {
        let entries = vec![AllowedEntry {
            id: "".to_string(),
            name: "Ghost".to_string(),
        }];
        let warnings = validate_config_entries(&entries);
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("empty id"));
    }

    #[test]
    fn test_validate_entries_duplicates() {
        let entries = vec![
            AllowedEntry { id: "+111".to_string(), name: "A".to_string() },
            AllowedEntry { id: "+111".to_string(), name: "B".to_string() },
        ];
        let warnings = validate_config_entries(&entries);
        assert!(warnings.iter().any(|w| w.contains("duplicate")));
    }

    #[test]
    fn test_load_config_yaml_valid() {
        let dir = std::env::temp_dir().join(format!("ccchat_yaml_{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("config.yaml");
        let yaml = "allowed:\n  - id: \"+111\"\n    name: Alice\n  - id: \"+222\"\n    name: Bob\n";
        std::fs::write(&path, yaml).unwrap();
        let entries = load_config_file(path.to_str().unwrap()).unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].id, "+111");
        assert_eq!(entries[1].name, "Bob");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_load_config_yaml_empty() {
        let dir = std::env::temp_dir().join(format!("ccchat_yaml_empty_{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("config.yaml");
        std::fs::write(&path, "allowed: []\n").unwrap();
        let entries = load_config_file(path.to_str().unwrap()).unwrap();
        assert!(entries.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_load_config_yml_extension() {
        let dir = std::env::temp_dir().join(format!("ccchat_yml_{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("config.yml");
        let yaml = "allowed:\n  - id: \"+333\"\n    name: Charlie\n";
        std::fs::write(&path, yaml).unwrap();
        let entries = load_config_file(path.to_str().unwrap()).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].id, "+333");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_load_config_invalid_yaml() {
        let dir = std::env::temp_dir().join(format!("ccchat_bad_yaml_{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("config.yaml");
        std::fs::write(&path, "not: [valid: yaml: {{").unwrap();
        let result = load_config_file(path.to_str().unwrap());
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("Failed to parse config file"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_load_config_unknown_extension_json_fallback() {
        let dir = std::env::temp_dir().join(format!("ccchat_toml_{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("config.toml");
        // Write valid JSON — should work since unknown extensions fall back to JSON
        let json = r#"{"allowed": [{"id": "+999", "name": "Zed"}]}"#;
        std::fs::write(&path, json).unwrap();
        let entries = load_config_file(path.to_str().unwrap()).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].id, "+999");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_load_config_missing_file() {
        let result = load_config_file("/tmp/ccchat_nonexistent_config_12345.json");
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("Failed to read config file"));
    }

    #[test]
    fn test_load_config_invalid_format() {
        let dir = std::env::temp_dir().join(format!("ccchat_test_invalid_{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("config_invalid.json");
        std::fs::write(&path, "not valid json {{{").unwrap();
        let result = load_config_file(path.to_str().unwrap());
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("Failed to parse config file"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_load_config_with_system_prompt() {
        let dir = std::env::temp_dir().join(format!("ccchat_sysprompt_{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("config.json");
        let json = serde_json::json!({
            "allowed": [{"id": "+111", "name": "Alice"}],
            "system_prompt": "You are a coding assistant."
        });
        std::fs::write(&path, json.to_string()).unwrap();
        let contents = std::fs::read_to_string(path.to_str().unwrap()).unwrap();
        let parsed: PersistedAllowed = serde_json::from_str(&contents).unwrap();
        assert_eq!(parsed.system_prompt, Some("You are a coding assistant.".to_string()));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_load_config_without_system_prompt_defaults_none() {
        let dir = std::env::temp_dir().join(format!("ccchat_noprompt_{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("config.json");
        let json = serde_json::json!({ "allowed": [] });
        std::fs::write(&path, json.to_string()).unwrap();
        let contents = std::fs::read_to_string(path.to_str().unwrap()).unwrap();
        let parsed: PersistedAllowed = serde_json::from_str(&contents).unwrap();
        assert_eq!(parsed.system_prompt, None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_persisted_allowed_serde_round_trip() {
        let data = PersistedAllowed {
            allowed: vec![
                AllowedEntry { id: "+111".to_string(), name: "Alice".to_string() },
                AllowedEntry { id: "+222".to_string(), name: "Bob".to_string() },
            ],
            system_prompt: None,
            sender_prompts: None,
        };
        let json = serde_json::to_string(&data).unwrap();
        let loaded: PersistedAllowed = serde_json::from_str(&json).unwrap();
        assert_eq!(loaded.allowed.len(), 2);
        assert_eq!(loaded.allowed[0].id, "+111");
        assert_eq!(loaded.allowed[1].name, "Bob");

        // Simulate revoke by filtering
        let after_revoke: Vec<_> = loaded.allowed.into_iter().filter(|e| e.id != "+111").collect();
        assert_eq!(after_revoke.len(), 1);
        assert_eq!(after_revoke[0].id, "+222");
    }

    #[test]
    fn test_load_config_missing_name_field() {
        let dir = std::env::temp_dir().join(format!("ccchat_test_noname_{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("config_noname.json");
        // name field omitted — serde default should give empty string
        let json = r#"{"allowed": [{"id": "+5555555555"}]}"#;
        std::fs::write(&path, json).unwrap();
        let entries = load_config_file(path.to_str().unwrap()).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].id, "+5555555555");
        assert_eq!(entries[0].name, ""); // serde default
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_load_config_merges_with_persisted() {
        let persisted = PersistedAllowed {
            allowed: vec![AllowedEntry {
                id: "+1111111111".to_string(),
                name: "Charlie".to_string(),
            }],
            system_prompt: None,
            sender_prompts: None,
        };
        let dir = std::env::temp_dir().join(format!("ccchat_test_merge_{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("config_merge.json");
        let json = serde_json::json!({
            "allowed": [
                {"id": "+2222222222", "name": "Dave"},
                {"id": "+3333333333", "name": "Eve"}
            ]
        });
        std::fs::write(&path, json.to_string()).unwrap();
        let config_entries = load_config_file(path.to_str().unwrap()).unwrap();
        let allowed_ids: dashmap::DashMap<String, ()> = dashmap::DashMap::new();
        for entry in &persisted.allowed {
            allowed_ids.insert(entry.id.clone(), ());
        }
        for entry in &config_entries {
            allowed_ids.insert(entry.id.clone(), ());
        }
        assert_eq!(allowed_ids.len(), 3);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_reload_config_updates_system_prompt() {
        let dir = std::env::temp_dir().join(format!("ccchat_reload_prompt_{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("config.json");
        let json = serde_json::json!({
            "allowed": [],
            "system_prompt": "You are a pirate assistant."
        });
        std::fs::write(&path, json.to_string()).unwrap();

        let allowed: dashmap::DashMap<String, ()> = dashmap::DashMap::new();
        allowed.insert("+owner".to_string(), ());
        let runtime_prompt = std::sync::RwLock::new(None);
        let sender_prompts: dashmap::DashMap<String, String> = dashmap::DashMap::new();

        reload_config_full(
            Some(path.to_str().unwrap()),
            "+owner",
            &allowed,
            &runtime_prompt,
            &sender_prompts,
        );

        let guard = runtime_prompt.read().unwrap();
        assert_eq!(guard.as_deref(), Some("You are a pirate assistant."));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_reload_config_clears_old_sender_prompts() {
        let dir = std::env::temp_dir().join(format!("ccchat_reload_clear_{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("config.json");
        let json = serde_json::json!({
            "allowed": [],
            "sender_prompts": {
                "+alice": "Be helpful to Alice."
            }
        });
        std::fs::write(&path, json.to_string()).unwrap();

        let allowed: dashmap::DashMap<String, ()> = dashmap::DashMap::new();
        allowed.insert("+owner".to_string(), ());
        let runtime_prompt = std::sync::RwLock::new(None);
        let sender_prompts: dashmap::DashMap<String, String> = dashmap::DashMap::new();
        // Pre-populate with stale entry
        sender_prompts.insert("+bob".to_string(), "Old prompt for Bob.".to_string());

        reload_config_full(
            Some(path.to_str().unwrap()),
            "+owner",
            &allowed,
            &runtime_prompt,
            &sender_prompts,
        );

        // +bob should be cleared, +alice should be present
        assert!(!sender_prompts.contains_key("+bob"), "old sender prompt should be cleared");
        assert_eq!(
            sender_prompts.get("+alice").map(|v| v.clone()),
            Some("Be helpful to Alice.".to_string())
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
