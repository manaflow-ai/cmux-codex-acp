//! `session/load`: a resumed thread's stored turns, replayed as the session
//! updates they produced.

use super::*;
use crate::{content, items};

impl State {
    pub(super) fn replay(&mut self, sid: &str, turns: Option<&Value>, out: &mut Vec<Out>) {
        let items_of = |t: &Value| t.get("items").and_then(Value::as_array).cloned().unwrap_or_default();
        for turn in turns.and_then(Value::as_array).into_iter().flatten() {
            for item in items_of(turn) {
                self.replay_item(sid, &item, out);
            }
        }
    }

    fn replay_item(&mut self, sid: &str, item: &Value, out: &mut Vec<Out>) {
        let s = |k: &str| item.get(k).and_then(Value::as_str);
        match s("type").unwrap_or("") {
            "userMessage" => {
                for block in content::acp_blocks(item.get("content").unwrap_or(&Value::Null)) {
                    out.push(update(sid, json!({"sessionUpdate": "user_message_chunk", "content": block})));
                }
            }
            "agentMessage" | "plan" => {
                if let Some(text) = s("text").filter(|t| !t.is_empty()) {
                    out.push(update(sid, json!({"sessionUpdate": "agent_message_chunk", "messageId": s("id"), "content": {"type": "text", "text": text}})));
                }
            }
            "reasoning" => {
                let summary: Vec<&str> = item
                    .get("summary")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .filter_map(Value::as_str)
                    .collect();
                if !summary.is_empty() {
                    out.push(update(sid, json!({"sessionUpdate": "agent_thought_chunk", "content": {"type": "text", "text": summary.join("\n\n")}})));
                }
            }
            "subAgentActivity" | "hookPrompt" | "functionCallOutput" | "enteredReviewMode" | "exitedReviewMode" => {}
            "fileChange" => {
                // The files have changed since: the stored diff is the record
                // (no file reads for history).
                let diffs: Vec<Value> = item
                    .get("changes")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .map(|c| {
                        let path = c.get("path").and_then(Value::as_str).unwrap_or("");
                        let diff = c.get("diff").and_then(Value::as_str).unwrap_or("");
                        json!({"type": "content", "content": {"type": "text", "text": format!("{path}\n{diff}")}})
                    })
                    .collect();
                if let Some(mut f) = items::tool_fields(item, true, Some(&diffs)) {
                    f["sessionUpdate"] = json!("tool_call");
                    out.push(update(sid, f));
                }
            }
            _ => {
                if let Some(mut f) = items::tool_fields(item, true, None) {
                    f["sessionUpdate"] = json!("tool_call");
                    out.push(update(sid, f));
                }
            }
        }
    }
}
