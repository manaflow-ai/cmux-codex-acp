//! app-server -> ACP client: responses and notifications.

use super::commands;
use super::*;
use crate::items;

fn s<'a>(v: &'a Value, k: &str) -> Option<&'a str> {
    v.get(k).and_then(Value::as_str)
}

/// The most of a running command's output one update carries: each update
/// replaces the call's content, so a long output is sent as its tail.
const OUTPUT_TAIL: usize = 64 * 1024;

fn tail(text: &str) -> &str {
    if text.len() <= OUTPUT_TAIL {
        return text;
    }
    let mut at = text.len() - OUTPUT_TAIL;
    while !text.is_char_boundary(at) {
        at += 1;
    }
    &text[at..]
}

/// Records that `item` streamed, allocating its id only the first time.
fn mark_streamed(streamed: &mut HashSet<String>, item: &str) {
    if !streamed.contains(item) {
        streamed.insert(item.to_owned());
    }
}

fn usage_delta(start: Option<&Value>, now: Option<&Value>) -> Option<Value> {
    let now = now?.get("total")?;
    let n = |v: Option<&Value>, k: &str| v.and_then(|v| v.get(k)).and_then(Value::as_u64).unwrap_or(0);
    let before = start.and_then(|s| s.get("total"));
    let d = |k: &str| n(Some(now), k).saturating_sub(n(before, k));
    Some(json!({
        "inputTokens": d("inputTokens"),
        "outputTokens": d("outputTokens"),
        "totalTokens": d("totalTokens"),
        "cachedReadTokens": d("cachedInputTokens"),
        "thoughtTokens": d("reasoningOutputTokens"),
    }))
}

impl State {
    pub(super) fn server(&mut self, msg: &Value, out: &mut Vec<Out>) {
        match (s(msg, "method"), msg.get("id")) {
            (Some(method), Some(id)) => {
                let (method, id) = (method.to_owned(), id.clone());
                self.server_request(&method, id, msg.get("params").unwrap_or(&Value::Null), out)
            }
            (Some(method), None) => self.server_notification(method, msg.get("params").unwrap_or(&Value::Null), out),
            (None, Some(id)) => {
                let Some(pending) = self.pending.remove(&key(id)) else { return };
                match msg.get("error") {
                    Some(e) => self.server_error(pending, e, out),
                    None => self.server_result(pending, msg.get("result").unwrap_or(&Value::Null), out),
                }
            }
            (None, None) => {}
        }
    }

    fn server_error(&mut self, pending: Pending, e: &Value, out: &mut Vec<Out>) {
        let message = s(e, "message").unwrap_or("codex app-server error").to_owned();
        let code = e.get("code").and_then(Value::as_i64).unwrap_or(INTERNAL);
        match pending {
            Pending::Initialize => {
                self.init_failed = Some(format!("codex app-server initialize failed: {message}"));
                for id in std::mem::take(&mut self.waiting_init) {
                    out.push(Out::Client(err(&id, INTERNAL, self.init_failed.clone().unwrap_or_default())));
                }
            }
            Pending::ModelList | Pending::Ignore | Pending::GoalRead { .. } => {}
            Pending::ModeApply { thread, nth } => {
                // The latest update refused: the next turn sends the mode with
                // it, and work that waited for it fails. An older one refused
                // changes nothing; the newer one decides.
                let waiting = match self.threads.get_mut(&thread) {
                    Some(t) if t.mode_gen == nth => {
                        t.applied_mode.clear();
                        std::mem::take(&mut t.after_mode)
                    }
                    _ => Vec::new(),
                };
                for (pending, _, _) in waiting {
                    self.server_error(pending, e, out);
                }
            }
            Pending::TurnStart { client, thread, input } => {
                let recover = goals::busy_refusal(code, &message);
                let goal_read_pending =
                    self.pending.values().any(|p| matches!(p, Pending::GoalRead { thread: sid, .. } if sid == &thread));
                let mut cancelled = false;
                if let Some(t) = self.threads.get_mut(&thread) {
                    cancelled = std::mem::take(&mut t.cancel_pending);
                    t.prompt = None;
                    t.starting = false;
                    t.command_turn = false;
                    // The overrides may not have reached Codex: send them again.
                    t.applied_mode.clear();
                    for (steer, _) in t.queued_steers.drain(..) {
                        out.push(Out::Client(err(&steer, INTERNAL, crate::STEER_NO_TURN)));
                    }
                }
                if cancelled {
                    return out.push(Out::Client(ok(&client, json!({"stopReason": "cancelled"}))));
                }
                if recover && let Some(input) = input {
                    if self.has_active_goal(&thread) {
                        return self.reconcile_goal_send(client, thread, input, out);
                    }
                    if goal_read_pending && let Some(t) = self.threads.get_mut(&thread) {
                        t.goal_version += 1;
                        let pending = Pending::GoalSendCheck {
                            client,
                            thread: thread.clone(),
                            input,
                            version: t.goal_version,
                            error: e.clone(),
                        };
                        return out.push(self.send(pending, "thread/goal/get", json!({"threadId": thread})));
                    }
                }
                out.push(Out::Client(err(&client, code, message)));
            }
            Pending::GoalSend { client, .. } | Pending::GoalSendCheck { client, .. } => {
                out.push(Out::Client(err(&client, code, message)))
            }
            Pending::TurnSteer { client } => {
                // The turn ended (or moved on) before the steer reached it.
                let lower = message.to_lowercase();
                let no_turn = lower.contains("no active turn")
                    || lower.contains("expectedturnid")
                    || lower.contains("expected turn")
                    || lower.contains("not the active turn")
                    || lower.contains("does not match");
                let message = if no_turn { format!("{}: {message}", crate::STEER_NO_TURN) } else { message };
                out.push(Out::Client(err(&client, INTERNAL, message)));
            }
            Pending::ThreadStart { client, .. }
            | Pending::ThreadResume { client, .. }
            | Pending::ThreadFork { client, .. } => out.push(Out::Client(err(&client, code, message))),
            Pending::ThreadList { client } | Pending::Ack { client } => {
                out.push(Out::Client(err(&client, code, message)))
            }
            Pending::Command { client, thread, wait } => self.command_error(client, thread, wait, code, message, out),
        }
    }

    fn server_result(&mut self, pending: Pending, r: &Value, out: &mut Vec<Out>) {
        match pending {
            Pending::Initialize => {
                self.initialized = true;
                self.user_agent = s(r, "userAgent").map(str::to_owned);
                out.push(Out::Server(notification("initialized", json!({}))));
                out.push(self.send(Pending::ModelList, "model/list", json!({})));
                self.drain_queued(out);
            }
            Pending::ModeApply { thread, .. } => {
                if self.mode_settled(&thread) {
                    let waiting = self.threads.get_mut(&thread).map(|t| std::mem::take(&mut t.after_mode));
                    for (pending, method, params) in waiting.unwrap_or_default() {
                        out.push(self.send(pending, &method, params));
                    }
                }
            }
            Pending::ModelList => {
                self.models = config::parse_models(r);
                let default = self.models.iter().find(|m| m.is_default).map(|m| m.id.clone());
                for sid in self.threads.keys().cloned().collect::<Vec<_>>() {
                    if let (Some(t), Some(d)) = (self.threads.get_mut(&sid), &default)
                        && t.model.is_empty()
                    {
                        t.model = d.clone();
                    }
                }
                for sid in self.roots() {
                    out.push(update(
                        &sid,
                        json!({"sessionUpdate": "config_option_update", "configOptions": self.config_options(&sid)}),
                    ));
                }
            }
            Pending::ThreadStart { client, mode, effort, titles } => match self.adopt_thread(r, mode, Some(effort)) {
                Some(sid) => {
                    self.settle_read_only(&sid, r, out);
                    self.title_choice(&sid, titles);
                    let extra = json!({"_meta": {"codexAppServer": {"threadPath": r.pointer("/thread/path"), "cliVersion": r.pointer("/thread/cliVersion"), "model": r.get("model")}}});
                    out.push(Out::Client(ok(&client, self.session_result(&sid, extra))));
                    self.publish_commands(&sid, out);
                    self.read_goal(&sid, out);
                }
                None => out.push(Out::Client(err(&client, INTERNAL, "thread/start returned no thread"))),
            },
            Pending::ThreadResume { client, session, mode, replay, titles } => {
                let effort = self.threads.get(&session).map(|t| t.effort.clone());
                // A mode picked while the resume was on its way stays the
                // session's, and its own update settles it.
                let picked = self
                    .threads
                    .get(&session)
                    .filter(|t| t.mode != mode || self.mode_on_its_way(&session))
                    .map(|t| (t.mode.clone(), t.applied_mode.clone()));
                let Some(sid) = self.adopt_thread(r, mode, effort) else {
                    return out.push(Out::Client(err(&client, INTERNAL, "thread/resume returned no thread")));
                };
                if let Some((mode, applied)) = picked.filter(|_| sid == session)
                    && let Some(t) = self.threads.get_mut(&sid)
                {
                    (t.mode, t.applied_mode) = (mode, applied);
                } else {
                    self.settle_read_only(&sid, r, out);
                }
                self.title_choice(&sid, titles);
                if replay {
                    self.replay(&sid, r.pointer("/thread/turns"), out);
                    self.history_title(&sid, r.get("thread").unwrap_or(&Value::Null), out);
                } else if let Some(t) = self.threads.get_mut(&sid) {
                    // Resumed without its history: its title stays the client's,
                    // unless the thread has a name of its own.
                    if t.title == commands::TitleSource::Unset {
                        t.title = commands::TitleSource::Unknown;
                    }
                    if let Some(name) =
                        r.pointer("/thread/name").and_then(Value::as_str).filter(|n| !n.trim().is_empty())
                    {
                        t.title = commands::TitleSource::Explicit;
                        out.push(update(&sid, json!({"sessionUpdate": "session_info_update", "title": name.split_whitespace().collect::<Vec<_>>().join(" ")})));
                    }
                }
                // A thread resumed mid-turn (still live in this app-server).
                if let Some(turn) = r
                    .pointer("/thread/turns")
                    .and_then(Value::as_array)
                    .and_then(|t| t.last())
                    .filter(|t| s(t, "status") == Some("inProgress"))
                    .and_then(|t| s(t, "id"))
                    && let Some(t) = self.threads.get_mut(&sid)
                {
                    t.turn = Some(turn.to_owned());
                }
                out.push(Out::Client(ok(&client, self.session_result(&sid, json!({})))));
                self.publish_commands(&sid, out);
                self.read_goal(&sid, out);
            }
            Pending::ThreadFork { client, mode, titles } => match self.adopt_thread(r, mode, None) {
                Some(sid) => {
                    self.settle_read_only(&sid, r, out);
                    self.title_choice(&sid, titles);
                    out.push(Out::Client(ok(&client, self.session_result(&sid, json!({})))));
                    self.publish_commands(&sid, out);
                    self.read_goal(&sid, out);
                }
                None => out.push(Out::Client(err(&client, INTERNAL, "thread/fork returned no thread"))),
            },
            Pending::ThreadList { client } => {
                let sessions: Vec<Value> = r
                    .get("data")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .map(|t| {
                        let title = s(t, "name").filter(|n| !n.is_empty()).or(s(t, "preview"));
                        json!({
                            "sessionId": t.get("id"),
                            "cwd": t.get("cwd"),
                            "title": title,
                            "updatedAt": t.get("updatedAt").and_then(Value::as_i64).map(rfc3339),
                        })
                    })
                    .collect();
                out.push(Out::Client(ok(&client, json!({"sessions": sessions, "nextCursor": r.get("nextCursor")}))));
            }
            Pending::TurnStart { client: _, thread, .. } => {
                let turn = r.pointer("/turn/id").and_then(Value::as_str).map(str::to_owned);
                let Some(t) = self.threads.get_mut(&thread) else { return };
                // The turn may have started and ended before this answer: then
                // `turn/completed` already answered the prompt and cleared
                // `starting`, and the finished turn must not be taken as running.
                if !std::mem::take(&mut t.starting) {
                    return;
                }
                if t.turn.is_none() {
                    t.turn = turn;
                }
                let steers = std::mem::take(&mut t.queued_steers);
                let cancel = std::mem::take(&mut t.cancel_pending);
                let Some(turn) = t.turn.clone() else { return };
                if cancel {
                    for (steer, _) in steers {
                        out.push(Out::Client(err(&steer, INTERNAL, crate::STEER_NO_TURN)));
                    }
                    out.push(self.send(Pending::Ignore, "turn/interrupt", json!({"threadId": thread, "turnId": turn})));
                    return;
                }
                for (steer, input) in steers {
                    let params = json!({"threadId": thread, "expectedTurnId": turn, "input": input});
                    out.push(self.send(Pending::TurnSteer { client: steer }, "turn/steer", params));
                }
            }
            Pending::TurnSteer { client } => out.push(Out::Client(ok(&client, json!({"stopReason": "steered"})))),
            Pending::Ack { client } => out.push(Out::Client(ok(&client, json!({})))),
            Pending::Command { client, thread, wait } => self.command_result(client, thread, wait, r, out),
            Pending::GoalRead { thread, version } => {
                if self.threads.get(&thread).is_some_and(|t| t.goal_version == version) {
                    self.goal_update(&thread, r.get("goal"), true, out);
                }
            }
            Pending::GoalSend { client, thread, input } => {
                if self.has_active_goal(&thread) {
                    self.steer_goal_send(client, &thread, input, &r["thread"], out);
                } else {
                    out.push(Out::Client(err(&client, INTERNAL, crate::STEER_NO_TURN)));
                }
            }
            Pending::GoalSendCheck { client, thread, input, version, error } => {
                if self.threads.get(&thread).is_some_and(|t| t.goal_version == version) {
                    self.goal_update(&thread, r.get("goal"), false, out);
                }
                if self.has_active_goal(&thread) {
                    self.reconcile_goal_send(client, thread, input, out);
                } else {
                    self.server_error(Pending::GoalSend { client, thread, input }, &error, out);
                }
            }
            Pending::Ignore => {}
        }
    }

    fn server_notification(&mut self, method: &str, p: &Value, out: &mut Vec<Out>) {
        // Borrowed from the line: no copy per streamed delta.
        let thread = s(p, "threadId");
        let known = thread.is_some_and(|t| self.threads.contains_key(t));
        match method {
            "account/rateLimits/updated" => return self.rate_limits(p, out),
            // A subagent thread whose parent this translator presents.
            "thread/started" => return self.thread_started(p.get("thread").unwrap_or(&Value::Null), out),
            _ if !known && self.title_line(method, p, out) => return,
            // Skills changed on disk (no thread): every session's `$skill` list again.
            "skills/changed" => {
                for sid in self.roots() {
                    self.publish_commands(&sid, out);
                }
                return;
            }
            _ if !known => return,
            _ => {}
        }
        let sid = thread.expect("known thread");
        match method {
            "turn/started" => {
                // The cancelled command turn, started after all: stop it, and keep
                // it apart from a later prompt's turn.
                if let (Some(t), Some(turn)) =
                    (self.threads.get_mut(sid), p.pointer("/turn/id").and_then(Value::as_str))
                    && t.stale_command_turn
                    && t.turn.is_none()
                    && !t.command_turn
                    && !t.starting
                {
                    t.stale_command_turn = false;
                    t.ignore_turn = Some(turn.to_owned());
                    let params = json!({"threadId": sid, "turnId": turn});
                    out.push(self.send(Pending::Ignore, "turn/interrupt", params));
                    return;
                }
                if let (Some(t), Some(turn)) =
                    (self.threads.get_mut(sid), p.pointer("/turn/id").and_then(Value::as_str))
                {
                    t.turn = Some(turn.to_owned());
                    t.starting = false;
                    // A command's turn (`/compact`, `/goal`) gets its id only
                    // here: a cancel that came before it stops it now, and steers
                    // that came before it reach it now.
                    if t.command_turn {
                        let steers = std::mem::take(&mut t.queued_steers);
                        if std::mem::take(&mut t.cancel_pending) {
                            for (steer, _) in steers {
                                out.push(Out::Client(err(&steer, INTERNAL, crate::STEER_NO_TURN)));
                            }
                            let params = json!({"threadId": sid, "turnId": turn});
                            out.push(self.send(Pending::Ignore, "turn/interrupt", params));
                        } else {
                            for (steer, input) in steers {
                                let params = json!({"threadId": sid, "expectedTurnId": turn, "input": input});
                                out.push(self.send(Pending::TurnSteer { client: steer }, "turn/steer", params));
                            }
                        }
                    }
                }
            }
            "thread/compacted" => {
                out.push(update(sid, json!({"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": "*Context compacted to fit the model's context window.*\n\n"}})));
                self.compacted(sid, out);
            }
            "thread/settings/updated" => {
                let mode = p.pointer("/threadSettings/collaborationMode/mode").and_then(Value::as_str);
                if let (Some(t), Some(mode)) = (self.threads.get_mut(sid), mode)
                    && !t.collab_pending
                    && t.collab.as_deref() != Some(mode)
                {
                    t.collab = Some(mode.to_owned());
                    out.push(update(
                        sid,
                        json!({"sessionUpdate": "config_option_update", "configOptions": self.config_options(sid)}),
                    ));
                }
            }
            "thread/goal/updated" => {
                self.goal_update(sid, p.get("goal"), false, out);
                self.goal_changed(sid, out);
            }
            "thread/goal/cleared" => {
                self.goal_update(sid, None, false, out);
                self.goal_changed(sid, out);
            }
            "turn/completed" => self.turn_completed(sid, p.get("turn").unwrap_or(&Value::Null), out),
            "item/started" => {
                if let Some(item) = p.get("item") {
                    self.item(sid, item, false, out);
                }
            }
            "item/completed" => {
                if let Some(item) = p.get("item") {
                    self.item(sid, item, true, out);
                }
            }
            "item/agentMessage/delta" => {
                let (Some(item), Some(delta)) = (s(p, "itemId"), s(p, "delta")) else { return };
                if let Some(t) = self.threads.get_mut(sid) {
                    mark_streamed(&mut t.streamed, item);
                }
                out.push(update(sid, json!({"sessionUpdate": "agent_message_chunk", "messageId": item, "content": {"type": "text", "text": delta}})));
            }
            "item/reasoning/summaryTextDelta" | "item/reasoning/textDelta" => {
                let (Some(item), Some(delta)) = (s(p, "itemId"), s(p, "delta")) else { return };
                if let Some(t) = self.threads.get_mut(sid) {
                    mark_streamed(&mut t.streamed, item);
                }
                out.push(update(
                    sid,
                    json!({"sessionUpdate": "agent_thought_chunk", "content": {"type": "text", "text": delta}}),
                ));
            }
            "item/reasoning/summaryPartAdded" => {
                if p.get("summaryIndex").and_then(Value::as_u64).unwrap_or(0) > 0 {
                    out.push(update(
                        sid,
                        json!({"sessionUpdate": "agent_thought_chunk", "content": {"type": "text", "text": "\n\n"}}),
                    ));
                }
            }
            "item/commandExecution/outputDelta" => {
                let (Some(item), Some(delta)) = (s(p, "itemId"), s(p, "delta")) else { return };
                let Some(t) = self.threads.get_mut(sid) else { return };
                if !t.output.contains_key(item) {
                    t.output.insert(item.to_owned(), Default::default());
                }
                let Some((buf, unsent)) = t.output.get_mut(item) else { return };
                buf.push_str(delta);
                *unsent += delta.len();
                // Keep only the tail a client is shown.
                if buf.len() > 2 * OUTPUT_TAIL {
                    *buf = tail(buf).to_owned();
                }
                let mut u = json!({
                    "sessionUpdate": "tool_call_update",
                    "toolCallId": item,
                    "_meta": {"terminal_output_delta": {"terminal_id": item, "data": delta}},
                });
                // The content (which replaces the call's whole output) goes
                // with a delta once the new bytes are an eighth of what it
                // carries, so the bytes sent stay linear in the output; every
                // delta still goes out at once in `terminal_output_delta`.
                let shown = tail(buf);
                if shown.len() <= 4096 || *unsent * 8 >= shown.len() {
                    u["content"] = json!([{"type": "content", "content": {"type": "text", "text": shown}}]);
                    *unsent = 0;
                }
                out.push(update(sid, u));
            }
            "item/fileChange/patchUpdated" => {
                let Some(item) = s(p, "itemId") else { return };
                let fake =
                    json!({"type": "fileChange", "id": item, "changes": p.get("changes"), "status": "inProgress"});
                let diffs = items::file_diffs(&fake);
                if let Some(mut fields) = items::tool_fields(&fake, false, Some(&diffs)) {
                    if let Some(t) = self.threads.get_mut(sid) {
                        t.tools.insert(item.to_owned(), fields.clone());
                    }
                    fields["sessionUpdate"] = json!("tool_call_update");
                    out.push(update(sid, fields));
                }
            }
            "turn/plan/updated" => {
                let entries: Vec<Value> = p
                    .get("plan")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .map(|step| {
                        let status = match s(step, "status") {
                            Some("completed") => "completed",
                            Some("inProgress") => "in_progress",
                            _ => "pending",
                        };
                        json!({"content": step.get("step"), "priority": "medium", "status": status})
                    })
                    .collect();
                let mut u = json!({"sessionUpdate": "plan", "entries": entries});
                if let Some(e) = p.get("explanation").filter(|e| e.is_string()) {
                    u["_meta"] = json!({"codexAppServer": {"explanation": e}});
                }
                out.push(update(sid, u));
            }
            "thread/tokenUsage/updated" => {
                let usage = p.get("tokenUsage").cloned().unwrap_or(Value::Null);
                let used = usage.pointer("/last/totalTokens").and_then(Value::as_u64).unwrap_or(0);
                let size = usage.get("modelContextWindow").and_then(Value::as_u64).unwrap_or(0);
                if let Some(t) = self.threads.get_mut(sid) {
                    t.usage = Some(usage.clone());
                }
                out.push(update(sid, json!({"sessionUpdate": "usage_update", "used": used, "size": size, "_meta": {"codexAppServer": {"tokenUsage": usage}}})));
            }
            "thread/name/updated" => {
                if let Some(t) = self.threads.get_mut(sid) {
                    t.title = if p.get("threadName").is_some_and(|n| !n.is_null()) {
                        commands::TitleSource::Explicit
                    } else {
                        commands::TitleSource::Unset
                    };
                }
                out.push(update(sid, json!({"sessionUpdate": "session_info_update", "title": p.get("threadName").cloned().unwrap_or(Value::Null)})));
            }
            "model/rerouted" => {
                if let (Some(t), Some(to)) = (self.threads.get_mut(sid), s(p, "toModel")) {
                    t.model = to.to_owned();
                }
                out.push(update(
                    sid,
                    json!({"sessionUpdate": "config_option_update", "configOptions": self.config_options(sid)}),
                ));
            }
            "serverRequest/resolved" => {
                // Answered or withdrawn on the server side (an interrupt): a
                // late client answer has nothing to answer.
                if let Some(rid) = p.get("requestId") {
                    let gone: Vec<String> =
                        self.asked.iter().filter(|(_, a)| a.server_id == *rid).map(|(k, _)| k.clone()).collect();
                    for k in gone {
                        self.asked.remove(&k);
                        let id: Value = serde_json::from_str(&k).unwrap_or(Value::String(k));
                        out.push(Out::Client(notification("$/cancel_request", json!({"requestId": id}))));
                    }
                }
            }
            _ => {}
        }
    }

    fn turn_completed(&mut self, sid: &str, turn: &Value, out: &mut Vec<Out>) {
        let Some(t) = self.threads.get_mut(sid) else { return };
        let turn_id = s(turn, "id");
        if turn_id.is_some() && t.ignore_turn.as_deref() == turn_id {
            t.ignore_turn = None;
            return;
        }
        if t.turn.as_deref().is_some_and(|cur| Some(cur) != turn_id) {
            return;
        }
        t.turn = None;
        t.starting = false;
        let steers = std::mem::take(&mut t.queued_steers);
        t.output.clear();
        t.tools.clear();
        t.streamed.clear();
        t.compacting = false;
        let command_turn = std::mem::take(&mut t.command_turn);
        for (steer, _) in steers {
            out.push(Out::Client(err(&steer, INTERNAL, crate::STEER_NO_TURN)));
        }
        // A plan-mode turn that made a plan asks whether to implement it; the
        // prompt waits for the answer.
        if s(turn, "status") == Some("completed") && self.ask_plan(sid, out) {
            return;
        }
        let Some(t) = self.threads.get_mut(sid) else { return };
        t.plan = None;
        let Some(prompt) = t.prompt.take() else { return };
        if !command_turn && s(turn, "status") == Some("completed") {
            let text = self.threads.get(sid).and_then(|t| t.prompt_text.clone());
            self.fallback_title(sid, out);
            self.generate_title(sid, text, out);
        }
        let t = self.threads.get(sid).expect("checked above");
        let usage = usage_delta(t.usage_at_start.as_ref(), t.usage.as_ref());
        match s(turn, "status") {
            Some("failed") => {
                let message = turn.pointer("/error/message").and_then(Value::as_str).unwrap_or("the turn failed");
                let mut e = err(&prompt, INTERNAL, message);
                e["error"]["data"] = json!({"codexErrorInfo": turn.pointer("/error/codexErrorInfo"), "additionalDetails": turn.pointer("/error/additionalDetails")});
                out.push(Out::Client(e));
            }
            status => {
                let stop = if status == Some("interrupted") { "cancelled" } else { "end_turn" };
                let mut r = json!({"stopReason": stop, "_meta": {"codexAppServer": {"turnId": turn_id, "durationMs": turn.get("durationMs")}}});
                if let Some(u) = usage {
                    r["usage"] = u;
                }
                out.push(Out::Client(ok(&prompt, r)));
            }
        }
    }

    /// One item started or completed.
    pub(super) fn item(&mut self, sid: &str, item: &Value, done: bool, out: &mut Vec<Out>) {
        let Some(id) = s(item, "id").map(str::to_owned) else { return };
        match s(item, "type").unwrap_or("") {
            "userMessage" | "hookPrompt" | "functionCallOutput" | "enteredReviewMode" => {}
            "contextCompaction" => {
                let meta = json!({"contextCompaction": {"version": 1}});
                if done {
                    out.push(update(sid, json!({"sessionUpdate": "tool_call_update", "toolCallId": id, "title": "Compact conversation", "status": "completed", "_meta": meta})));
                    self.compacted(sid, out);
                } else {
                    out.push(update(sid, json!({"sessionUpdate": "tool_call", "toolCallId": id, "kind": "think", "title": "Compact conversation", "status": "in_progress", "_meta": meta})));
                }
            }
            "exitedReviewMode" => {
                if let Some(review) = s(item, "review").map(str::trim).filter(|r| !r.is_empty() && done) {
                    out.push(update(
                        sid,
                        json!({"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": review}}),
                    ));
                }
            }
            "agentMessage" | "plan" => {
                if !done {
                    return;
                }
                if s(item, "type") == Some("plan")
                    && let Some(text) = s(item, "text").filter(|t| !t.is_empty())
                    && let Some(t) = self.threads.get_mut(sid)
                {
                    t.plan = Some((id.clone(), text.to_owned()));
                }
                let streamed = self.threads.get_mut(sid).is_some_and(|t| t.streamed.remove(&id));
                if let Some(text) = s(item, "text").filter(|t| !t.is_empty() && !streamed) {
                    out.push(update(sid, json!({"sessionUpdate": "agent_message_chunk", "messageId": id, "content": {"type": "text", "text": text}})));
                }
            }
            "reasoning" => {
                if !done {
                    return;
                }
                let streamed = self.threads.get_mut(sid).is_some_and(|t| t.streamed.remove(&id));
                let summary: Vec<&str> = item
                    .get("summary")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .filter_map(Value::as_str)
                    .collect();
                if !streamed && !summary.is_empty() {
                    out.push(update(sid, json!({"sessionUpdate": "agent_thought_chunk", "content": {"type": "text", "text": summary.join("\n\n")}})));
                }
            }
            "subAgentActivity" => self.subagent_activity(sid, item, out),
            kind => {
                if kind == "collabAgentToolCall" {
                    self.collab(sid, item, out);
                }
                self.tool_item(sid, &id, item, done, out);
            }
        }
    }

    fn tool_item(&mut self, sid: &str, id: &str, item: &Value, done: bool, out: &mut Vec<Out>) {
        let Some(t) = self.threads.get_mut(sid) else { return };
        if !done {
            let diffs = (s(item, "type") == Some("fileChange")).then(|| items::file_diffs(item));
            let Some(mut fields) = items::tool_fields(item, false, diffs.as_deref()) else { return };
            t.tools.insert(id.to_owned(), fields.clone());
            fields["sessionUpdate"] = json!("tool_call");
            return out.push(update(sid, fields));
        }
        let started = t.tools.remove(id);
        t.output.remove(id);
        // The diffs computed at start: the file has changed since.
        let diffs: Option<Vec<Value>> = started
            .as_ref()
            .and_then(|f| f.get("content"))
            .and_then(Value::as_array)
            .cloned()
            .filter(|_| s(item, "type") == Some("fileChange"));
        let diffs = diffs.or_else(|| (s(item, "type") == Some("fileChange")).then(|| items::file_diffs(item)));
        let Some(mut fields) = items::tool_fields(item, true, diffs.as_deref()) else { return };
        // A call nobody saw start (no item/started) still needs its call.
        fields["sessionUpdate"] = json!(if started.is_some() { "tool_call_update" } else { "tool_call" });
        out.push(update(sid, fields));
    }

    /// Subagent start and end from a `subAgentActivity` item (multi-agent
    /// v2): the child thread's id is its ACP session id.
    pub(super) fn subagent_activity(&mut self, sid: &str, item: &Value, out: &mut Vec<Out>) {
        let Some(child) = s(item, "agentThreadId").map(str::to_owned) else { return };
        let path = s(item, "agentPath").unwrap_or("");
        let name = path.rsplit('/').find(|p| !p.is_empty()).unwrap_or("agent").to_owned();
        let state = match s(item, "kind") {
            Some("completed") => "completed",
            Some("interrupted") => "cancelled",
            _ => "running",
        };
        let meta = json!({"codexAppServer": {"agentPath": path, "model": item.get("model"), "reasoningEffort": item.get("reasoningEffort")}});
        self.subagent(sid, &child, &name, None, state, meta, None, out);
    }

    /// Subagents a collaboration call names (multi-agent v1 `spawnAgent`,
    /// and agent states in later calls).
    fn collab(&mut self, sid: &str, item: &Value, out: &mut Vec<Out>) {
        let receivers: Vec<String> = item
            .get("receiverThreadIds")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|v| v.as_str().map(str::to_owned))
            .collect();
        let spawn = s(item, "tool") == Some("spawnAgent");
        for child in receivers {
            let status = item.pointer(&format!("/agentsStates/{child}/status")).and_then(Value::as_str);
            let state = match status {
                Some("completed") => "completed",
                Some("interrupted") => "cancelled",
                Some("errored" | "shutdown" | "notFound") => "failed",
                _ => "running",
            };
            if !spawn && !self.threads.contains_key(&child) {
                continue;
            }
            let task = s(item, "prompt").map(str::to_owned);
            let meta =
                json!({"codexAppServer": {"model": item.get("model"), "reasoningEffort": item.get("reasoningEffort")}});
            self.subagent(sid, &child, "agent", task, state, meta, s(item, "id"), out);
        }
    }

    /// Records a subagent and reports its start or a change of its state,
    /// once per change, under the parent's session.
    #[allow(clippy::too_many_arguments)]
    fn subagent(
        &mut self,
        parent: &str,
        child: &str,
        name: &str,
        task: Option<String>,
        state: &'static str,
        meta: Value,
        tool_call: Option<&str>,
        out: &mut Vec<Out>,
    ) {
        let parent_model = self.threads.get(parent).map(|t| (t.mode.clone(), t.model.clone(), t.effort.clone()));
        let fresh = !self.threads.contains_key(child);
        let t = self.threads.entry(child.to_owned()).or_default();
        if fresh {
            t.parent = Some(parent.to_owned());
            if let Some((mode, model, effort)) = parent_model {
                t.mode = mode;
                t.model = model;
                t.effort = effort;
            }
            t.sub_state = Some("running");
            let mut u = json!({
                "sessionUpdate": "subagent_spawned",
                "subagentSessionId": child,
                "name": name,
                "task": task.clone().unwrap_or_else(|| name.to_owned()),
                "capabilities": {},
                "_meta": meta,
            });
            if let Some(task) = task {
                u["prompt"] = json!(task);
            }
            if let Some(call) = tool_call {
                u["_meta"]["codexAppServer"]["toolCallId"] = json!(call);
            }
            out.push(update(parent, u));
            if state == "running" {
                return;
            }
        }
        let t = self.threads.get_mut(child).expect("inserted above");
        if t.sub_state == Some(state) {
            return;
        }
        t.sub_state = Some(state);
        out.push(update(
            parent,
            json!({"sessionUpdate": "subagent_state_update", "subagentSessionId": child, "state": state}),
        ));
    }

    /// A thread started with a parent this translator presents: a subagent
    /// (multi-agent runs that announce children this way).
    fn thread_started(&mut self, thread: &Value, out: &mut Vec<Out>) {
        let (Some(id), Some(parent)) = (s(thread, "id"), s(thread, "parentThreadId")) else { return };
        if self.threads.contains_key(id) || !self.threads.contains_key(parent) {
            return;
        }
        let name = s(thread, "agentNickname").or(s(thread, "agentRole")).unwrap_or("agent").to_owned();
        let meta = json!({"codexAppServer": {"agentRole": thread.get("agentRole"), "model": thread.get("model")}});
        let (id, parent) = (id.to_owned(), parent.to_owned());
        self.subagent(&parent, &id, &name, None, "running", meta, None, out);
    }

    fn rate_limits(&mut self, p: &Value, out: &mut Vec<Out>) {
        let Some(Value::Object(fresh)) = p.get("rateLimits") else { return };
        let mut merged = self.rate_limits.clone().unwrap_or_else(|| json!({}));
        if let Value::Object(m) = &mut merged {
            for (k, v) in fresh {
                if !v.is_null() {
                    m.insert(k.clone(), v.clone());
                }
            }
        }
        if self.rate_limits.as_ref() == Some(&merged) {
            return;
        }
        self.rate_limits = Some(merged.clone());
        for sid in self.roots() {
            out.push(update(
                &sid,
                json!({"sessionUpdate": "session_info_update", "_meta": {"codexAppServer": {"rateLimits": merged}}}),
            ));
        }
    }
}

/// Unix seconds as an RFC 3339 UTC time.
pub(super) fn rfc3339(secs: i64) -> String {
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    // Civil date from days since 1970-01-01 (Howard Hinnant's algorithm).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z", rem / 3600, rem % 3600 / 60, rem % 60)
}
