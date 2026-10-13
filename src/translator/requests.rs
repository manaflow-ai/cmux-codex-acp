//! app-server requests: approvals and questions become ACP
//! `session/request_permission`, answered by the client's policy.

use super::*;
use crate::items;

fn s<'a>(v: &'a Value, k: &str) -> Option<&'a str> {
    v.get(k).and_then(Value::as_str)
}

/// One ACP permission option for a Codex decision.
fn option_for(decision: &Value) -> Option<(Value, Value)> {
    let o = |id: &str, name: String, kind: &str| json!({"optionId": id, "name": name, "kind": kind});
    if let Some(d) = decision.as_str() {
        return Some((
            match d {
                "accept" => o("accept", "Allow".into(), "allow_once"),
                "acceptForSession" => o("acceptForSession", "Allow for this session".into(), "allow_always"),
                "decline" => o("decline", "Deny".into(), "reject_once"),
                "cancel" => o("cancel", "Deny and stop".into(), "reject_once"),
                _ => return None,
            },
            decision.clone(),
        ));
    }
    if let Some(a) = decision.pointer("/acceptWithExecpolicyAmendment/execpolicy_amendment").and_then(Value::as_array) {
        let prefix: Vec<&str> = a.iter().filter_map(Value::as_str).collect();
        return Some((
            o("acceptWithExecpolicyAmendment", format!("Always allow `{}`", prefix.join(" ")), "allow_always"),
            decision.clone(),
        ));
    }
    if let Some(a) = decision.pointer("/applyNetworkPolicyAmendment/network_policy_amendment") {
        let host = s(a, "host").unwrap_or("this host");
        let deny = s(a, "action") == Some("deny");
        let (name, kind) = if deny {
            (format!("Always deny {host}"), "reject_always")
        } else {
            (format!("Always allow {host}"), "allow_always")
        };
        let id = format!("{}:{host}", if deny { "denyNetworkHost" } else { "allowNetworkHost" });
        return Some((o(&id, name, kind), decision.clone()));
    }
    None
}

impl State {
    pub(super) fn server_request(&mut self, method: &str, id: Value, p: &Value, out: &mut Vec<Out>) {
        let thread = s(p, "threadId").unwrap_or("").to_owned();
        let item = s(p, "itemId").unwrap_or("").to_owned();
        let known = self.threads.contains_key(&thread);
        match method {
            "item/commandExecution/requestApproval" if known => {
                let mut call = self.tool_call_for(&thread, &item).unwrap_or_else(|| {
                    let command = s(p, "command").unwrap_or("");
                    json!({"toolCallId": item, "kind": "execute", "title": items::shell_script(command), "rawInput": {"command": items::shell_script(command), "cwd": p.get("cwd")}})
                });
                if let Some(reason) = s(p, "reason") {
                    call["content"] = json!([{"type": "content", "content": {"type": "text", "text": reason}}]);
                }
                call["_meta"] = json!({"codexAppServer": {
                    "kind": p.get("kind"),
                    "approvalId": p.get("approvalId"),
                    "network": p.get("networkApprovalContext"),
                    "proposedExecpolicyAmendment": p.get("proposedExecpolicyAmendment"),
                }});
                let decisions = p
                    .get("availableDecisions")
                    .and_then(Value::as_array)
                    .cloned()
                    .unwrap_or_else(|| default_command_decisions(p));
                self.ask(&thread, id, call, &decisions, Shape::Decision, out);
            }
            "item/fileChange/requestApproval" if known => {
                let mut call = self
                    .tool_call_for(&thread, &item)
                    .unwrap_or_else(|| json!({"toolCallId": item, "kind": "edit", "title": "Edit files"}));
                if let Some(reason) = s(p, "reason") {
                    let mut content = call.get("content").and_then(Value::as_array).cloned().unwrap_or_default();
                    content.push(json!({"type": "content", "content": {"type": "text", "text": reason}}));
                    call["content"] = Value::Array(content);
                }
                call["_meta"] = json!({"codexAppServer": {"grantRoot": p.get("grantRoot")}});
                let decisions = [json!("accept"), json!("acceptForSession"), json!("decline")];
                self.ask(&thread, id, call, &decisions, Shape::Decision, out);
            }
            "item/permissions/requestApproval" if known => {
                let asked = p.get("permissions").cloned().unwrap_or_else(|| json!({}));
                let mut what = Vec::new();
                if let Some(w) = asked.pointer("/fileSystem/write").and_then(Value::as_array) {
                    what.extend(w.iter().filter_map(Value::as_str).map(|p| format!("write {p}")));
                }
                if let Some(r) = asked.pointer("/fileSystem/read").and_then(Value::as_array) {
                    what.extend(r.iter().filter_map(Value::as_str).map(|p| format!("read {p}")));
                }
                if asked.pointer("/network/enabled") == Some(&json!(true)) {
                    what.push("network access".into());
                }
                let title =
                    if what.is_empty() { "More permissions".to_owned() } else { format!("Allow {}", what.join(", ")) };
                let mut call = json!({"toolCallId": item, "kind": "other", "title": title, "rawInput": asked});
                if let Some(reason) = s(p, "reason") {
                    call["content"] = json!([{"type": "content", "content": {"type": "text", "text": reason}}]);
                }
                let options = vec![
                    (json!({"optionId": "turn", "name": "Allow for this turn", "kind": "allow_once"}), json!("turn")),
                    (
                        json!({"optionId": "session", "name": "Allow for this session", "kind": "allow_always"}),
                        json!("session"),
                    ),
                    (json!({"optionId": "deny", "name": "Deny", "kind": "reject_once"}), json!("deny")),
                ];
                self.ask_with(&thread, id, call, options, json!("deny"), Shape::Permissions { asked }, out);
            }
            "item/tool/requestUserInput" if known => {
                // One question with choices maps onto permission options;
                // anything richer has no ACP form yet and is declined, so the
                // turn goes on.
                let questions = p.get("questions").and_then(Value::as_array).cloned().unwrap_or_default();
                let single = match questions.as_slice() {
                    [q] if q.get("options").and_then(Value::as_array).is_some_and(|o| !o.is_empty())
                        && q.get("isSecret") != Some(&json!(true)) =>
                    {
                        Some(q.clone())
                    }
                    _ => None,
                };
                let Some(q) = single else {
                    return out.push(Out::Server(ok(&id, json!({"answers": {}}))));
                };
                let qid = s(&q, "id").unwrap_or("q").to_owned();
                let options: Vec<(Value, Value)> = q
                    .get("options")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .enumerate()
                    .filter_map(|(i, o)| {
                        let label = s(o, "label")?;
                        Some((json!({"optionId": format!("option-{i}"), "name": label, "kind": "allow_once", "_meta": {"description": o.get("description")}}), json!(label)))
                    })
                    .collect();
                let call = json!({
                    "toolCallId": item,
                    "kind": "other",
                    "title": format!("Question: {}", s(&q, "question").unwrap_or("")),
                    "rawInput": {"questions": questions},
                    "_meta": {"codexAppServer": {"question": true, "header": q.get("header")}},
                });
                self.ask_with(&thread, id, call, options, json!(""), Shape::Question { question_id: qid }, out);
            }
            "mcpServer/elicitation/request" => {
                out.push(Out::Server(ok(&id, json!({"action": "decline"}))));
            }
            "item/tool/requestUserInput" => out.push(Out::Server(ok(&id, json!({"answers": {}})))),
            _ if !known && method.contains("requestApproval") => {
                // A thread this translator does not present: refuse.
                out.push(Out::Server(ok(&id, json!({"decision": "decline"}))));
            }
            other => {
                out.push(Out::Server(err(&id, METHOD_NOT_FOUND, format!("cmux-codex-acp does not handle {other}"))))
            }
        }
    }

    /// The tool call an approval is about, as it started.
    fn tool_call_for(&self, thread: &str, item: &str) -> Option<Value> {
        let mut call = self.threads.get(thread)?.tools.get(item)?.clone();
        if let Some(o) = call.as_object_mut() {
            o.remove("sessionUpdate");
        }
        Some(call)
    }

    fn ask(
        &mut self,
        thread: &str,
        server_id: Value,
        call: Value,
        decisions: &[Value],
        shape: Shape,
        out: &mut Vec<Out>,
    ) {
        let options: Vec<(Value, Value)> = decisions.iter().filter_map(option_for).collect();
        let on_cancel = if decisions.iter().any(|d| d == "cancel") { json!("cancel") } else { json!("decline") };
        self.ask_with(thread, server_id, call, options, on_cancel, shape, out);
    }

    #[allow(clippy::too_many_arguments)]
    fn ask_with(
        &mut self,
        thread: &str,
        server_id: Value,
        mut call: Value,
        options: Vec<(Value, Value)>,
        on_cancel: Value,
        shape: Shape,
        out: &mut Vec<Out>,
    ) {
        call["status"] = json!("pending");
        let cid = self.client_id();
        let decisions =
            options.iter().filter_map(|(o, d)| Some((o.get("optionId")?.as_str()?.to_owned(), d.clone()))).collect();
        self.asked.insert(key(&cid), Asked { thread: thread.to_owned(), server_id, decisions, on_cancel, shape });
        let params = json!({
            "sessionId": thread,
            "toolCall": call,
            "options": options.into_iter().map(|(o, _)| o).collect::<Vec<_>>(),
        });
        out.push(Out::Client(request(cid, "session/request_permission", params)));
    }
}

/// The decisions of a command approval from a server that does not list
/// them: allow, allow for the session, allow the proposed command prefix and
/// network rules when Codex proposes them, deny, deny and stop.
fn default_command_decisions(p: &Value) -> Vec<Value> {
    let mut d = vec![json!("accept"), json!("acceptForSession")];
    if let Some(prefix) = p.get("proposedExecpolicyAmendment").filter(|a| a.is_array()) {
        d.push(json!({"acceptWithExecpolicyAmendment": {"execpolicy_amendment": prefix}}));
    }
    for a in p.get("proposedNetworkPolicyAmendments").and_then(Value::as_array).into_iter().flatten() {
        d.push(json!({"applyNetworkPolicyAmendment": {"network_policy_amendment": a}}));
    }
    d.extend([json!("decline"), json!("cancel")]);
    d
}
