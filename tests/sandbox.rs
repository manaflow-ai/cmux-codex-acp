//! A command that a Codex rule (`$CODEX_HOME/rules/*.rules`) allows skips the
//! approval prompt, not the sandbox: in `read-only` it still runs inside the
//! read-only sandbox (Seatbelt on macOS, bubblewrap on Linux).
//!
//! Each test runs the `cmux-codex-acp` binary over stdio against a real
//! `codex app-server` and a local stand-in for the model, which asks for one
//! shell command that writes a file. No model access is needed, but a codex
//! binary is:
//!
//! ```text
//! CMUX_CODEX_ACP_CODEX=/path/to/codex cargo test --test sandbox
//! ```
//!
//! Without CMUX_CODEX_ACP_CODEX each test says so and passes.

#[path = "support/acp.rs"]
mod acp;

use acp::Agent;
use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex};
use std::time::Duration;

const TURN: Duration = Duration::from_secs(120);

/// The model stand-in: an OpenAI Responses endpoint whose first answer is
/// one `exec_command` call of `command` and whose later answers end the turn.
/// It keeps every request body it got, and sends the command's output (what
/// Codex told the model) on `outputs`.
struct Model {
    url: String,
    bodies: Arc<Mutex<Vec<Value>>>,
    outputs: Receiver<String>,
}

fn model(command: String) -> Model {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind the model stand-in");
    let url = format!("http://{}/v1", listener.local_addr().expect("its address"));
    let bodies = Arc::new(Mutex::new(Vec::new()));
    let kept = bodies.clone();
    let (tx, outputs) = channel();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(stream) = stream else { break };
            let (kept, command, tx) = (kept.clone(), command.clone(), tx.clone());
            std::thread::spawn(move || answer(stream, &kept, &command, &tx));
        }
    });
    Model { url, bodies, outputs }
}

/// The output of the command call in a request body, if it has one.
fn command_output(body: &Value) -> Option<String> {
    body["input"]
        .as_array()?
        .iter()
        .find(|i| i["type"] == "function_call_output" && i["call_id"] == "call-1")
        .map(|i| i["output"].as_str().map_or_else(|| i["output"].to_string(), str::to_owned))
}

fn answer(stream: TcpStream, kept: &Mutex<Vec<Value>>, command: &str, outputs: &Sender<String>) {
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
    if let Some(output) = command_output(&body) {
        let _ = outputs.send(output);
    }
    let first = {
        let mut kept = kept.lock().unwrap();
        kept.push(body);
        kept.len() == 1
    };
    let usage = json!({"input_tokens": 0, "input_tokens_details": null, "output_tokens": 0, "output_tokens_details": null, "total_tokens": 0});
    let item = if first {
        let arguments = json!({"cmd": command, "yield_time_ms": 5000}).to_string();
        json!({"type": "function_call", "call_id": "call-1", "name": "exec_command", "arguments": arguments})
    } else {
        json!({"type": "message", "role": "assistant", "id": "msg-1", "content": [{"type": "output_text", "text": "done"}]})
    };
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

struct Rig {
    argv: Vec<String>,
    home: PathBuf,
    work: PathBuf,
    /// The file the command writes.
    target: PathBuf,
    model: Model,
}

/// A CODEX_HOME whose only provider is the model stand-in and whose rules
/// allow `touch`, a git repository to work in, and the bridge's command line
/// (`--mode` when `mode` is given).
fn rig(name: &str, mode: Option<&str>) -> Option<Rig> {
    let Ok(codex) = std::env::var("CMUX_CODEX_ACP_CODEX") else {
        eprintln!("{name}: CMUX_CODEX_ACP_CODEX is not set; no codex run");
        return None;
    };
    let root = std::env::temp_dir().join(format!("cxacp-sandbox-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let (home, work) = (root.join("home"), root.join("work"));
    std::fs::create_dir_all(home.join("rules")).expect("CODEX_HOME");
    std::fs::create_dir_all(&work).expect("project dir");
    let _ = std::process::Command::new("git").args(["init", "-q"]).current_dir(&work).status();
    let target = work.join("written");
    let model = model(format!("touch {}", target.display()));
    std::fs::write(
        home.join("config.toml"),
        format!(
            r#"model = "mock-model"
model_provider = "mock"

[model_providers.mock]
name = "mock"
base_url = "{}"
wire_api = "responses"
request_max_retries = 0
stream_max_retries = 0
supports_websockets = false
"#,
            model.url
        ),
    )
    .expect("config.toml");
    std::fs::write(home.join("rules").join("test.rules"), "prefix_rule(pattern = [\"touch\"], decision = \"allow\")\n")
        .expect("rules");
    let mut argv = vec![env!("CARGO_BIN_EXE_cmux-codex-acp").to_owned(), "--codex".into(), codex, "--no-titles".into()];
    if let Some(mode) = mode {
        argv.extend(["--mode".into(), mode.into()]);
    }
    Some(Rig { argv, home, work, target, model })
}

impl Rig {
    fn start(&self) -> (Agent, String) {
        let envs = [("CODEX_HOME".to_owned(), self.home.display().to_string())];
        let mut a = Agent::spawn(&self.argv, &envs, &self.work);
        // A rules-allowed command asks nothing; any question is refused.
        a.policy = Box::new(|_| None);
        a.initialize();
        let s = a.new_session(&self.work);
        (a, s["sessionId"].as_str().expect("sessionId").to_owned())
    }

    /// Runs one prompt; asserts the command ran without a permission request,
    /// and returns whether its file exists and what the model was told.
    fn run_turn(&self, a: &mut Agent, sid: &str, text: &str) -> (bool, String) {
        let (r, from) = a.prompt(sid, text, TURN);
        assert!(r.get("error").is_none(), "prompt failed: {r}");
        let asked = a.log[from..].iter().any(|(_, m)| m["method"] == "session/request_permission");
        assert!(!asked, "a rules-allowed command asked for permission");
        let bodies = self.model.bodies.lock().unwrap();
        let output = bodies.iter().find_map(command_output).expect("the command's output went back to the model");
        (self.target.exists(), output)
    }

    /// Sends a prompt and waits only until the command's output reached the
    /// model (a review's own end needs a review result the stand-in does not
    /// give). Any permission request is refused (`start`), which would show
    /// in the output as a refusal, not a sandbox denial.
    fn run_until_output(&self, a: &mut Agent, sid: &str, text: &str) -> (bool, String) {
        a.request("session/prompt", json!({"sessionId": sid, "prompt": [{"type": "text", "text": text}]}));
        let output = self.model.outputs.recv_timeout(TURN).expect("the command's output went back to the model");
        (self.target.exists(), output)
    }
}

/// The command ran, and the sandbox refused its write: Linux's bubblewrap
/// mounts the filesystem read-only, macOS's Seatbelt denies the write.
fn assert_sandboxed(written: bool, output: &str, target: &Path) {
    assert!(
        !written,
        "a command a Codex rule allows ran outside the read-only sandbox: it wrote {} (output: {output})",
        target.display()
    );
    assert!(
        output.contains("touch")
            && (output.contains("Read-only file system") || output.contains("Operation not permitted")),
        "the command did not run into the sandbox: {output}"
    );
}

#[test]
fn a_rules_allowed_command_runs_and_writes_where_its_mode_lets_it() {
    // The control: the same command in `agent` writes in the workspace.
    let Some(rig) = rig("control", Some("agent")) else { return };
    let (mut a, sid) = rig.start();
    let (written, output) = rig.run_turn(&mut a, &sid, "go");
    assert!(written, "the command did not write in agent mode (output: {output})");
    assert!(output.contains("exited with code 0"), "{output}");
    a.close();
}

#[test]
fn a_rules_allowed_command_runs_in_the_read_only_sandbox() {
    let Some(rig) = rig("start", Some("read-only")) else { return };
    let (mut a, sid) = rig.start();
    let (written, output) = rig.run_turn(&mut a, &sid, "go");
    assert_sandboxed(written, &output, &rig.target);
    a.close();
}

fn switch_to_read_only(a: &mut Agent, sid: &str) {
    let id = a.request("session/set_mode", json!({"sessionId": sid, "modeId": "read-only"}));
    a.result(id, Duration::from_secs(10));
}

#[test]
fn a_rules_allowed_command_runs_in_the_read_only_sandbox_after_a_switch() {
    let Some(rig) = rig("switch", None) else { return };
    let (mut a, sid) = rig.start();
    switch_to_read_only(&mut a, &sid);
    let (written, output) = rig.run_turn(&mut a, &sid, "go");
    assert_sandboxed(written, &output, &rig.target);
    a.close();
}

#[test]
fn a_review_after_a_switch_runs_in_the_read_only_sandbox() {
    let Some(rig) = rig("review", None) else { return };
    let (mut a, sid) = rig.start();
    switch_to_read_only(&mut a, &sid);
    let (written, output) = rig.run_until_output(&mut a, &sid, "/review");
    assert_sandboxed(written, &output, &rig.target);
    a.close();
}
