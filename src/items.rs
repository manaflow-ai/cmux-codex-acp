//! Codex thread items as ACP tool calls.

use crate::patch;
use serde_json::{Value, json};
use std::path::Path;

fn s<'a>(v: &'a Value, k: &str) -> Option<&'a str> {
    v.get(k).and_then(Value::as_str)
}

fn file_name(path: &str) -> String {
    Path::new(path).file_name().map(|f| f.to_string_lossy().into_owned()).unwrap_or_else(|| path.to_owned())
}

/// The script a shell wrapper runs: `/bin/zsh -lc 'cat a.txt'` is
/// `cat a.txt`. Any other command line is returned as it is.
pub fn shell_script(command: &str) -> String {
    for shell in ["/bin/zsh", "/bin/bash", "/bin/sh", "zsh", "bash", "sh", "/usr/bin/env bash"] {
        for flag in [" -lc ", " -c "] {
            if let Some(rest) = command.strip_prefix(shell).and_then(|r| r.strip_prefix(flag)) {
                let rest = rest.trim();
                if rest.len() >= 2 && rest.starts_with('\'') && rest.ends_with('\'') {
                    return rest[1..rest.len() - 1].replace("'\\''", "'");
                }
                if rest.len() >= 2 && rest.starts_with('"') && rest.ends_with('"') {
                    return rest[1..rest.len() - 1].replace("\\\"", "\"");
                }
                return rest.to_owned();
            }
        }
    }
    command.to_owned()
}

/// A command's ACP kind and title from Codex's parsed actions: one read is
/// a read, one search a search; anything else is a command.
fn command_kind_title(item: &Value, script: &str) -> (&'static str, String) {
    let actions: Vec<&Value> = item.get("commandActions").and_then(Value::as_array).into_iter().flatten().collect();
    let first_line = script.lines().next().unwrap_or("").chars().take(160).collect::<String>();
    if let [a] = actions.as_slice() {
        match s(a, "type") {
            Some("read") => return ("read", format!("Read {}", s(a, "name").unwrap_or(&first_line))),
            Some("search") => {
                let q = s(a, "query").unwrap_or("");
                return ("search", if q.is_empty() { first_line } else { format!("Search {q}") });
            }
            Some("listFiles") => return ("search", format!("List {}", s(a, "path").unwrap_or("."))),
            _ => {}
        }
    }
    ("execute", first_line)
}

fn locations_of_actions(item: &Value) -> Option<Value> {
    let paths: Vec<Value> = item
        .get("commandActions")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|a| s(a, "path").map(|p| json!({"path": p})))
        .collect();
    (!paths.is_empty()).then_some(Value::Array(paths))
}

fn status(item: &Value) -> &'static str {
    match s(item, "status") {
        Some("completed") => "completed",
        Some("failed" | "declined" | "interrupted") => "failed",
        _ => "in_progress",
    }
}

fn text_content(t: impl Into<String>) -> Value {
    json!({"type": "content", "content": {"type": "text", "text": t.into()}})
}

/// The most of a file read for a change's before text.
const MAX_READ: u64 = 8 << 20;

/// A regular file's text, up to `MAX_READ` bytes; None for anything else
/// (a FIFO or device would block, a huge file is not worth a diff).
fn read_small(path: &str) -> Option<String> {
    let meta = std::fs::metadata(path).ok()?;
    if !meta.is_file() || meta.len() > MAX_READ {
        return None;
    }
    std::fs::read_to_string(path).ok()
}

/// The ACP `diff` blocks of a file change item, reading each file once.
pub fn file_diffs(item: &Value) -> Vec<Value> {
    item.get("changes")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|c| {
            let path = s(c, "path")?;
            let kind = c.pointer("/kind/type").and_then(Value::as_str).unwrap_or("update");
            let diff = s(c, "diff").unwrap_or("");
            let old = read_small(path);
            let target = c.pointer("/kind/move_path").and_then(Value::as_str).unwrap_or(path);
            match patch::before_after(kind, old.as_deref(), diff) {
                Some((before, after)) => {
                    Some(json!({"type": "diff", "path": target, "oldText": before, "newText": after}))
                }
                // The hunks do not fit the file: show the diff itself.
                None => Some(text_content(format!("{target}\n{diff}"))),
            }
        })
        .collect()
}

fn file_title(item: &Value) -> String {
    let changes: Vec<&Value> = item.get("changes").and_then(Value::as_array).into_iter().flatten().collect();
    match changes.as_slice() {
        [one] => {
            let verb = match one.pointer("/kind/type").and_then(Value::as_str) {
                Some("add") => "Write",
                Some("delete") => "Delete",
                _ => "Edit",
            };
            format!("{verb} {}", file_name(s(one, "path").unwrap_or("")))
        }
        many => format!("Edit {} files", many.len()),
    }
}

fn file_locations(item: &Value) -> Value {
    Value::Array(
        item.get("changes")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|c| s(c, "path").map(|p| json!({"path": p})))
            .collect(),
    )
}

fn collab_title(item: &Value) -> String {
    let n = item.get("receiverThreadIds").and_then(Value::as_array).map(Vec::len).unwrap_or(0);
    match s(item, "tool") {
        Some("spawnAgent") => "Start agent".into(),
        Some("wait") => "Wait for agents".into(),
        Some("sendInput" | "sendMessage" | "followupTask") => "Message agent".into(),
        Some("resumeAgent") => "Resume agent".into(),
        Some("interruptAgent") => "Interrupt agent".into(),
        Some("closeAgent") => "Close agent".into(),
        Some("listAgents") => "List agents".into(),
        _ if n > 0 => format!("Agents ({n})"),
        _ => "Agents".into(),
    }
}

/// MCP or dynamic tool result content as ACP tool call content.
fn tool_result_content(blocks: Option<&Value>) -> Vec<Value> {
    blocks
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|b| match s(b, "type") {
            Some("text" | "inputText" | "input_text") => Some(text_content(s(b, "text").unwrap_or(""))),
            Some("image") => Some(json!({"type": "content", "content": b})),
            Some("inputImage" | "input_image") => {
                let url = s(b, "imageUrl").or(s(b, "image_url"))?;
                let (mime, data) = url.strip_prefix("data:")?.split_once(";base64,")?;
                Some(json!({"type": "content", "content": {"type": "image", "mimeType": mime, "data": data}}))
            }
            _ => Some(text_content(b.to_string())),
        })
        .collect()
}

/// What one tool-like item looks like as an ACP tool call: the fields of a
/// `tool_call` (at start) or `tool_call_update` (at completion). None for
/// items that are not tool calls (messages, reasoning, subagent activity).
/// `diffs` are a file change's diff blocks, computed once at its start.
pub fn tool_fields(item: &Value, done: bool, diffs: Option<&[Value]>) -> Option<Value> {
    let id = s(item, "id")?;
    let mut f = json!({"toolCallId": id, "status": if done { status(item) } else { "in_progress" }});
    match s(item, "type")? {
        "commandExecution" => {
            let command = s(item, "command").unwrap_or("");
            let script = shell_script(command);
            let (kind, title) = command_kind_title(item, &script);
            f["kind"] = json!(kind);
            f["title"] = json!(title);
            f["rawInput"] = json!({"command": script, "cwd": item.get("cwd"), "commandLine": command});
            if let Some(l) = locations_of_actions(item) {
                f["locations"] = l;
            }
            if done {
                let output = s(item, "aggregatedOutput").unwrap_or("");
                f["content"] = json!([text_content(output)]);
                f["rawOutput"] = json!({"exit_code": item.get("exitCode"), "duration_ms": item.get("durationMs"), "status": item.get("status")});
            }
        }
        "fileChange" => {
            f["kind"] = json!("edit");
            f["title"] = json!(file_title(item));
            f["locations"] = file_locations(item);
            if let Some(d) = diffs {
                f["content"] = Value::Array(d.to_vec());
            }
            f["rawInput"] = json!({"changes": item.get("changes")});
        }
        "mcpToolCall" => {
            let server = s(item, "server").unwrap_or("mcp");
            let tool = s(item, "tool").unwrap_or("tool");
            f["kind"] = json!(if item.get("readOnlyHint") == Some(&json!(true)) { "read" } else { "other" });
            f["title"] = json!(format!("{server}: {tool}"));
            f["rawInput"] = item.get("arguments").cloned().unwrap_or(Value::Null);
            if done {
                let mut content = tool_result_content(item.pointer("/result/content"));
                if let Some(err) = item.pointer("/error/message").and_then(Value::as_str) {
                    content.push(text_content(err));
                }
                f["content"] = Value::Array(content);
                f["rawOutput"] = item.get("result").cloned().unwrap_or(Value::Null);
            }
        }
        "dynamicToolCall" => {
            f["kind"] = json!("other");
            f["title"] = json!(s(item, "tool").unwrap_or("tool"));
            f["rawInput"] = item.get("arguments").cloned().unwrap_or(Value::Null);
            if done {
                f["content"] = Value::Array(tool_result_content(item.get("contentItems")));
            }
        }
        "webSearch" => {
            f["kind"] = json!("fetch");
            let q = s(item, "query").unwrap_or("");
            f["title"] = json!(if q.is_empty() { "Web search".to_owned() } else { format!("Search {q}") });
            f["rawInput"] = json!({"query": q, "action": item.get("action")});
            if done {
                f["status"] = json!("completed");
                if let Some(r) = item.get("results").filter(|r| !r.is_null()) {
                    f["rawOutput"] = r.clone();
                }
            }
        }
        "imageView" => {
            let path = s(item, "path").unwrap_or("");
            f["kind"] = json!("read");
            f["title"] = json!(format!("View {}", file_name(path)));
            f["locations"] = json!([{"path": path}]);
            if done {
                f["status"] = json!("completed");
            }
        }
        "imageGeneration" => {
            f["kind"] = json!("other");
            f["title"] = json!("Generate image");
            f["rawInput"] = json!({"prompt": item.get("revisedPrompt")});
            if done && let Some(p) = s(item, "savedPath") {
                f["content"] = json!([{"type": "content", "content": {"type": "resource_link", "uri": format!("file://{p}"), "name": file_name(p)}}]);
                f["locations"] = json!([{"path": p}]);
            }
        }
        "collabAgentToolCall" => {
            f["kind"] = json!("think");
            f["title"] = json!(collab_title(item));
            f["rawInput"] = json!({
                "tool": item.get("tool"),
                "prompt": item.get("prompt"),
                "model": item.get("model"),
                "reasoningEffort": item.get("reasoningEffort"),
                "receiverThreadIds": item.get("receiverThreadIds"),
                "agentsStates": item.get("agentsStates"),
            });
        }
        "contextCompaction" => {
            f["kind"] = json!("other");
            f["title"] = json!("Compact context");
            if done {
                f["status"] = json!("completed");
            }
        }
        "sleep" => {
            f["kind"] = json!("other");
            f["title"] = json!(format!("Wait {} ms", item.get("durationMs").and_then(Value::as_u64).unwrap_or(0)));
            if done {
                f["status"] = json!("completed");
            }
        }
        _ => return None,
    }
    f["_meta"] = json!({"codexAppServer": {"itemType": item.get("type")}});
    Some(f)
}
