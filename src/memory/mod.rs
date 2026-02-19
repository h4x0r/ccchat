mod admin;
mod config;
mod context;
pub(crate) mod messages;
pub(crate) mod schema;

// Re-export the public API so callers use `crate::memory::*` unchanged
pub(crate) use admin::{forget_with_counts, memory_status, search_memory_formatted};
pub(crate) use config::{
    allowed_file_path, export_config, load_config_file, load_persisted_allowed, persist_allow,
    persist_revoke, reload_config_full, validate_config_entries,
};
pub(crate) use context::{format_epoch, inject_context, save_memory, store_message_pair};
pub(crate) use messages::purge_old_messages;
pub(crate) use schema::{hash_sender, open_memory_db};

#[cfg(test)]
pub(crate) use context::delete_memory;
#[cfg(test)]
pub(crate) use messages::store_message;
