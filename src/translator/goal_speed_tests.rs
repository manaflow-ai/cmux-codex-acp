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
    tell(translator, "thread/goal/updated", json!({"threadId": "t", "goal": {
        "threadId": "t", "objective": "Finish the task", "status": "active"
    }}));
}

fn send(translator: &Translator) -> Vec<Out> {
    translator.from_client(&json!({"id": 10, "method": "session/prompt", "params": {
        "sessionId": "t", "prompt": [{"type": "text", "text": "Also cover the timeout"}]
    }}))
}

fn request(outs: Vec<Out>, method: &str) -> Value {
    outs.into_iter().find_map(|out| match out {
        Out::Server(msg) if msg["method"] == method => Some(msg),
        _ => None,
    }).unwrap_or_else(|| panic!("no {method} request"))
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
    let steer = request(translator.from_server(&json!({"id": read["id"], "result": {
        "thread": {"id": "t", "turns": [{"id": "auto-2", "status": "inProgress"}]}
    }})), "turn/steer");
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
