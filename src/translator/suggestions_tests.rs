//! Prompt suggestions (`suggestions.rs`) against fake app-server lines.

use super::suggestions::normalize;
use super::*;

fn setup(on: bool) -> Translator {
    let translator = Translator::new(Options { prompt_suggestions: on, ..Options::default() });
    let mut state = translator.lock();
    state.initialized = true;
    state.threads.insert("t".into(), Thread { title_generated: true, ..Thread::default() });
    drop(state);
    translator
}

fn tell(translator: &Translator, method: &str, params: Value) -> Vec<Out> {
    translator.from_server(&json!({"method": method, "params": params}))
}

fn answer(translator: &Translator, request: &Value, result: Value) -> Vec<Out> {
    translator.from_server(&json!({"id": request["id"], "result": result}))
}

fn refuse(translator: &Translator, request: &Value, code: i64, message: &str) -> Vec<Out> {
    translator.from_server(&json!({"id": request["id"], "error": {"code": code, "message": message}}))
}

fn prompt(translator: &Translator, id: i64, text: &str) -> Vec<Out> {
    translator.from_client(&json!({"id": id, "method": "session/prompt", "params": {
        "sessionId": "t", "prompt": [{"type": "text", "text": text}]
    }}))
}

fn requests(outs: &[Out]) -> Vec<Value> {
    outs.iter()
        .filter_map(|o| match o {
            Out::Server(m) if m.get("method").is_some() => Some(m.clone()),
            _ => None,
        })
        .collect()
}

fn request(outs: &[Out], method: &str) -> Value {
    requests(outs).into_iter().find(|m| m["method"] == method).unwrap_or_else(|| panic!("no {method} in {outs:?}"))
}

fn client_lines(outs: &[Out]) -> Vec<Value> {
    outs.iter()
        .filter_map(|o| match o {
            Out::Client(m) => Some(m.clone()),
            _ => None,
        })
        .collect()
}

fn suggestions(outs: &[Out]) -> Vec<Value> {
    client_lines(outs)
        .into_iter()
        .filter(|m| m["params"]["update"]["sessionUpdate"] == "prompt_suggestion")
        .map(|m| m["params"]["update"]["suggestion"].clone())
        .collect()
}

/// A prompt whose turn `turn` runs and ends with `status`; the lines after its end.
fn run_turn(translator: &Translator, id: i64, turn: &str, status: &str) -> Vec<Out> {
    let start = request(&prompt(translator, id, "fix the flaky test"), "turn/start");
    answer(translator, &start, json!({"turn": {"id": turn}}));
    tell(translator, "turn/started", json!({"threadId": "t", "turn": {"id": turn}}));
    tell(translator, "turn/completed", json!({"threadId": "t", "turn": {"id": turn, "status": status, "items": []}}))
}

/// `run_turn` that completes, with the prediction API refused, so the
/// connection is on `thread/sample`: its request.
fn sampled_turn(translator: &Translator, id: i64, turn: &str) -> Value {
    let outs = run_turn(translator, id, turn, "completed");
    let ask = request(&outs, "thread/prediction/request");
    let outs = refuse(translator, &ask, -32601, "thread/prediction/request is not implemented yet");
    request(&outs, "thread/sample")
}

#[test]
fn a_completed_turn_asks_codex_own_prediction_api_first() {
    let translator = setup(true);
    let outs = run_turn(&translator, 1, "turn-1", "completed");
    // The prompt's answer comes before the ask, which never delays it.
    let answer_at = outs.iter().position(|o| matches!(o, Out::Client(m) if m["id"] == 1)).expect("prompt answer");
    let ask_at = outs
        .iter()
        .position(|o| matches!(o, Out::Server(m) if m["method"] == "thread/prediction/request"))
        .expect("prediction ask");
    assert!(answer_at < ask_at);
    assert_eq!(requests(&outs)[0]["params"], json!({"threadId": "t", "sourceTurnId": "turn-1"}));
    let outs = tell(
        &translator,
        "thread/prediction/updated",
        json!({"threadId": "t", "sourceTurnId": "turn-1", "result": {"type": "completed", "text": "run the tests"}}),
    );
    assert_eq!(suggestions(&outs), [json!("run the tests")]);
}

#[test]
fn not_implemented_falls_back_to_a_sample_and_stays_there() {
    let translator = setup(true);
    let sample = sampled_turn(&translator, 1, "turn-1");
    assert_eq!(sample["params"]["threadId"], "t");
    assert_eq!(sample["params"]["expectedTurnId"], "turn-1");
    assert_eq!(sample["params"]["effort"], "low");
    assert!(sample["params"]["prompt"].as_str().is_some_and(|p| p.len() <= 4000));
    let outs =
        answer(&translator, &sample, json!({"sourceTurnId": "turn-1", "responseId": "r", "text": "\"Commit it.\""}));
    assert_eq!(suggestions(&outs), [json!("Commit it")]);
    // The next turn goes straight to the sample.
    let outs = prompt(&translator, 2, "commit it");
    assert_eq!(suggestions(&outs), [Value::Null]);
    let start = request(&outs, "turn/start");
    answer(&translator, &start, json!({"turn": {"id": "turn-2"}}));
    let outs =
        tell(&translator, "turn/completed", json!({"threadId": "t", "turn": {"id": "turn-2", "status": "completed"}}));
    assert_eq!(requests(&outs).len(), 1);
    assert_eq!(request(&outs, "thread/sample")["params"]["expectedTurnId"], "turn-2");
}

#[test]
fn without_sample_a_hidden_prediction_fork_answers() {
    let translator = setup(true);
    let sample = sampled_turn(&translator, 1, "turn-1");
    let outs = refuse(&translator, &sample, -32601, "method not found");
    let fork = request(&outs, "thread/fork");
    assert_eq!(fork["params"]["threadId"], "t");
    assert_eq!(fork["params"]["ephemeral"], true);
    assert_eq!(fork["params"]["experimentalPredictionMode"], true);
    let outs = answer(&translator, &fork, json!({"thread": {"id": "fork-1"}}));
    assert!(client_lines(&outs).is_empty());
    let start = request(&outs, "turn/start");
    assert_eq!(start["params"]["threadId"], "fork-1");
    assert_eq!(start["params"]["effort"], "low");
    // Nothing of the fork reaches the session.
    let mut seen = Vec::new();
    seen.extend(answer(&translator, &start, json!({"turn": {"id": "fork-turn"}})));
    seen.extend(tell(&translator, "thread/started", json!({"thread": {"id": "fork-1", "forkedFromId": "t"}})));
    seen.extend(tell(&translator, "turn/started", json!({"threadId": "fork-1", "turn": {"id": "fork-turn"}})));
    seen.extend(tell(
        &translator,
        "item/agentMessage/delta",
        json!({"threadId": "fork-1", "itemId": "m", "delta": "run "}),
    ));
    seen.extend(tell(
        &translator,
        "item/reasoning/summaryTextDelta",
        json!({"threadId": "fork-1", "itemId": "r", "delta": "hmm"}),
    ));
    seen.extend(tell(
        &translator,
        "thread/tokenUsage/updated",
        json!({"threadId": "fork-1", "tokenUsage": {"last": {"totalTokens": 9}}}),
    ));
    seen.extend(tell(
        &translator,
        "item/completed",
        json!({"threadId": "fork-1", "item": {"type": "agentMessage", "id": "m", "text": "run the tests"}}),
    ));
    assert!(client_lines(&seen).is_empty(), "{seen:?}");
    let outs = tell(
        &translator,
        "turn/completed",
        json!({"threadId": "fork-1", "turn": {"id": "fork-turn", "status": "completed",
            "items": [{"type": "agentMessage", "id": "m", "text": "run the tests"}]}}),
    );
    assert_eq!(client_lines(&outs).len(), 1);
    assert_eq!(suggestions(&outs), [json!("run the tests")]);
    assert_eq!(request(&outs, "thread/unsubscribe")["params"]["threadId"], "fork-1");
    assert!(translator.lock().prediction_forks.is_empty());
}

#[test]
fn a_fork_that_starts_a_tool_is_interrupted_and_suggests_nothing() {
    let translator = setup(true);
    translator.lock().suggestion_route = suggestions::Route::Fork;
    let outs = run_turn(&translator, 1, "turn-1", "completed");
    let fork = request(&outs, "thread/fork");
    let start = request(&answer(&translator, &fork, json!({"thread": {"id": "fork-1"}})), "turn/start");
    answer(&translator, &start, json!({"turn": {"id": "fork-turn"}}));
    let outs = tell(
        &translator,
        "item/started",
        json!({"threadId": "fork-1", "item": {"type": "commandExecution", "id": "c", "command": "rm -rf build"}}),
    );
    assert!(client_lines(&outs).is_empty());
    assert_eq!(request(&outs, "turn/interrupt")["params"], json!({"threadId": "fork-1", "turnId": "fork-turn"}));
    let outs = tell(
        &translator,
        "turn/completed",
        json!({"threadId": "fork-1", "turn": {"id": "fork-turn", "status": "interrupted",
            "items": [{"type": "agentMessage", "id": "m", "text": "run the tests"}]}}),
    );
    assert!(suggestions(&outs).is_empty());
    request(&outs, "thread/unsubscribe");
}

#[test]
fn a_fork_server_request_is_declined_not_asked() {
    let translator = setup(true);
    translator.lock().suggestion_route = suggestions::Route::Fork;
    let outs = run_turn(&translator, 1, "turn-1", "completed");
    let fork = request(&outs, "thread/fork");
    answer(&translator, &fork, json!({"thread": {"id": "fork-1"}}));
    let outs = translator.from_server(&json!({"id": 99, "method": "item/commandExecution/requestApproval",
        "params": {"threadId": "fork-1", "itemId": "c", "command": "ls"}}));
    assert!(client_lines(&outs).is_empty());
    assert!(matches!(&outs[..], [Out::Server(m)] if m["result"]["decision"] == "decline"));
}

#[test]
fn a_stale_suggestion_is_dropped() {
    let translator = setup(true);
    let sample = sampled_turn(&translator, 1, "turn-1");
    // The person sent their next prompt before the sample came back.
    let outs = prompt(&translator, 2, "now the docs");
    assert!(suggestions(&outs).is_empty());
    let outs = answer(&translator, &sample, json!({"text": "run the tests"}));
    assert!(suggestions(&outs).is_empty());
    // A prediction for an older turn changes nothing either.
    let outs = tell(
        &translator,
        "thread/prediction/updated",
        json!({"threadId": "t", "sourceTurnId": "turn-0", "result": {"type": "completed", "text": "yes"}}),
    );
    assert!(suggestions(&outs).is_empty());
}

#[test]
fn a_stale_fork_is_let_go_unused() {
    let translator = setup(true);
    translator.lock().suggestion_route = suggestions::Route::Fork;
    let outs = run_turn(&translator, 1, "turn-1", "completed");
    let fork = request(&outs, "thread/fork");
    prompt(&translator, 2, "now the docs");
    let outs = answer(&translator, &fork, json!({"thread": {"id": "fork-1"}}));
    assert_eq!(requests(&outs).len(), 1);
    assert_eq!(request(&outs, "thread/unsubscribe")["params"]["threadId"], "fork-1");
}

#[test]
fn interrupted_or_failed_turns_and_subagents_ask_nothing() {
    for status in ["interrupted", "failed"] {
        let translator = setup(true);
        let outs = run_turn(&translator, 1, "turn-1", status);
        assert!(requests(&outs).is_empty(), "{status}: {outs:?}");
    }
    let translator = setup(true);
    translator.lock().threads.get_mut("t").unwrap().parent = Some("root".into());
    let outs = run_turn(&translator, 1, "turn-1", "completed");
    assert!(requests(&outs).is_empty());
}

#[test]
fn off_in_settings_there_is_no_option_and_no_ask() {
    let translator = setup(false);
    let options = translator.lock().config_options("t");
    assert!(options.as_array().unwrap().iter().all(|o| o["id"] != suggestions::OPTION));
    let outs = run_turn(&translator, 1, "turn-1", "completed");
    assert!(requests(&outs).is_empty());
    let outs = translator.from_client(&json!({"id": 5, "method": "session/set_config_option",
        "params": {"sessionId": "t", "configId": "prompt-suggestions", "value": "on"}}));
    assert!(client_lines(&outs)[0].get("error").is_some());
}

#[test]
fn the_session_option_turns_them_off_and_clears_the_shown_one() {
    let translator = setup(true);
    let options = translator.lock().config_options("t");
    let option = options.as_array().unwrap().iter().find(|o| o["id"] == "prompt-suggestions").cloned().unwrap();
    assert_eq!(option["currentValue"], "on");
    assert_eq!(option["category"], "model_config");
    assert_eq!(option["type"], "select");
    let sample = sampled_turn(&translator, 1, "turn-1");
    answer(&translator, &sample, json!({"text": "run the tests"}));
    let outs = translator.from_client(&json!({"id": 5, "method": "session/set_config_option",
        "params": {"sessionId": "t", "configId": "prompt-suggestions", "value": "off"}}));
    assert_eq!(suggestions(&outs), [Value::Null]);
    let reply = client_lines(&outs).into_iter().find(|m| m["id"] == 5).unwrap();
    let off =
        reply["result"]["configOptions"].as_array().unwrap().iter().find(|o| o["id"] == "prompt-suggestions").cloned();
    assert_eq!(off.unwrap()["currentValue"], "off");
    // Off: no ask after the next turn.
    let start = request(&prompt(&translator, 2, "commit it"), "turn/start");
    answer(&translator, &start, json!({"turn": {"id": "turn-2"}}));
    let outs =
        tell(&translator, "turn/completed", json!({"threadId": "t", "turn": {"id": "turn-2", "status": "completed"}}));
    assert!(requests(&outs).is_empty());
}

#[test]
fn a_refused_or_empty_prediction_suggests_nothing() {
    let translator = setup(true);
    let sample = sampled_turn(&translator, 1, "turn-1");
    let outs = refuse(&translator, &sample, -32600, "sample source must be idle");
    assert!(suggestions(&outs).is_empty() && requests(&outs).is_empty());
    let start = request(&prompt(&translator, 2, "go on"), "turn/start");
    answer(&translator, &start, json!({"turn": {"id": "turn-2"}}));
    let outs =
        tell(&translator, "turn/completed", json!({"threadId": "t", "turn": {"id": "turn-2", "status": "completed"}}));
    // Another refusal never moves the route.
    let sample = request(&outs, "thread/sample");
    let outs = answer(&translator, &sample, json!({"text": null}));
    assert!(suggestions(&outs).is_empty());
    let outs = tell(
        &translator,
        "thread/prediction/updated",
        json!({"threadId": "t", "sourceTurnId": "turn-2", "result": {"type": "failed"}}),
    );
    assert!(suggestions(&outs).is_empty());
}

#[test]
fn predictions_that_are_not_a_message_from_the_person_are_dropped() {
    for (text, kept) in [
        ("run the tests", Some("run the tests")),
        ("\"commit this\"", Some("commit this")),
        ("Yes, go ahead.", Some("Yes, go ahead")),
        ("update app.ts too", Some("update app.ts too")),
        ("테스트 돌려줘", Some("테스트 돌려줘")),
        ("nothing to predict", None),
        ("", None),
        ("(no suggestion)", None),
        ("None", None),
        ("Let me run the tests", None),
        ("Thanks, looks good", None),
        ("should I commit?", None),
        ("Run the tests. Then commit.", None),
        ("run the tests\nthen commit", None),
        ("**run the tests**", None),
        ("please go ahead and run every single test in the whole repository right now", None),
    ] {
        assert_eq!(normalize(text).as_deref(), kept, "{text:?}");
    }
}

#[test]
fn a_sample_that_times_out_moves_the_connection_to_the_fork() {
    let translator = setup(true);
    let sample = sampled_turn(&translator, 1, "turn-1");
    let outs = refuse(&translator, &sample, -32603, "sample timed out");
    let fork = request(&outs, "thread/fork");
    assert_eq!(fork["params"]["threadId"], "t");
    assert_eq!(translator.lock().suggestion_route, suggestions::Route::Fork);
}

#[test]
fn the_next_prompt_interrupts_a_fork_still_predicting() {
    let translator = setup(true);
    translator.lock().suggestion_route = suggestions::Route::Fork;
    let outs = run_turn(&translator, 1, "turn-1", "completed");
    let fork = request(&outs, "thread/fork");
    let start = request(&answer(&translator, &fork, json!({"thread": {"id": "fork-1"}})), "turn/start");
    // Sent before the fork's turn had an id: the interrupt waits for it.
    let outs = prompt(&translator, 2, "now the docs");
    assert!(requests(&outs).iter().all(|m| m["method"] != "turn/interrupt"));
    let outs = answer(&translator, &start, json!({"turn": {"id": "fork-turn"}}));
    assert_eq!(request(&outs, "turn/interrupt")["params"], json!({"threadId": "fork-1", "turnId": "fork-turn"}));
    let outs = tell(
        &translator,
        "turn/completed",
        json!({"threadId": "fork-1", "turn": {"id": "fork-turn", "status": "interrupted", "items": []}}),
    );
    assert!(client_lines(&outs).is_empty());
    request(&outs, "thread/unsubscribe");
}

#[test]
fn a_second_session_refused_after_the_route_moved_asks_again_there() {
    let translator = setup(true);
    translator.lock().threads.insert("u".into(), Thread { title_generated: true, ..Thread::default() });
    let first = request(&run_turn(&translator, 1, "turn-1", "completed"), "thread/prediction/request");
    // Session u finishes a turn while t's ask is still on its way.
    let start = request(
        &translator.from_client(&json!({"id": 2, "method": "session/prompt", "params": {
            "sessionId": "u", "prompt": [{"type": "text", "text": "and here"}]}})),
        "turn/start",
    );
    answer(&translator, &start, json!({"turn": {"id": "u-1"}}));
    let outs =
        tell(&translator, "turn/completed", json!({"threadId": "u", "turn": {"id": "u-1", "status": "completed"}}));
    let second = request(&outs, "thread/prediction/request");
    let outs = refuse(&translator, &first, -32601, "not implemented yet");
    assert_eq!(request(&outs, "thread/sample")["params"]["threadId"], "t");
    let outs = refuse(&translator, &second, -32601, "not implemented yet");
    assert_eq!(request(&outs, "thread/sample")["params"]["threadId"], "u");
    assert_eq!(translator.lock().suggestion_route, suggestions::Route::Sample);
}

#[test]
fn closing_a_session_interrupts_its_prediction_fork() {
    let translator = setup(true);
    translator.lock().suggestion_route = suggestions::Route::Fork;
    let outs = run_turn(&translator, 1, "turn-1", "completed");
    let fork = request(&outs, "thread/fork");
    let start = request(&answer(&translator, &fork, json!({"thread": {"id": "fork-1"}})), "turn/start");
    answer(&translator, &start, json!({"turn": {"id": "fork-turn"}}));
    let outs = translator.from_client(&json!({"id": 9, "method": "session/close", "params": {"sessionId": "t"}}));
    assert_eq!(request(&outs, "turn/interrupt")["params"], json!({"threadId": "fork-1", "turnId": "fork-turn"}));
}
