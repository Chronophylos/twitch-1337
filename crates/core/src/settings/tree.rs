pub(super) fn diff(
    a: &serde_json::Value,
    b: &serde_json::Value,
) -> Vec<(String, serde_json::Value, serde_json::Value)> {
    let mut out = Vec::new();
    diff_at("", a, b, &mut out);
    out
}

fn diff_at(
    path: &str,
    a: &serde_json::Value,
    b: &serde_json::Value,
    out: &mut Vec<(String, serde_json::Value, serde_json::Value)>,
) {
    match (a.as_object(), b.as_object()) {
        (Some(a_obj), Some(b_obj)) => {
            let mut keys: Vec<&String> = a_obj.keys().chain(b_obj.keys()).collect();
            keys.sort();
            keys.dedup();

            for key in keys {
                let child_a = a_obj.get(key).unwrap_or(&serde_json::Value::Null);
                let child_b = b_obj.get(key).unwrap_or(&serde_json::Value::Null);
                let child_path = if path.is_empty() {
                    key.clone()
                } else {
                    format!("{path}.{key}")
                };
                diff_at(&child_path, child_a, child_b, out);
            }
        }
        _ => {
            if a != b {
                out.push((path.to_string(), a.clone(), b.clone()));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::diff;
    use serde_json::json;

    #[test]
    fn identical_values_emit_nothing() {
        let a = json!({ "cooldowns": { "ai": 30 } });
        assert!(diff(&a, &a).is_empty());
    }

    #[test]
    fn one_changed_leaf_in_nested_object() {
        let a = json!({ "ai": { "connection": { "timeout": 30 } } });
        let b = json!({ "ai": { "connection": { "timeout": 60 } } });
        let changes = diff(&a, &b);
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].0, "ai.connection.timeout");
        assert_eq!(changes[0].1, json!(30));
        assert_eq!(changes[0].2, json!(60));
    }

    #[test]
    fn null_vs_object_is_single_entry_not_recursed() {
        let a = json!({ "ai": { "prefill": null } });
        let b = json!({ "ai": { "prefill": { "threshold": 0.5, "enabled": true } } });
        let changes = diff(&a, &b);
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].0, "ai.prefill");
        assert_eq!(changes[0].1, json!(null));
        assert_eq!(changes[0].2, json!({ "threshold": 0.5, "enabled": true }));
    }

    #[test]
    fn two_objects_with_differing_leaves() {
        let a = json!({ "ai": { "prefill": { "threshold": 0.3, "enabled": false } } });
        let b = json!({ "ai": { "prefill": { "threshold": 0.5, "enabled": false } } });
        let changes = diff(&a, &b);
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].0, "ai.prefill.threshold");
        assert_eq!(changes[0].1, json!(0.3));
        assert_eq!(changes[0].2, json!(0.5));
    }

    #[test]
    fn array_diff_is_wholesale() {
        let a = json!({ "schedules": [1, 2, 3] });
        let b = json!({ "schedules": [1, 2, 4] });
        let changes = diff(&a, &b);
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].0, "schedules");
        assert_eq!(changes[0].1, json!([1, 2, 3]));
        assert_eq!(changes[0].2, json!([1, 2, 4]));
    }

    #[test]
    fn key_present_on_only_one_side() {
        let a = json!({ "cooldowns": { "ai": 30 } });
        let b = json!({ "cooldowns": { "ai": 30, "news": 60 } });
        let changes = diff(&a, &b);
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].0, "cooldowns.news");
        assert_eq!(changes[0].1, json!(null));
        assert_eq!(changes[0].2, json!(60));
    }
}
