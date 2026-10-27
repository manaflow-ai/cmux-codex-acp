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
    for (code, message) in [
        (-32600, "model is unavailable"),
        (-32603, "thread already has an active turn"),
        (-32600, "thread is not already in a turn"),
    ] {
        let translator = setup();
        active_goal(&translator);
        let start = request(send(&translator), "turn/start");
        let outs = translator.from_server(&json!({"id": start["id"], "error": {"code": code, "message": message}}));
        assert!(outs.iter().all(|out| matches!(out, Out::Client(_))));
    }
}

#[test]
fn a_repeated_clear_invalidates_an_older_goal_snapshot() {
    let translator = setup();
    let mut outs = Vec::new();
    translator.lock().read_goal("t", &mut outs);
    let read = request(outs, "thread/goal/get");
    tell(&translator, "thread/goal/cleared", json!({"threadId": "t"}));
    let outs = translator.from_server(&json!({"id": read["id"], "result": {
        "goal": {"status": "active", "objective": "Old task"}
    }}));
    assert!(outs.is_empty());
    assert!(translator.lock().threads["t"].goal.is_none());
}

#[test]
fn a_busy_start_before_the_initial_goal_read_finishes_confirms_the_goal_then_steers() {
    for initial_reply_first in [true, false] {
        let translator = setup();
        let mut outs = Vec::new();
        translator.lock().read_goal("t", &mut outs);
        let initial = request(outs, "thread/goal/get");
        let start = request(send(&translator), "turn/start");
        let check = request(
            translator.from_server(&json!({"id": start["id"], "error": {
                "code": -32600, "message": "thread already has an active turn"
            }})),
            "thread/goal/get",
        );
        let old = json!({"id": initial["id"], "result": {"goal": null}});
        if initial_reply_first {
            assert!(translator.from_server(&old).is_empty());
        }
        let read = request(
            translator.from_server(&json!({"id": check["id"], "result": {"goal": {"status": "active"}}})),
            "thread/read",
        );
        if !initial_reply_first {
            assert!(translator.from_server(&old).is_empty());
        }
        let steer = request(
            translator.from_server(&json!({"id": read["id"], "result": {
                "thread": {"turns": [{"id": "auto-3", "status": "inProgress"}]}
            }})),
            "turn/steer",
        );
        assert_eq!(steer["params"]["input"], start["params"]["input"]);
        assert_eq!(steer["params"]["expectedTurnId"], "auto-3");
    }
}

#[test]
fn a_cancel_during_goal_send_recovery_never_steers_afterwards() {
    for cancel_before_refusal in [true, false] {
        let translator = setup();
        active_goal(&translator);
        let start = request(send(&translator), "turn/start");
        let cancel = json!({"method": "session/cancel", "params": {"sessionId": "t"}});
        if cancel_before_refusal {
            translator.from_client(&cancel);
        }
        let outs = translator.from_server(&json!({"id": start["id"], "error": {
            "code": -32600, "message": "thread already has an active turn"
        }}));
        if cancel_before_refusal {
            assert!(
                outs.iter().any(|out| matches!(out, Out::Client(msg) if msg["result"]["stopReason"] == "cancelled"))
            );
            assert!(outs.iter().all(|out| matches!(out, Out::Client(_))));
        } else {
            let read = request(outs, "thread/read");
            let cancelled = translator.from_client(&cancel);
            assert!(
                cancelled
                    .iter()
                    .any(|out| matches!(out, Out::Client(msg) if msg["result"]["stopReason"] == "cancelled"))
            );
            assert!(
                translator
                    .from_server(&json!({"id": read["id"], "result": {
                        "thread": {"turns": [{"id": "auto-4", "status": "inProgress"}]}
                    }}))
                    .is_empty()
            );
        }
    }
}

#[test]
fn empty_service_tiers_fall_back_to_the_advertised_speed_tiers() {
    let tiers = crate::speed::model_tiers(&json!({"serviceTiers": [], "additionalSpeedTiers": ["fast", "ultrafast"]}));
    assert_eq!(tiers.iter().map(|tier| tier["value"].as_str().unwrap()).collect::<Vec<_>>(), ["fast", "ultrafast"]);
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
