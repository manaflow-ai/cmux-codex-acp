//! Prompt suggestions: after a session's turn completes, a predicted next
//! message from the person, which an ACP client can show as ghost text in an
//! empty composer (Codex's own desktop app and CLI show one there too).
//!
//! The client sees one `session/update`:
//! `{"sessionUpdate": "prompt_suggestion", "suggestion": "<text>"}`, and the
//! same with `"suggestion": null` when it no longer applies (the next prompt
//! started, or the session turned suggestions off). Nothing of how it was
//! made ever reaches the session: no item, usage, title or request.
//!
//! [`Options::prompt_suggestions`] is the global switch. While it is on,
//! each session has the `prompt-suggestions` config option (`on`/`off`, on
//! by default); off, that session asks for none. A suggestion is asked for
//! only after a prompt's own turn completes in a root session: never after
//! an interrupted or failed turn, a command or goal turn, or in a subagent.
//! One whose source turn is no longer the session's latest is dropped.
//!
//! How Codex is asked ([`Route`]): the first route is
//! `thread/prediction/request` with `thread/prediction/updated`, Codex's own
//! prediction API (declared since 0.163, answered -32601 "not implemented
//! yet" by 0.164). On -32601 the connection moves to `thread/sample`: one
//! model response on a private, cache-sharing fork of the thread that Codex
//! never lets act (a tool call samples as null) and never streams to us. An
//! app-server without that moves to the last route: an ephemeral
//! `thread/fork` in prediction mode (the parent's own config and history, so
//! its prompt cache), one low-effort turn on it, its last agent message, then
//! `thread/unsubscribe`; its lines never reach the session, and a tool it
//! starts interrupts it. So each connection settles on the best route its
//! app-server has, and moves up by itself once Codex implements the API.

use super::*;

/// The config option's id (the same id acpmux's Claude Code sessions use).
pub(super) const OPTION: &str = "prompt-suggestions";

/// What a prediction asks for. The model sees the whole thread before it.
const INSTRUCTIONS: &str = "Stop working on the task. Do not use any tools. Instead, write the one message the user is most likely to send you next, exactly as they would type it.

Base it only on what the user has asked for so far. It must read like the user's own words, not advice or a summary from you. Good predictions are short, concrete next steps: \"run the tests\" after a fix, \"commit it\" once the work is done, the option the user would pick when you offered choices, or \"yes\" when you asked whether to go on.

Reply with nothing at all if the next step is unclear, if the last turn failed or misunderstood the user, or if the message would involve credentials, secrets, private data or anything sensitive. Never predict thanks or praise, a question, an idea the user did not raise, or text in your own voice.

Match the user's language and tone. One line of 2 to 12 words, with no quotes, no markdown and no explanation.";

/// How suggestions are asked of this app-server; see the module docs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(super) enum Route {
    #[default]
    Prediction,
    Sample,
    Fork,
    /// The app-server has none of them: no more asks on this connection.
    Unavailable,
}

/// A prediction fork on its way: its session and source turn.
#[derive(Debug, Clone)]
pub(super) struct PredictionFork {
    session: String,
    turn: String,
    /// The fork's own turn, once it started.
    fork_turn: Option<String>,
    /// It started a tool and was interrupted: its answer is not used.
    spoiled: bool,
    /// Agent messages it completed.
    text: Vec<String>,
}

/// The `session/update` for `suggestion` (`None` clears it).
pub(super) fn update_value(suggestion: Option<&str>) -> Value {
    json!({"sessionUpdate": "prompt_suggestion", "suggestion": suggestion})
}

impl State {
    /// The session's `prompt-suggestions` option, while the global switch is on.
    pub(super) fn suggestion_option(&self, thread: &str) -> Option<Value> {
        if !self.opts.prompt_suggestions {
            return None;
        }
        let on = self.threads.get(thread).is_none_or(|t| !t.suggestions_off);
        Some(json!({"id": OPTION, "name": "Prompt suggestions", "type": "select", "category": "model_config",
        "currentValue": if on { "on" } else { "off" },
        "options": [
            {"value": "off", "name": "Off"},
            {"value": "on", "name": "On", "description": "Suggests your next prompt after a turn; one extra model call per turn, which counts toward usage"},
        ]}))
    }

    /// `session/set_config_option` of [`OPTION`].
    pub(super) fn set_suggestions(&mut self, sid: &str, value: &str, out: &mut Vec<Out>) -> Result<(), String> {
        let on = match value {
            "on" | "true" => true,
            "off" | "false" => false,
            _ => return Err(format!("{OPTION} must be on or off")),
        };
        if !self.opts.prompt_suggestions {
            return Err("prompt suggestions are off in settings".into());
        }
        let t = self.threads.get_mut(sid).ok_or_else(|| format!("unknown session {sid}"))?;
        t.suggestions_off = !on;
        if !on {
            t.suggest_turn = None;
            self.clear_suggestion(sid, out);
        }
        Ok(())
    }

    /// The shown suggestion no longer applies (a prompt started, or the session
    /// turned them off): the client clears it, and one on its way is dropped.
    pub(super) fn clear_suggestion(&mut self, sid: &str, out: &mut Vec<Out>) {
        let Some(t) = self.threads.get_mut(sid) else { return };
        t.suggest_turn = None;
        if std::mem::take(&mut t.suggestion_shown) {
            out.push(update(sid, update_value(None)));
        }
        // A fork still predicting for it stops: its answer would be dropped.
        let running: Vec<(String, String)> = self
            .prediction_forks
            .iter_mut()
            .filter(|(_, f)| f.session == sid && !f.spoiled)
            .filter_map(|(fork, f)| {
                f.spoiled = true;
                Some((fork.clone(), f.fork_turn.clone()?))
            })
            .collect();
        for (fork, turn) in running {
            out.push(self.send(Pending::Ignore, "turn/interrupt", json!({"threadId": fork, "turnId": turn})));
        }
    }

    /// After a prompt's turn `turn` completed in `sid`: ask for a suggestion
    /// when this session takes them.
    pub(super) fn suggest(&mut self, sid: &str, turn: Option<&str>, out: &mut Vec<Out>) {
        let Some(turn) = turn.filter(|t| !t.is_empty()) else { return };
        if !self.opts.prompt_suggestions || self.suggestion_route == Route::Unavailable || self.has_active_goal(sid) {
            return;
        }
        let Some(t) = self.threads.get_mut(sid) else { return };
        if t.parent.is_some() || t.suggestions_off {
            return;
        }
        t.suggest_turn = Some(turn.to_owned());
        self.ask_suggestion(sid, turn, out);
    }

    /// Sends the ask on this connection's route.
    fn ask_suggestion(&mut self, sid: &str, turn: &str, out: &mut Vec<Out>) {
        let route = self.suggestion_route;
        let pending = Pending::Suggest { thread: sid.to_owned(), turn: turn.to_owned(), route };
        let (method, params) = match route {
            // Switches to Codex's own prediction API by itself: the first ask on
            // each connection tries it, and only its -32601 moves on.
            Route::Prediction => ("thread/prediction/request", json!({"threadId": sid, "sourceTurnId": turn})),
            Route::Sample => (
                "thread/sample",
                json!({"threadId": sid, "expectedTurnId": turn, "prompt": INSTRUCTIONS, "effort": "low", "disableMcp": true}),
            ),
            Route::Fork => (
                "thread/fork",
                json!({"threadId": sid, "ephemeral": true, "experimentalPredictionMode": true,
                    "experimentalDisablePredictionMcp": true, "excludeTurns": true}),
            ),
            Route::Unavailable => return,
        };
        out.push(self.send(pending, method, params));
    }

    /// Whether `turn` is still what `sid` would be suggested for.
    fn suggestion_current(&self, sid: &str, turn: &str) -> bool {
        self.opts.prompt_suggestions
            && self.threads.get(sid).is_some_and(|t| {
                !t.suggestions_off && t.suggest_turn.as_deref() == Some(turn) && t.turn.is_none() && !t.starting
            })
    }

    /// A predicted message for `sid`'s `turn`: shown when it still applies and
    /// reads like a message from the person.
    fn deliver_suggestion(&mut self, sid: &str, turn: &str, text: Option<&str>, out: &mut Vec<Out>) {
        if !self.suggestion_current(sid, turn) {
            return;
        }
        let t = self.threads.get_mut(sid).expect("checked above");
        t.suggest_turn = None;
        let Some(text) = text.and_then(normalize) else { return };
        t.suggestion_shown = true;
        out.push(update(sid, update_value(Some(&text))));
    }

    /// The app-server answered an ask.
    pub(super) fn suggestion_result(
        &mut self,
        thread: String,
        turn: String,
        route: Route,
        r: &Value,
        out: &mut Vec<Out>,
    ) {
        match route {
            // The answer comes as `thread/prediction/updated`.
            Route::Prediction | Route::Unavailable => {}
            Route::Sample => {
                let text = r.get("text").and_then(Value::as_str);
                self.deliver_suggestion(&thread, &turn, text, out);
            }
            Route::Fork => {
                let Some(fork) = r.pointer("/thread/id").and_then(Value::as_str).map(str::to_owned) else { return };
                if !self.suggestion_current(&thread, &turn) {
                    return out.push(self.send(Pending::Ignore, "thread/unsubscribe", json!({"threadId": fork})));
                }
                self.prediction_forks.insert(
                    fork.clone(),
                    PredictionFork { session: thread, turn, fork_turn: None, spoiled: false, text: Vec::new() },
                );
                let params = json!({"threadId": fork, "input": [{"type": "text", "text": INSTRUCTIONS, "text_elements": []}], "effort": "low"});
                out.push(self.send(Pending::SuggestTurn { fork }, "turn/start", params));
            }
        }
    }

    /// The app-server refused an ask: -32601 (or a sample that timed out)
    /// moves the connection to the next route and asks again there; any other
    /// refusal drops this one.
    pub(super) fn suggestion_error(
        &mut self,
        thread: String,
        turn: String,
        route: Route,
        code: i64,
        message: &str,
        out: &mut Vec<Out>,
    ) {
        // A sample has 30 s in Codex: a slower model (a router, a long
        // context) times every one out, so this connection forks instead.
        let too_slow = route == Route::Sample && code == INTERNAL && message.contains("timed out");
        if !(code == METHOD_NOT_FOUND || too_slow) || route != self.suggestion_route {
            return;
        }
        self.suggestion_route = match route {
            Route::Prediction => Route::Sample,
            Route::Sample => Route::Fork,
            Route::Fork | Route::Unavailable => Route::Unavailable,
        };
        if self.suggestion_current(&thread, &turn) {
            self.ask_suggestion(&thread, &turn, out);
        }
    }

    /// A fork stopped before its turn had an id is interrupted once it has one.
    pub(super) fn fork_turn_known(&mut self, fork: &str, r: &Value, out: &mut Vec<Out>) {
        let Some(f) = self.prediction_forks.get_mut(fork) else { return };
        if f.fork_turn.is_some() {
            return;
        }
        f.fork_turn = r.pointer("/turn/id").and_then(Value::as_str).map(str::to_owned);
        if f.spoiled
            && let Some(turn) = f.fork_turn.clone()
        {
            out.push(self.send(Pending::Ignore, "turn/interrupt", json!({"threadId": fork, "turnId": turn})));
        }
    }

    /// The fork's turn did not start: it is let go.
    pub(super) fn suggestion_turn_error(&mut self, fork: String, out: &mut Vec<Out>) {
        if self.prediction_forks.remove(&fork).is_some() {
            out.push(self.send(Pending::Ignore, "thread/unsubscribe", json!({"threadId": fork})));
        }
    }

    /// `thread/prediction/updated` for a session.
    pub(super) fn prediction_updated(&mut self, sid: &str, p: &Value, out: &mut Vec<Out>) {
        let Some(turn) = p.get("sourceTurnId").and_then(Value::as_str) else { return };
        let result = p.get("result").unwrap_or(&Value::Null);
        let text = (result.get("type").and_then(Value::as_str) == Some("completed"))
            .then(|| result.get("text").and_then(Value::as_str))
            .flatten();
        let turn = turn.to_owned();
        if text.is_none() {
            if self.suggestion_current(sid, &turn)
                && let Some(t) = self.threads.get_mut(sid)
            {
                t.suggest_turn = None;
            }
            return;
        }
        self.deliver_suggestion(sid, &turn, text, out);
    }

    /// A line about a prediction fork. True when it was one: the session never sees it.
    pub(super) fn prediction_line(&mut self, method: &str, p: &Value, out: &mut Vec<Out>) -> bool {
        let Some(fork) = p.get("threadId").and_then(Value::as_str) else { return false };
        let Some(f) = self.prediction_forks.get_mut(fork) else { return false };
        let fork = fork.to_owned();
        match method {
            "turn/started" => self.fork_turn_known(&fork, p, out),
            "item/started" | "item/completed" => {
                let kind = p.pointer("/item/type").and_then(Value::as_str).unwrap_or("");
                match kind {
                    "agentMessage" if method == "item/completed" => {
                        if let Some(text) = p.pointer("/item/text").and_then(Value::as_str) {
                            f.text.push(text.to_owned());
                        }
                    }
                    "agentMessage" | "reasoning" | "userMessage" | "hookPrompt" | "plan" => {}
                    // It was told not to act; one that does is stopped, and its answer dropped.
                    _ if !f.spoiled => {
                        f.spoiled = true;
                        let turn =
                            f.fork_turn.clone().or_else(|| p.get("turnId").and_then(Value::as_str).map(str::to_owned));
                        if let Some(turn) = turn {
                            out.push(self.send(
                                Pending::Ignore,
                                "turn/interrupt",
                                json!({"threadId": fork, "turnId": turn}),
                            ));
                        }
                    }
                    _ => {}
                }
            }
            "turn/completed" => {
                let f = self.prediction_forks.remove(&fork).expect("found above");
                out.push(self.send(Pending::Ignore, "thread/unsubscribe", json!({"threadId": fork})));
                let turn = p.get("turn").unwrap_or(&Value::Null);
                if f.spoiled || turn.get("status").and_then(Value::as_str) != Some("completed") {
                    if let Some(t) = self.threads.get_mut(&f.session)
                        && t.suggest_turn.as_deref() == Some(f.turn.as_str())
                    {
                        t.suggest_turn = None;
                    }
                    return true;
                }
                let last = turn
                    .get("items")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .filter(|i| i.get("type").and_then(Value::as_str) == Some("agentMessage"))
                    .filter_map(|i| i.get("text").and_then(Value::as_str))
                    .next_back()
                    .map(str::to_owned)
                    .or_else(|| f.text.last().cloned());
                self.deliver_suggestion(&f.session, &f.turn, last.as_deref(), out);
            }
            _ => {}
        }
        true
    }
}

/// The prediction as a suggestion, or `None` when it does not read like a
/// short message from the person: empty, meta text ("nothing to suggest"),
/// several sentences or lines, a question, the assistant's voice, thanks or
/// praise, markdown, or more than 12 words.
pub(super) fn normalize(text: &str) -> Option<String> {
    let text = text.trim();
    let text = text.trim_start_matches(['"', '\'', '`', '\u{201c}', '\u{2018}']);
    let text = text.trim_end_matches(['"', '\'', '`', '\u{201d}', '\u{2019}']);
    let text = text.strip_suffix('.').unwrap_or(text).trim();
    let lower = text.to_lowercase();
    let words = text.split_whitespace().count();
    let refused = text.is_empty()
        || text.chars().count() >= 100
        || words > 12
        || text.contains(['\n', '\r', '*', '#', '`'])
        || text.ends_with('?')
        || several_sentences(text)
        || starts_with_any(
            &lower,
            &[
                "i'll",
                "i will",
                "i'm",
                "i am",
                "i can",
                "let me",
                "here's",
                "here is",
                "sure",
                "of course",
                "certainly",
            ],
        )
        || starts_with_any(
            &lower,
            &[
                "none",
                "nothing",
                "no suggestion",
                "no suggestions",
                "no prediction",
                "no predictions",
                "n/a",
                "silence",
            ],
        )
        || (text.starts_with('(') && text.ends_with(')'))
        || (text.starts_with('[') && text.ends_with(']'))
        || ["thanks", "thank you", "looks good", "sounds good", "great", "perfect", "awesome", "nice"]
            .iter()
            .any(|w| has_word(&lower, w));
    (!refused).then(|| text.to_owned())
}

/// A stop then more text: `. x`, `! x`, `? x`, or a CJK stop before anything
/// (`file.ts` stays one sentence).
fn several_sentences(text: &str) -> bool {
    let chars: Vec<char> = text.chars().collect();
    chars.iter().enumerate().any(|(i, c)| match c {
        '.' | '!' | '?' => {
            chars.get(i + 1).is_some_and(|n| n.is_whitespace()) && chars[i + 1..].iter().any(|n| !n.is_whitespace())
        }
        '\u{3002}' | '\u{ff01}' | '\u{ff1f}' => chars[i + 1..].iter().any(|n| !n.is_whitespace()),
        _ => false,
    })
}

fn boundary(c: Option<char>) -> bool {
    c.is_none_or(|c| !c.is_alphanumeric())
}

/// `lower` starts with one of `phrases`, as a whole word.
fn starts_with_any(lower: &str, phrases: &[&str]) -> bool {
    phrases.iter().any(|p| lower.strip_prefix(p).is_some_and(|rest| boundary(rest.chars().next())))
}

/// `lower` has `word` as a whole word (or phrase).
fn has_word(lower: &str, word: &str) -> bool {
    lower
        .match_indices(word)
        .any(|(at, _)| boundary(lower[..at].chars().next_back()) && boundary(lower[at + word.len()..].chars().next()))
}
