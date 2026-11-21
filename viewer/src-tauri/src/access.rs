//! Host access rules (allowed IDs, allowed hours) and the idle timeout: the
//! settings commands and what a new session gets from them.

use super::*;
use chrono::{Datelike, Timelike};
use host_core::policy::{self, AccessRules, Admission, LocalTime};

/// The viewer is warned that the host will end the unused session soon.
#[derive(Serialize, Clone)]
pub(crate) struct IdleWarningEvent {
    pub(crate) seconds: u32,
}

/// This computer's local weekday and time of day.
pub(crate) fn local_now() -> LocalTime {
    let now = chrono::Local::now();
    LocalTime { weekday: now.weekday().num_days_from_monday() as u8, minute: (now.hour() * 60 + now.minute()) as u16 }
}

/// What the access rules say about `peer_id` connecting now, and the idle
/// timeout for its session.
pub(crate) fn session_policy(config: &ConfigStore, peer_id: &str) -> (Admission, Option<Duration>) {
    let settings = &config.config;
    let admission = policy::admit(settings.allow_mode, config.is_allowed(peer_id), &settings.allowed_hours, local_now());
    (admission, policy::idle_timeout(settings.idle_timeout_minutes))
}

#[tauri::command]
pub(crate) fn get_access_rules(state: tauri::State<'_, AppState>) -> AccessRules {
    state.lock().config.access_rules()
}

/// Save the access rules; they apply from the next connection. Returns them
/// as saved (trimmed labels, duplicates dropped).
#[tauri::command]
pub(crate) fn set_access_rules(state: tauri::State<'_, AppState>, rules: AccessRules) -> Result<AccessRules, String> {
    state.lock().config.set_access_rules(rules)
}

#[tauri::command]
pub(crate) fn get_idle_timeout(state: tauri::State<'_, AppState>) -> u32 {
    state.lock().config.config.idle_timeout_minutes
}

/// Minutes without viewer activity before a hosted session ends; 0 is never.
/// Applies from the next session.
#[tauri::command]
pub(crate) fn set_idle_timeout(state: tauri::State<'_, AppState>, minutes: u32) -> Result<(), String> {
    state.lock().config.set_idle_timeout_minutes(minutes)
}
