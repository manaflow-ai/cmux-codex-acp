//! Prompt suggestions against a real `codex app-server` and a local stand-in
//! for the model: after a turn, the bridge (`--prompt-suggestions`) asks
//! Codex for the person's next message on whatever route this Codex has, and
//! the session gets one `prompt_suggestion` update and nothing else of it.
//!
//! ```text
//! CMUX_CODEX_ACP_CODEX=/path/to/codex cargo test --test suggestions
//! ```
//!
//! Without CMUX_CODEX_ACP_CODEX the test says so and passes.

#[path = "support/acp.rs"]
mod acp;

use acp::Agent;
use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex};
use std::time::Duration;

const WAIT: Duration = Duration::from_secs(120);
const SUGGESTION: &str = "run the tests";

/// Whether a model request is the prediction (its last user text asks for one).
fn is_prediction(body: &Value) -> bool {
    body["input"].as_array().is_some_and(|items| {
        items
            .iter()
            .rev()
            .filter(|i| i["role"] == "user")
            .take(1)
            .any(|i| i.to_string().contains("most likely to send"))
    })
}

/// One Responses request: the prediction gets [`SUGGESTION`], a turn gets "Fixed it.".
fn answer(stream: TcpStream, kept: &Mutex<Vec<Value>>) {
    let mut reader = BufReader::new(stream.try_clone().expect("clone the stream"));
    let mut request_line = String::new();
    if reader.read_line(&mut request_line).is_err() {
        return;
    }
    let mut length = 0usize;
    loop {
        let mut header = String::new();
        if reader.read_line(&mut header).is_err() || header.trim().is_empty() {
            break;
        }
        if let Some((name, value)) = header.split_once(':')
            && name.eq_ignore_ascii_case("content-length")
        {
            length = value.trim().parse().unwrap_or(0);
        }
    }
    let mut body = vec![0; length];
    if reader.read_exact(&mut body).is_err() {
        return;
    }
    let mut stream = stream;
    if !(request_line.starts_with("POST ")
        && request_line.split_whitespace().nth(1).is_some_and(|p| p.ends_with("/responses")))
    {
        let _ = stream.write_all(b"HTTP/1.1 404 Not Found\r\ncontent-length: 0\r\nconnection: close\r\n\r\n");
        return;
    }
    let body: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
    let text = if is_prediction(&body) { SUGGESTION } else { "Fixed it." };
    kept.lock().unwrap().push(body);
    let usage = json!({"input_tokens": 100, "input_tokens_details": {"cached_tokens": 80}, "output_tokens": 4, "output_tokens_details": null, "total_tokens": 104});
    let item = json!({"type": "message", "role": "assistant", "id": "msg-1", "content": [{"type": "output_text", "text": text}]});
    let events = [
        json!({"type": "response.created", "response": {"id": "resp-1"}}),
        json!({"type": "response.output_item.done", "item": item}),
        json!({"type": "response.completed", "response": {"id": "resp-1", "usage": usage}}),
    ];
    let sse: String = events.iter().map(|e| format!("event: {}\ndata: {e}\n\n", e["type"].as_str().unwrap())).collect();
    let head = format!(
        "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
        sse.len()
    );
    let _ = stream.write_all(head.as_bytes()).and_then(|()| stream.write_all(sse.as_bytes()));
}

#[test]
fn a_completed_turn_gets_one_hidden_suggestion_and_the_next_prompt_clears_it() {
    let Ok(codex) = std::env::var("CMUX_CODEX_ACP_CODEX") else {
        eprintln!("suggestions: CMUX_CODEX_ACP_CODEX is not set; no codex run");
        return;
    };
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind the model stand-in");
    let url = format!("http://{}/v1", listener.local_addr().expect("its address"));
    let bodies = Arc::new(Mutex::new(Vec::new()));
    let kept = bodies.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(stream) = stream else { break };
            let kept = kept.clone();
            std::thread::spawn(move || answer(stream, &kept));
        }
    });
    let root = std::env::temp_dir().join(format!("cxacp-suggestions-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let (home, work) = (root.join("home"), root.join("work"));
    std::fs::create_dir_all(&home).expect("CODEX_HOME");
    std::fs::create_dir_all(&work).expect("project dir");
    std::fs::write(
        home.join("config.toml"),
        format!(
            r#"model = "mock-model"
model_provider = "mock"

[model_providers.mock]
name = "mock"
base_url = "{url}"
wire_api = "responses"
request_max_retries = 0
stream_max_retries = 0
supports_websockets = false
"#
        ),
    )
    .expect("config.toml");
    let argv = vec![
        env!("CARGO_BIN_EXE_cmux-codex-acp").to_owned(),
        "--codex".into(),
        codex,
        "--no-titles".into(),
        "--prompt-suggestions".into(),
    ];
    let mut a = Agent::spawn(&argv, &[("CODEX_HOME".to_owned(), home.display().to_string())], &work);
    a.policy = Box::new(|_| None);
    a.initialize();
    let s = a.new_session(&work);
    let sid = s["sessionId"].as_str().expect("sessionId").to_owned();
    assert!(
        s["configOptions"].as_array().is_some_and(|o| o.iter().any(|o| o["id"] == "prompt-suggestions")),
        "no prompt-suggestions option: {s}"
    );
    let (r, from) = a.prompt(&sid, "fix the flaky test", WAIT);
    assert!(r.get("error").is_none(), "prompt failed: {r}");
    let suggestion = a.until(WAIT, |m| m["params"]["update"]["sessionUpdate"] == "prompt_suggestion");
    assert_eq!(suggestion["params"]["sessionId"], sid.as_str());
    assert_eq!(suggestion["params"]["update"]["suggestion"], SUGGESTION);
    // The prediction never showed in the session: one agent message, the turn's.
    assert_eq!(a.text(from, &sid), "Fixed it.");
    let usage = a.updates(from, &sid).iter().filter(|u| u["sessionUpdate"] == "usage_update").count();
    assert!(usage <= 1, "prediction usage reached the session");
    let model_calls = bodies.lock().unwrap().len();
    assert_eq!(model_calls, 2, "one turn and one prediction");
    let (r, from) = a.prompt(&sid, "run the tests", WAIT);
    assert!(r.get("error").is_none(), "prompt failed: {r}");
    let cleared: Vec<Value> = a
        .updates(from, &sid)
        .into_iter()
        .filter(|u| u["sessionUpdate"] == "prompt_suggestion")
        .map(|u| u["suggestion"].clone())
        .collect();
    assert_eq!(cleared.first(), Some(&Value::Null), "the next prompt cleared it");
    a.close();
    let _ = std::fs::remove_dir_all(&root);
}
