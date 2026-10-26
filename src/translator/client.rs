//! ACP client -> app-server.

use super::commands;
use super::*;
use crate::content;

fn s<'a>(v: &'a Value, k: &str) -> Option<&'a str> {
    v.get(k).and_then(Value::as_str)
}

impl State {
    pub(super) fn client(&mut self, msg: &Value, out: &mut Vec<Out>) {
        let method = s(msg, "method");
        let id = msg.get("id").cloned();
        let params = msg.get("params").cloned().unwrap_or(Value::Null);
        match (method, id) {
            // A response to a request we sent the client (a permission).
            (None, Some(id)) => self.client_answer(&id, msg, out),
            (Some("initialize"), Some(id)) => {
                if let Some(e) = &self.init_failed {
                    out.push(Out::Client(err(&id, INTERNAL, e.clone())));
                } else if self.initialized {
                    out.push(Out::Client(ok(&id, self.initialize_result())));
                } else {
                    self.waiting_init.push(id);
                }
            }
            (Some(_), _) if !self.initialized => {
                if let (Some(e), Some(id)) = (&self.init_failed, msg.get("id")) {
                    out.push(Out::Client(err(id, INTERNAL, e.clone())));
                } else {
                    self.queued.push(msg.clone());
                }
            }
            (Some(m), Some(id)) => self.client_request(m, id, params, out),
            (Some(m), None) => self.client_notification(m, params, out),
            (None, None) => {}
        }
    }

    pub(super) fn initialize_result(&self) -> Value {
        json!({
            "protocolVersion": 1,
            "agentCapabilities": {
                "loadSession": true,
                "promptCapabilities": {"image": true, "audio": self.audio_input(), "embeddedContext": true},
                "mcpCapabilities": {"http": true, "sse": false},
                "sessionCapabilities": {"fork": {}, "resume": {}, "list": {}, "close": {}},
            },
            "agentInfo": {"name": "cmux-codex-acp", "title": "Codex", "version": env!("CARGO_PKG_VERSION")},
            "authMethods": [],
            "_meta": {
                "steering": {"supported": true},
                "codexAppServer": {"userAgent": self.user_agent, "testedVersion": crate::CODEX_TESTED_VERSION},
            },
        })
    }

    /// Runs the client messages that waited for the server's `initialize`.
    pub(super) fn drain_queued(&mut self, out: &mut Vec<Out>) {
        for id in std::mem::take(&mut self.waiting_init) {
            out.push(Out::Client(ok(&id, self.initialize_result())));
        }
        for m in std::mem::take(&mut self.queued) {
            self.client(&m, out);
        }
    }

    fn client_request(&mut self, method: &str, id: Value, p: Value, out: &mut Vec<Out>) {
        match method {
            "authenticate" => out.push(Out::Client(ok(&id, json!({})))),
            "session/new" | "session/load" | "session/resume" | "session/fork" => {
                self.client_request_session(method, id, p, out)
            }
            "session/list" => {
                let mut params = json!({"limit": 50});
                if let Some(cwd) = p.get("cwd").filter(|c| c.is_string()) {
                    params["cwd"] = cwd.clone();
                }
                if let Some(c) = p.get("cursor").filter(|c| c.is_string()) {
                    params["cursor"] = c.clone();
                }
                out.push(self.send(Pending::ThreadList { client: id }, "thread/list", params));
            }
            "session/close" => {
                let Some(sid) = s(&p, "sessionId").map(str::to_owned) else {
                    return out.push(Out::Client(err(&id, INVALID_PARAMS, "sessionId is required")));
                };
                // A running turn ends first: its prompt is answered, its
                // approvals are withdrawn, and Codex is told to stop.
                self.cancel(&sid, out);
                let family = self.family(&sid);
                let withdrawn: Vec<String> =
                    self.asked.iter().filter(|(_, a)| family.contains(&a.thread)).map(|(k, _)| k.clone()).collect();
                for k in withdrawn {
                    if let Some(a) = self.asked.remove(&k) {
                        out.push(Out::Server(ok(&a.server_id, cancelled_answer(&a))));
                    }
                }
                for tid in &family {
                    if let Some(mut t) = self.threads.remove(tid) {
                        if let Some(prompt) = t.prompt.take() {
                            out.push(Out::Client(ok(&prompt, json!({"stopReason": "cancelled"}))));
                        }
                        for (steer, _) in t.queued_steers.drain(..) {
                            out.push(Out::Client(err(&steer, INTERNAL, crate::STEER_NO_TURN)));
                        }
                    }
                }
                out.push(self.send(Pending::Ack { client: id }, "thread/unsubscribe", json!({"threadId": sid})));
            }
            "session/prompt" => self.prompt(id, &p, out),
            "session/set_mode" => {
                let (Some(sid), Some(mode)) = (s(&p, "sessionId"), s(&p, "modeId")) else {
                    return out.push(Out::Client(err(&id, INVALID_PARAMS, "sessionId and modeId are required")));
                };
                match self.set_mode(sid, mode) {
                    Ok(()) => {
                        out.push(Out::Client(ok(&id, json!({}))));
                        out.push(update(sid, json!({"sessionUpdate": "current_mode_update", "currentModeId": mode})));
                        out.push(update(
                            sid,
                            json!({"sessionUpdate": "config_option_update", "configOptions": self.config_options(sid)}),
                        ));
                    }
                    Err(e) => out.push(Out::Client(err(&id, INVALID_PARAMS, e))),
                }
            }
            "session/set_model" => {
                let (Some(sid), Some(model)) = (s(&p, "sessionId"), s(&p, "modelId")) else {
                    return out.push(Out::Client(err(&id, INVALID_PARAMS, "sessionId and modelId are required")));
                };
                match self.set_option(sid, "model", model) {
                    Ok(()) => {
                        out.push(Out::Client(ok(&id, json!({}))));
                        out.push(update(
                            sid,
                            json!({"sessionUpdate": "config_option_update", "configOptions": self.config_options(sid)}),
                        ));
                    }
                    Err(e) => out.push(Out::Client(err(&id, INVALID_PARAMS, e))),
                }
            }
            "session/set_config_option" => {
                let (Some(sid), Some(config_id)) = (s(&p, "sessionId"), s(&p, "configId")) else {
                    return out.push(Out::Client(err(&id, INVALID_PARAMS, "sessionId and configId are required")));
                };
                let value = match p.get("value") {
                    Some(Value::String(v)) => v.clone(),
                    Some(Value::Bool(b)) => b.to_string(),
                    _ => return out.push(Out::Client(err(&id, INVALID_PARAMS, "value must be a string"))),
                };
                if config_id == commands::COLLABORATION {
                    let result = if value != commands::PLAN && value != commands::DEFAULT_COLLABORATION {
                        Err(format!("unknown collaboration mode {value:?}"))
                    } else if !self.threads.contains_key(sid) {
                        Err(format!("unknown session {sid}"))
                    } else {
                        Ok(())
                    };
                    match result {
                        // Answered once Codex took it (`collab_done`).
                        Ok(()) => self.set_collaboration(sid, &value, id, commands::CollabThen::Config, out),
                        Err(e) => out.push(Out::Client(err(&id, INVALID_PARAMS, e))),
                    }
                    return;
                }
                let result = if config_id == "mode" {
                    self.set_mode(sid, &value)
                } else {
                    self.set_option(sid, config_id, &value)
                };
                match result {
                    Ok(()) => {
                        out.push(Out::Client(ok(&id, json!({"configOptions": self.config_options(sid)}))));
                        if config_id == "mode" {
                            out.push(update(
                                sid,
                                json!({"sessionUpdate": "current_mode_update", "currentModeId": value}),
                            ));
                        }
                    }
                    Err(e) => out.push(Out::Client(err(&id, INVALID_PARAMS, e))),
                }
            }
            other => out.push(Out::Client(err(&id, METHOD_NOT_FOUND, format!("method not found: {other}")))),
        }
    }

    fn client_notification(&mut self, method: &str, p: Value, out: &mut Vec<Out>) {
        if method == "session/cancel"
            && let Some(sid) = s(&p, "sessionId")
        {
            self.cancel(sid, out);
        }
    }

    fn set_mode(&mut self, sid: &str, mode: &str) -> Result<(), String> {
        if config::mode(mode).is_none() {
            return Err(format!("unknown mode {mode:?}"));
        }
        let t = self.threads.get_mut(sid).ok_or_else(|| format!("unknown session {sid}"))?;
        t.mode = mode.to_owned();
        Ok(())
    }

    /// A model or effort pick; it applies from the next turn on (Codex takes
    /// both per turn), including a turn started while one runs.
    fn set_option(&mut self, sid: &str, config_id: &str, value: &str) -> Result<(), String> {
        let models = self.models.clone();
        let t = self.threads.get_mut(sid).ok_or_else(|| format!("unknown session {sid}"))?;
        match config_id {
            "model" => {
                let (model, effort) = config::split_model(value, &models);
                t.model = model;
                if let Some(e) = effort {
                    t.effort = e;
                }
                // An effort the new model lacks falls back to its default.
                if let Some(m) = models.iter().find(|m| m.id == t.model)
                    && t.effort != config::DEFAULT_EFFORT
                    && !m.efforts.iter().any(|(e, _)| *e == t.effort)
                {
                    t.effort = config::DEFAULT_EFFORT.into();
                }
                Ok(())
            }
            "reasoning_effort" | "effort" | "thought_level" => {
                t.effort = value.to_owned();
                Ok(())
            }
            other => Err(format!("unknown config option {other:?}")),
        }
    }

    fn thread_params(&self, p: &Value, mode: &str) -> Value {
        let m = config::mode(mode).unwrap_or(&config::MODES[2]);
        let profile = self.read_only_profile();
        let mut params = json!({"approvalPolicy": m.approval});
        if profile && m.id == "read-only" {
            params["permissions"] = json!(config::READ_ONLY_PROFILE);
        } else {
            params["sandbox"] = json!(m.sandbox);
        }
        if let Some(cwd) = p.get("cwd").filter(|c| c.is_string()) {
            params["cwd"] = cwd.clone();
        }
        let mut cfg = config::mcp_config(p.get("mcpServers")).unwrap_or_else(|| json!({}));
        if profile {
            let (key, value) = config::read_only_profile();
            cfg[key] = value;
        }
        if cfg.as_object().is_some_and(|c| !c.is_empty()) {
            params["config"] = cfg;
        }
        params
    }

    /// `_meta.cmux.generateTitle: false` on a session request: no generated title for it.
    fn wants_titles(&self, p: &Value) -> bool {
        self.opts.generate_titles && p.pointer("/_meta/cmux/generateTitle").and_then(Value::as_bool) != Some(false)
    }

    fn client_request_session(&mut self, method: &str, id: Value, p: Value, out: &mut Vec<Out>) {
        match method {
            "session/new" => self.session_new(id, &p, out),
            "session/load" => self.session_resume(id, &p, true, out),
            "session/resume" => self.session_resume(id, &p, false, out),
            _ => self.session_fork(id, &p, out),
        }
    }

    fn session_new(&mut self, id: Value, p: &Value, out: &mut Vec<Out>) {
        let mode = self.opts.default_mode.clone();
        let mut params = self.thread_params(p, &mode);
        if let Some(m) = &self.opts.model {
            params["model"] = json!(m);
        }
        let effort = self.opts.effort.clone().unwrap_or_else(|| config::DEFAULT_EFFORT.into());
        let titles = self.wants_titles(p);
        out.push(self.send(Pending::ThreadStart { client: id, mode, effort, titles }, "thread/start", params));
    }

    fn session_resume(&mut self, id: Value, p: &Value, replay: bool, out: &mut Vec<Out>) {
        let Some(sid) = s(p, "sessionId").map(str::to_owned) else {
            return out.push(Out::Client(err(&id, INVALID_PARAMS, "sessionId is required")));
        };
        let mode = self.threads.get(&sid).map(|t| t.mode.clone()).unwrap_or_else(|| self.opts.default_mode.clone());
        let mut params = self.thread_params(p, &mode);
        params["threadId"] = json!(sid);
        params["excludeTurns"] = json!(!replay);
        let titles = self.wants_titles(p);
        out.push(self.send(
            Pending::ThreadResume { client: id, session: sid, mode, replay, titles },
            "thread/resume",
            params,
        ));
    }

    fn session_fork(&mut self, id: Value, p: &Value, out: &mut Vec<Out>) {
        let Some(sid) = s(p, "sessionId").map(str::to_owned) else {
            return out.push(Out::Client(err(&id, INVALID_PARAMS, "sessionId is required")));
        };
        let mode = self.threads.get(&sid).map(|t| t.mode.clone()).unwrap_or_else(|| self.opts.default_mode.clone());
        let mut params = self.thread_params(p, &mode);
        params["threadId"] = json!(sid);
        params["excludeTurns"] = json!(true);
        if let Some(last) = p.pointer("/_meta/lastTurnId").filter(|v| v.is_string()) {
            params["lastTurnId"] = last.clone();
        }
        let titles = self.wants_titles(p);
        out.push(self.send(Pending::ThreadFork { client: id, mode, titles }, "thread/fork", params));
    }

    fn prompt(&mut self, id: Value, p: &Value, out: &mut Vec<Out>) {
        let Some(sid) = s(p, "sessionId").map(str::to_owned) else {
            return out.push(Out::Client(err(&id, INVALID_PARAMS, "sessionId is required")));
        };
        let blocks: Vec<Value> = p.get("prompt").and_then(Value::as_array).cloned().unwrap_or_default();
        let takes_audio = self.thread_takes_audio(&sid);
        let input = content::user_input(&blocks, takes_audio);
        let steer = p.pointer("/_meta/steer").and_then(Value::as_bool) == Some(true);
        let Some(t) = self.threads.get(&sid) else {
            return out.push(Out::Client(err(&id, INVALID_PARAMS, format!("unknown session {sid}"))));
        };
        // Audio for a model that does not take it is dropped. A prompt left
        // with nothing else is refused, and a prompt that is accepted tells the
        // client why it went out as text alone (a rejected one says nothing,
        // so its retry does not repeat the notice).
        let is_audio = |b: &Value| b.get("type").and_then(Value::as_str) == Some("audio");
        let dropped = !takes_audio && blocks.iter().any(is_audio);
        if dropped && blocks.iter().all(is_audio) {
            return out.push(Out::Client(err(&id, INVALID_PARAMS, crate::AUDIO_NOT_TAKEN)));
        }
        let notice = dropped.then(|| {
            update(
                &sid,
                json!({"sessionUpdate": "agent_message_chunk", "messageId": format!("audio-dropped-{id}"),
                    "content": {"type": "text", "text": crate::AUDIO_DROPPED}}),
            )
        });
        if steer {
            match (t.turn.clone(), t.starting) {
                (Some(turn), _) => {
                    out.extend(notice);
                    let params = json!({"threadId": sid, "expectedTurnId": turn, "input": input});
                    out.push(self.send(Pending::TurnSteer { client: id }, "turn/steer", params));
                }
                // The turn has no id yet: steer it once it has one.
                (None, true) => {
                    out.extend(notice);
                    self.threads.get_mut(&sid).expect("checked above").queued_steers.push((id, input));
                }
                (None, false) => out.push(Out::Client(err(&id, INTERNAL, crate::STEER_NO_TURN))),
            }
            return;
        }
        if t.turn.is_some() || t.starting || t.plan_review.is_some() || t.goal_prompt.is_some() {
            return out.push(Out::Client(err(&id, INTERNAL, "a turn is already running in this session")));
        }
        if let Some((name, rest)) = commands::parse(&blocks)
            && self.command(&id, &sid, &name, &rest, out)
        {
            return;
        }
        let t = self.threads.get(&sid).expect("checked above");
        out.extend(notice);
        let mut params = json!({"threadId": sid, "input": input});
        // Approval and sandbox only when the mode changed: the app-server
        // keeps them per thread, and a sandbox policy sent every turn would
        // drop the user's own sandbox settings.
        let mode_changed = t.mode != t.applied_mode;
        if mode_changed && let Some(mode) = config::mode(&t.mode) {
            params["approvalPolicy"] = json!(mode.approval);
            // `read-only` is a permission profile (see `config::READ_ONLY_PROFILE`).
            if mode.id == "read-only" && self.read_only_profile() {
                params["permissions"] = json!(config::READ_ONLY_PROFILE);
            } else {
                params["sandboxPolicy"] = config::sandbox_policy(mode);
            }
        }
        if !t.model.is_empty() {
            params["model"] = json!(t.model);
        }
        if t.effort != config::DEFAULT_EFFORT && !t.effort.is_empty() {
            params["effort"] = json!(t.effort);
        }
        let usage = t.usage.clone();
        let first_title = commands::title_of(&blocks);
        let t = self.threads.get_mut(&sid).expect("checked above");
        if t.title == commands::TitleSource::Unset && t.prompt_title.is_none() {
            t.prompt_title = first_title;
        }
        t.prompt_text = commands::raw_text(&blocks);
        t.command_turn = false;
        t.plan = None;
        if mode_changed {
            t.applied_mode = t.mode.clone();
        }
        t.prompt = Some(id.clone());
        t.starting = true;
        t.cancel_pending = false;
        t.usage_at_start = usage;
        out.push(self.send(Pending::TurnStart { client: id, thread: sid }, "turn/start", params));
    }

    /// Interrupts the session's turn and its subagents' turns.
    pub(super) fn cancel(&mut self, sid: &str, out: &mut Vec<Out>) {
        self.end_command_prompts(sid, "cancelled", out);
        // A plan review still open: withdraw it, and the prompt ends cancelled.
        if let Some(t) = self.threads.get_mut(sid)
            && let Some((ask, _)) = t.plan_review.take()
        {
            out.push(Out::Client(notification("$/cancel_request", json!({"requestId": ask}))));
            if let Some(prompt) = t.prompt.take() {
                out.push(Out::Client(ok(&prompt, json!({"stopReason": "cancelled"}))));
            }
        }
        for tid in self.family(sid) {
            let Some(t) = self.threads.get_mut(&tid) else { continue };
            for (steer, _) in t.queued_steers.drain(..) {
                out.push(Out::Client(err(&steer, INTERNAL, crate::STEER_NO_TURN)));
            }
            match t.turn.clone() {
                Some(turn) => {
                    let params = json!({"threadId": tid, "turnId": turn});
                    out.push(self.send(Pending::Ignore, "turn/interrupt", params));
                }
                None if t.starting => t.cancel_pending = true,
                None => {}
            }
        }
    }

    /// The client answered a permission request.
    fn client_answer(&mut self, id: &Value, msg: &Value, out: &mut Vec<Out>) {
        if self.plan_answer(id, msg, out) {
            return;
        }
        let Some(asked) = self.asked.remove(&key(id)) else { return };
        let outcome = msg.pointer("/result/outcome");
        let chosen = match outcome.and_then(|o| s(o, "outcome")) {
            Some("selected") => outcome.and_then(|o| s(o, "optionId")).map(str::to_owned),
            _ => None,
        };
        let decision = chosen
            .as_ref()
            .and_then(|c| asked.decisions.iter().find(|(o, _)| o == c).map(|(_, d)| d.clone()))
            .unwrap_or_else(|| asked.on_cancel.clone());
        let result = answer(&asked, &decision);
        out.push(Out::Server(ok(&asked.server_id, result)));
    }
}

/// The app-server answer for a permission decision.
pub(super) fn answer(asked: &Asked, decision: &Value) -> Value {
    match &asked.shape {
        Shape::Decision => json!({"decision": decision}),
        Shape::Whole => decision.clone(),
        Shape::Permissions { asked: profile } => match decision.as_str() {
            Some("session") => json!({"permissions": profile, "scope": "session"}),
            Some("turn") => json!({"permissions": profile, "scope": "turn"}),
            _ => json!({"permissions": {}, "scope": "turn"}),
        },
        Shape::Question { question_id } => {
            let answers = match decision.as_str() {
                Some(label) if !label.is_empty() => json!([label]),
                _ => json!([]),
            };
            json!({"answers": {question_id.clone(): {"answers": answers}}})
        }
    }
}

/// The answer for an approval nobody will give (a closed session).
pub(super) fn cancelled_answer(asked: &Asked) -> Value {
    answer(asked, &asked.on_cancel)
}
