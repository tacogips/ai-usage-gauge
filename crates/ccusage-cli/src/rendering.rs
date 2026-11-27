use serde_json::Value;

pub fn render_value(value: &Value, json: bool) -> String {
    if json {
        return serde_json::to_string_pretty(value).unwrap_or_else(|_| "{}".into());
    }
    if let Some(obj) = value.as_object() {
        let mut lines = Vec::new();
        for (k, v) in obj {
            match v {
                Value::String(s) => lines.push(format!("{k}: {s}")),
                Value::Number(n) => lines.push(format!("{k}: {n}")),
                Value::Bool(b) => lines.push(format!("{k}: {b}")),
                Value::Null => lines.push(format!("{k}: null")),
                Value::Array(arr) => lines.push(format!("{k}: {} item(s)", arr.len())),
                Value::Object(_) => lines.push(format!(
                    "{k}: {}",
                    serde_json::to_string(v).unwrap_or_default()
                )),
            }
        }
        return lines.join("\n");
    }
    serde_json::to_string_pretty(value).unwrap_or_default()
}
