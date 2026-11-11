//! Expected result of the fixed two-lesson fixture, derived from its input relationships.

pub fn expected(ids: &[i64], multi: bool) -> serde_json::Value {
    let values: Vec<_> = ids
        .iter()
        .enumerate()
        .map(|(index, id)| {
            let ordinal = index + 1;
            let group = if ordinal % 2 == 0 { "cool" } else { "unknown" };
            let name = format!("Person {ordinal:05}");
            if multi {
                serde_json::json!({
                    "id": id, "display_name": name, "group_id": group, "group_name": group,
                    "__count": if group == "cool" { 1 } else { 2 },
                })
            } else {
                serde_json::json!({"id": id, "display_name": name, "__count": 2})
            }
        })
        .collect();
    if multi {
        serde_json::json!({"values": values})
    } else {
        serde_json::json!({"parent_field": false, "values": values})
    }
}
