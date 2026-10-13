//! ACP content blocks and Codex user input.

use serde_json::{Value, json};

/// The path of a `file://` URI, percent-decoded.
fn file_path(uri: &str) -> Option<String> {
    let raw = uri.strip_prefix("file://")?;
    let raw = raw.strip_prefix("localhost").unwrap_or(raw);
    let bytes = raw.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && let Some(b) = raw.get(i + 1..i + 3).and_then(|h| u8::from_str_radix(h, 16).ok())
        {
            out.push(b);
            i += 3;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8(out).ok()
}

fn shown(uri: &str) -> String {
    file_path(uri).unwrap_or_else(|| uri.to_owned())
}

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
                if let Some(path) = file_path(&url) {
                    out.push(json!({"type": "localImage", "path": path}));
                } else {
                    out.push(json!({"type": "image", "url": url}));
                }
            }
            Some("resource_link") => {
                out.push(text(format!("@{}", shown(s("uri").unwrap_or("")))));
            }
            Some("resource") => {
                let r = b.get("resource").cloned().unwrap_or(Value::Null);
                let shown = shown(r.get("uri").and_then(Value::as_str).unwrap_or(""));
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
