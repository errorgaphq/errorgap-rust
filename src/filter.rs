//! Mask sensitive keys from user-supplied params.

use serde_json::{Map, Value};

const FILTERED: &str = "[FILTERED]";

/// Return a new map with any key whose name contains a filtered substring
/// replaced with `"[FILTERED]"`. Recurses into nested objects.
pub(crate) fn filter(params: &Map<String, Value>, filter_keys: &[String]) -> Map<String, Value> {
    let lowered: Vec<String> = filter_keys.iter().map(|k| k.to_lowercase()).collect();
    walk(params, &lowered)
}

fn walk(input: &Map<String, Value>, lowered: &[String]) -> Map<String, Value> {
    let mut out = Map::with_capacity(input.len());
    for (key, value) in input {
        if is_sensitive(key, lowered) {
            out.insert(key.clone(), Value::String(FILTERED.into()));
        } else if let Value::Object(nested) = value {
            out.insert(key.clone(), Value::Object(walk(nested, lowered)));
        } else {
            out.insert(key.clone(), value.clone());
        }
    }
    out
}

fn is_sensitive(key: &str, lowered: &[String]) -> bool {
    let lk = key.to_lowercase();
    lowered.iter().any(|needle| lk.contains(needle.as_str()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn defaults() -> Vec<String> {
        vec!["password", "token", "secret", "api_key", "authorization", "cookie"]
            .into_iter()
            .map(String::from)
            .collect()
    }

    fn map(value: Value) -> Map<String, Value> {
        match value {
            Value::Object(m) => m,
            _ => panic!("expected object"),
        }
    }

    #[test]
    fn masks_filtered_keys() {
        let out = filter(
            &map(json!({"username":"alice","password":"hunter2","access_token":"x"})),
            &defaults(),
        );
        assert_eq!(out.get("username"), Some(&json!("alice")));
        assert_eq!(out.get("password"), Some(&json!("[FILTERED]")));
        assert_eq!(out.get("access_token"), Some(&json!("[FILTERED]")));
    }

    #[test]
    fn recurses_into_nested_maps() {
        let out = filter(
            &map(json!({"user":{"name":"alice","api_key":"x"}})),
            &defaults(),
        );
        let nested = out.get("user").unwrap().as_object().unwrap();
        assert_eq!(nested.get("name"), Some(&json!("alice")));
        assert_eq!(nested.get("api_key"), Some(&json!("[FILTERED]")));
    }

    #[test]
    fn case_insensitive() {
        let out = filter(&map(json!({"Authorization":"Bearer xyz"})), &defaults());
        assert_eq!(out.get("Authorization"), Some(&json!("[FILTERED]")));
    }
}
