//! Small pure helpers for the account / settings journal entries.

use serde_json::Value;

/// Only the global `data.*` settings are journaled; the rest of `app_settings`
/// is UI layout (columns, themes, window positions) and would only be noise.
pub fn is_journaled_setting(key: &str) -> bool {
    key.starts_with("data.")
}

/// Names of the top-level `settings.json` sections that differ, sorted.
pub fn changed_sections(old: &Value, new: &Value) -> Vec<String> {
    let empty = serde_json::Map::new();
    let o = old.as_object().unwrap_or(&empty);
    let n = new.as_object().unwrap_or(&empty);
    let mut keys: Vec<&String> = o.keys().chain(n.keys()).collect();
    keys.sort();
    keys.dedup();
    keys.into_iter().filter(|k| o.get(*k) != n.get(*k)).cloned().collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn only_data_settings_are_journaled() {
        assert!(is_journaled_setting("data.source"));
        assert!(is_journaled_setting("data.fyers_account"));
        assert!(!is_journaled_setting("ui.theme"));
        assert!(!is_journaled_setting("chain.columns@pop-1"));
        assert!(!is_journaled_setting("windows.open"));
    }

    #[test]
    fn changed_sections_lists_added_removed_and_edited() {
        let old = json!({ "trading": { "a": 1 }, "server": { "url": "x" }, "charts": 1 });
        let new = json!({ "trading": { "a": 2 }, "server": { "url": "x" }, "fyers": {} });
        assert_eq!(changed_sections(&old, &new), vec!["charts", "fyers", "trading"]);
        assert!(changed_sections(&old, &old).is_empty());
        assert!(changed_sections(&Value::Null, &Value::Null).is_empty());
        assert_eq!(changed_sections(&Value::Null, &json!({ "a": 1 })), vec!["a"]);
    }
}
