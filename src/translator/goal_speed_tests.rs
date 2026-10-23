use super::*;

fn setup() -> Translator {
    let translator = Translator::new(Options::default());
    let mut state = translator.lock();
    state.initialized = true;
    state.threads.insert("t".into(), Thread::default());
    drop(state);
    translator
}

fn tell(translator: &Translator, method: &str, params: Value) {
    translator.from_server(&json!({"method": method, "params": params}));
}

fn active_goal(translator: &Translator) {
    tell(
        translator,
        "thread/goal/updated",
        json!({"threadId": "t", "goal": {
            "threadId": "t", "objective": "Finish the task", "status": "active"
        }}),
    );
}

fn send(translator: &Translator) -> Vec<Out> {
    translator.from_client(&json!({"id": 10, "method": "session/prompt", "params": {
        "sessionId": "t", "prompt": [{"type": "text", "text": "Also cover the timeout"}]
    }}))
}

fn request(outs: Vec<Out>, method: &str) -> Value {
    outs.into_iter()
        .find_map(|out| match out {
            Out::Server(msg) if msg["method"] == method => Some(msg),
            _ => None,
        })
        .unwrap_or_else(|| panic!("no {method} request"))
}

#[test]
fn a_message_during_an_autonomous_goal_turn_steers_that_turn() {
    let translator = setup();
    active_goal(&translator);
    tell(&translator, "turn/started", json!({"threadId": "t", "turn": {"id": "auto-1"}}));
    let steer = request(send(&translator), "turn/steer");
    assert_eq!(steer["params"]["expectedTurnId"], "auto-1");
    assert_eq!(steer["params"]["input"][0]["text"], "Also cover the timeout");
    assert!(translator.in_turn("t"));
}

#[test]
fn a_second_ordinary_prompt_still_does_not_replace_its_inflight_prompt() {
    let translator = setup();
    let first = request(send(&translator), "turn/start");
    let duplicate = send(&translator);
    assert!(duplicate.iter().any(|out| matches!(out, Out::Client(msg) if msg.get("error").is_some())));
    assert_eq!(translator.lock().threads["t"].prompt, Some(json!(10)));
    assert!(first["id"].is_number());
}

#[test]
fn a_goal_continuation_winning_the_send_race_preserves_the_input() {
    let translator = setup();
    active_goal(&translator);
    let start = request(send(&translator), "turn/start");
    let recovery = translator.from_server(&json!({"id": start["id"], "error": {
        "code": -32600, "message": "thread already has an active turn"
    }}));
    let read = request(recovery, "thread/read");
    let steer = request(
        translator.from_server(&json!({"id": read["id"], "result": {
            "thread": {"id": "t", "turns": [{"id": "auto-2", "status": "inProgress"}]}
        }})),
        "turn/steer",
    );
    assert_eq!(steer["params"]["expectedTurnId"], "auto-2");
    assert_eq!(steer["params"]["input"], start["params"]["input"]);
}

#[test]
fn an_unrelated_start_error_is_never_replayed() {
    let translator = setup();
    active_goal(&translator);
    let start = request(send(&translator), "turn/start");
    let outs = translator.from_server(&json!({"id": start["id"], "error": {
        "code": -32600, "message": "model is unavailable"
    }}));
    assert!(outs.iter().all(|out| matches!(out, Out::Client(_))));
}

#[test]
fn model_advertised_fast_and_ultrafast_are_sent_as_their_actual_service_tiers() {
    let translator = setup();
    {
        let mut state = translator.lock();
        state.models = config::parse_models(&json!({"data": [{"id": "m", "serviceTiers": [
            {"id": "priority", "name": "Fast"}, {"id": "ultrafast", "name": "Ultrafast"}
        ]}]}));
        state.threads.get_mut("t").unwrap().model = "m".into();
    }
    for value in ["priority", "ultrafast", "default"] {
        let picked = translator.from_client(&json!({"id": 11, "method": "session/set_config_option", "params": {
            "sessionId": "t", "configId": "fast-mode", "value": value
        }}));
        assert!(picked.iter().any(|out| matches!(out, Out::Client(msg) if msg.get("result").is_some())));
        let start = request(send(&translator), "turn/start");
        assert_eq!(start["params"]["serviceTier"], value);
        tell(&translator, "turn/completed", json!({"threadId": "t", "turn": {"id": "done", "status": "completed"}}));
    }
    let refused = translator.from_client(&json!({"id": 12, "method": "session/set_config_option", "params": {
        "sessionId": "t", "configId": "fast-mode", "value": "invented"
    }}));
    assert!(refused.iter().any(|out| matches!(out, Out::Client(msg) if msg.get("error").is_some())));
}
