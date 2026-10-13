//! ACP content blocks and Codex user input.

use serde_json::{Value, json};

fn text(t: String) -> Value {
    json!({"type": "text", "text": t, "text_elements": []})
}

/// ACP prompt blocks as `UserInput` items for `turn/start`.
///
/// Text stays text. An image goes in as a data URL (or its URI). A file
/// link becomes an `@path` mention in the text, as Codex's own composer
/// writes it; embedded file contents are quoted inline with their path, so
/// the model sees what the client attached.
pub fn user_input(blocks: &[Value]) -> Vec<Value> {
    let mut out = Vec::new();
    for b in blocks {
        let s = |k: &str| b.get(k).and_then(Value::as_str);
        match s("type") {
            Some("text") => out.push(text(s("text").unwrap_or("").to_owned())),
            Some("image") => {
                let url = match (s("data"), s("uri")) {
                    (Some(data), _) => {
                        format!("data:{};base64,{data}", s("mimeType").unwrap_or("image/png"))
                    }
                    (None, Some(uri)) => uri.to_owned(),
                    _ => continue,
                };
                if let Some(path) = url.strip_prefix("file://") {
                    out.push(json!({"type": "localImage", "path": path}));
                } else {
                    out.push(json!({"type": "image", "url": url}));
                }
            }
            Some("resource_link") => {
                let uri = s("uri").unwrap_or("");
                let shown = uri.strip_prefix("file://").unwrap_or(uri);
                out.push(text(format!("@{shown}")));
            }
            Some("resource") => {
                let r = b.get("resource").cloned().unwrap_or(Value::Null);
                let uri = r.get("uri").and_then(Value::as_str).unwrap_or("");
                let shown = uri.strip_prefix("file://").unwrap_or(uri);
                if let Some(t) = r.get("text").and_then(Value::as_str) {
                    out.push(text(format!("<file path=\"{shown}\">\n{t}\n</file>")));
                } else {
                    out.push(text(format!("@{shown}")));
                }
            }
            _ => {}
        }
    }
    out
}

/// A stored user message's content as ACP blocks (history replay).
pub fn acp_blocks(content: &Value) -> Vec<Value> {
    content
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|c| match c.get("type").and_then(Value::as_str) {
            Some("text") => Some(json!({"type": "text", "text": c.get("text").cloned().unwrap_or(json!(""))})),
            Some("image") => {
                let url = c.get("url").and_then(Value::as_str)?;
                let (mime, data) = url.strip_prefix("data:")?.split_once(";base64,")?;
                Some(json!({"type": "image", "mimeType": mime, "data": data}))
            }
            Some("localImage") => Some(json!({"type": "resource_link", "uri": format!("file://{}", c.get("path")?.as_str()?), "name": "image"})),
            Some("mention" | "skill") => Some(json!({"type": "text", "text": format!("@{}", c.get("path")?.as_str()?)})),
            _ => None,
        })
        .collect()
}
