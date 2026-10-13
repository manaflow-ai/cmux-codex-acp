//! Behavior tests at the ACP boundary: the `cmux-codex-acp` binary over
//! stdio, against a real `codex app-server`.
//!
//! They need a codex binary and a CODEX_HOME that can reach a model:
//!
//! ```text
//! CMUX_CODEX_ACP_CODEX=/path/to/codex CODEX_HOME=/path/to/home \
//!   cargo test --test live -- --test-threads 4
//! ```
//!
//! Without CMUX_CODEX_ACP_CODEX each test says so and passes: CI runners
//! have no model access. `CMUX_CODEX_ACP_MODEL` picks the model.

#[path = "support/acp.rs"]
mod acp;

use acp::{Agent, pick};
use serde_json::{Value, json};
use std::path::PathBuf;
use std::time::Duration;

const TURN: Duration = Duration::from_secs(480);

struct Live {
    argv: Vec<String>,
    dir: PathBuf,
}

fn live(name: &str) -> Option<Live> {
    let Ok(codex) = std::env::var("CMUX_CODEX_ACP_CODEX") else {
        eprintln!("{name}: CMUX_CODEX_ACP_CODEX is not set; no live codex run");
        return None;
    };
    let mut argv = vec![env!("CARGO_BIN_EXE_cmux-codex-acp").to_owned(), "--codex".into(), codex];
    if let Ok(m) = std::env::var("CMUX_CODEX_ACP_MODEL") {
        argv.extend(["--model".into(), m]);
    }
    let dir = std::env::temp_dir().join(format!("cxacp-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("project dir");
    // Codex treats a git repository as a trusted workspace.
    let _ = std::process::Command::new("git").args(["init", "-q"]).current_dir(&dir).status();
    Some(Live { argv, dir })
}

fn start(l: &Live) -> (Agent, String) {
    let mut a = Agent::spawn(&l.argv, &[], &l.dir);
    a.initialize();
    let s = a.new_session(&l.dir);
    let sid = s["sessionId"].as_str().expect("sessionId").to_owned();
    (a, sid)
}

fn stop_reason(r: &Value) -> &str {
    assert!(r.get("error").is_none(), "prompt failed: {r}");
    r["result"]["stopReason"].as_str().unwrap_or("")
}

fn set_mode(a: &mut Agent, sid: &str, mode: &str) {
    let id = a.request("session/set_mode", json!({"sessionId": sid, "modeId": mode}));
    a.result(id, Duration::from_secs(10));
}

#[test]
fn multi_turn_session_streams_and_remembers() {
    let Some(l) = live("multi") else { return };
    let (mut a, sid) = start(&l);
    let (r, from) = a.prompt(&sid, "Reply with exactly the word: pineapple", TURN);
    assert_eq!(stop_reason(&r), "end_turn");
    let chunks = a.updates(from, &sid).iter().filter(|u| u["sessionUpdate"] == "agent_message_chunk").count();
    assert!(chunks >= 1, "no streamed message chunks");
    assert!(a.text(from, &sid).to_lowercase().contains("pineapple"), "first answer: {}", a.text(from, &sid));
    let usage = a.updates(from, &sid).into_iter().find(|u| u["sessionUpdate"] == "usage_update");
    assert!(usage.is_some_and(|u| u["size"].as_u64().unwrap_or(0) > 0), "no usage_update with a context size");
    assert!(r["result"]["usage"]["totalTokens"].as_u64().unwrap_or(0) > 0, "prompt result has no usage: {r}");

    let (r, from) =
        a.prompt(&sid, "Which word did you reply with in your previous answer? Reply with only that word.", TURN);
    assert_eq!(stop_reason(&r), "end_turn");
    assert!(a.text(from, &sid).to_lowercase().contains("pineapple"), "second answer: {}", a.text(from, &sid));
    a.close();
}

#[test]
fn exec_asks_for_approval_and_streams_output() {
    let Some(l) = live("exec") else { return };
    std::fs::write(l.dir.join("note.txt"), "marker-4242\n").unwrap();
    let (mut a, sid) = start(&l);
    set_mode(&mut a, &sid, "ask");
    let (r, from) = a.prompt(&sid, "Run the shell command `cat note.txt` and reply with its output.", TURN);
    assert_eq!(stop_reason(&r), "end_turn");
    let asked: Vec<Value> = a.log[from..]
        .iter()
        .filter(|(_, m)| m["method"] == "session/request_permission")
        .map(|(_, m)| m["params"].clone())
        .collect();
    assert!(!asked.is_empty(), "no permission request in ask mode");
    let call = &asked[0]["toolCall"];
    assert_eq!(asked[0]["sessionId"], sid.as_str());
    assert!(matches!(call["kind"].as_str(), Some("execute" | "read")), "kind: {call}");
    assert!(asked[0]["options"].as_array().unwrap().iter().any(|o| o["kind"] == "allow_once"));
    let done = a
        .updates(from, &sid)
        .into_iter()
        .find(|u| {
            u["sessionUpdate"] == "tool_call_update"
                && u["status"] == "completed"
                && u["toolCallId"] == call["toolCallId"]
        })
        .expect("the command's completion");
    assert!(done["content"].to_string().contains("marker-4242"), "output: {done}");
    assert!(a.text(from, &sid).contains("marker-4242"));
    a.close();
}

#[test]
fn patch_asks_for_approval_with_before_and_after() {
    let Some(l) = live("patch") else { return };
    std::fs::write(l.dir.join("greet.txt"), "hello\n").unwrap();
    let (mut a, sid) = start(&l);
    set_mode(&mut a, &sid, "ask");
    let (r, from) = a.prompt(
        &sid,
        "Use apply_patch to change greet.txt so its only line is goodbye instead of hello. Do not run shell commands.",
        TURN,
    );
    assert_eq!(stop_reason(&r), "end_turn");
    let asked = a.log[from..]
        .iter()
        .map(|(_, m)| m)
        .find(|m| m["method"] == "session/request_permission" && m["params"]["toolCall"]["kind"] == "edit")
        .map(|m| m["params"].clone())
        .expect("an edit approval");
    let diff = &asked["toolCall"]["content"][0];
    assert_eq!(diff["type"], "diff", "approval content: {asked}");
    assert_eq!(diff["oldText"], "hello\n");
    assert!(diff["newText"].as_str().unwrap_or("").contains("goodbye"));
    assert!(diff["path"].as_str().unwrap_or("").ends_with("greet.txt"));
    assert!(std::fs::read_to_string(l.dir.join("greet.txt")).unwrap().contains("goodbye"));
    a.close();
}

#[test]
fn denied_patch_leaves_the_file() {
    let Some(l) = live("deny") else { return };
    std::fs::write(l.dir.join("keep.txt"), "keep me\n").unwrap();
    let (mut a, sid) = start(&l);
    set_mode(&mut a, &sid, "ask");
    a.policy = pick("reject_once");
    let (r, _) = a.prompt(
        &sid,
        "Use apply_patch to change keep.txt so it says changed. If the change is refused, stop and reply refused.",
        TURN,
    );
    let _ = stop_reason(&r);
    assert_eq!(std::fs::read_to_string(l.dir.join("keep.txt")).unwrap(), "keep me\n");
    a.close();
}

#[test]
fn cancel_ends_a_running_turn() {
    let Some(l) = live("cancel") else { return };
    let (mut a, sid) = start(&l);
    set_mode(&mut a, &sid, "agent-full-access");
    let id = a.request(
        "session/prompt",
        json!({"sessionId": sid, "prompt": [{"type": "text", "text": "Run the shell command `sleep 120` and then reply done."}]}),
    );
    a.until(TURN, |m| {
        m["method"] == "session/update"
            && m["params"]["update"]["sessionUpdate"] == "tool_call"
            && m["params"]["update"]["kind"] == "execute"
    });
    let t0 = std::time::Instant::now();
    a.notify("session/cancel", json!({"sessionId": sid}));
    let r = a.response(id, Duration::from_secs(60));
    assert_eq!(stop_reason(&r), "cancelled");
    assert!(t0.elapsed() < Duration::from_secs(30), "cancel took {:?}", t0.elapsed());
    // The session takes the next prompt.
    let (r, _) = a.prompt(&sid, "Reply with exactly: ready", TURN);
    assert_eq!(stop_reason(&r), "end_turn");
    a.close();
}

#[test]
fn two_subagents_stream_under_their_own_sessions() {
    let Some(l) = live("subagents") else { return };
    let (mut a, sid) = start(&l);
    set_mode(&mut a, &sid, "agent-full-access");
    let (r, from) = a.prompt(
        &sid,
        "Spawn two sub-agents in parallel with your agent tools. The first must reply with exactly the word alpha, the second with exactly the word beta. Wait for both, then reply with their two answers.",
        TURN,
    );
    assert_eq!(stop_reason(&r), "end_turn");
    let parent = a.updates(from, &sid);
    let children: Vec<String> = parent
        .iter()
        .filter(|u| u["sessionUpdate"] == "subagent_spawned")
        .filter_map(|u| u["subagentSessionId"].as_str().map(str::to_owned))
        .collect();
    assert_eq!(children.len(), 2, "spawned: {children:?}");
    assert_ne!(children[0], children[1]);
    let mut words = Vec::new();
    for child in &children {
        assert_ne!(child, &sid);
        let text = a.text(from, child).to_lowercase();
        assert!(!text.is_empty(), "child {child} streamed no message under its own session");
        words.push(text);
        assert!(
            parent.iter().any(|u| u["sessionUpdate"] == "subagent_state_update"
                && u["subagentSessionId"] == child.as_str()
                && u["state"] == "completed"),
            "child {child} never completed"
        );
    }
    let all = words.join(" ");
    assert!(all.contains("alpha") && all.contains("beta"), "children said: {all}");
    a.close();
}

#[test]
fn a_session_resumes_in_a_new_process() {
    let Some(l) = live("resume") else { return };
    let (mut a, sid) = start(&l);
    let (r, _) = a.prompt(&sid, "Remember the code word walrus. Reply with exactly: ok", TURN);
    assert_eq!(stop_reason(&r), "end_turn");
    a.close();

    let mut b = Agent::spawn(&l.argv, &[], &l.dir);
    b.initialize();
    let from = b.log.len();
    let id = b.request("session/load", json!({"sessionId": sid, "cwd": l.dir, "mcpServers": []}));
    let loaded = b.result(id, Duration::from_secs(120));
    assert!(loaded["modes"]["currentModeId"].is_string());
    let replay = b.updates(from, &sid);
    assert!(
        replay.iter().any(|u| u["sessionUpdate"] == "user_message_chunk"
            && u["content"]["text"].as_str().unwrap_or("").contains("walrus")),
        "history replay lacks the first prompt: {replay:#?}"
    );
    let (r, from) = b.prompt(&sid, "What was the code word? Reply with only the word.", TURN);
    assert_eq!(stop_reason(&r), "end_turn");
    assert!(b.text(from, &sid).to_lowercase().contains("walrus"));
    b.close();
}

#[test]
fn model_switch_applies_to_the_next_turn() {
    let Some(l) = live("model") else { return };
    let mut a = Agent::spawn(&l.argv, &[], &l.dir);
    a.initialize();
    let s = a.new_session(&l.dir);
    let sid = s["sessionId"].as_str().unwrap().to_owned();
    let path = s["_meta"]["codexAppServer"]["threadPath"].as_str().map(PathBuf::from);
    // The model list may arrive after session/new.
    let (r, _) = a.prompt(&sid, "Reply with exactly: one", TURN);
    assert_eq!(stop_reason(&r), "end_turn");
    let opts = a
        .log
        .iter()
        .rev()
        .find_map(|(_, m)| {
            (m["params"]["sessionId"] == sid.as_str()
                && m["params"]["update"]["sessionUpdate"] == "config_option_update")
                .then(|| m["params"]["update"]["configOptions"].clone())
        })
        .unwrap_or_else(|| s["configOptions"].clone());
    let model_opt = opts.as_array().unwrap().iter().find(|o| o["id"] == "model").unwrap().clone();
    let current = model_opt["currentValue"].as_str().unwrap().to_owned();
    let other = model_opt["options"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|o| o["value"].as_str())
        .find(|v| *v != current)
        .expect("a second model")
        .to_owned();
    let id = a.request("session/set_config_option", json!({"sessionId": sid, "configId": "model", "value": other}));
    let set = a.result(id, Duration::from_secs(10));
    let now =
        set["configOptions"].as_array().unwrap().iter().find(|o| o["id"] == "model").unwrap()["currentValue"].clone();
    assert_eq!(now, other.as_str());
    let (r, _) = a.prompt(&sid, "Reply with exactly: two", TURN);
    assert_eq!(stop_reason(&r), "end_turn");
    if let Some(path) = path {
        let rollout = std::fs::read_to_string(&path).expect("rollout file");
        let last_model = rollout
            .lines()
            .rev()
            .filter_map(|l| serde_json::from_str::<Value>(l).ok())
            .filter(|v| v["type"] == "turn_context")
            .find_map(|v| v["payload"]["model"].as_str().map(str::to_owned));
        assert_eq!(last_model.as_deref(), Some(other.as_str()), "the second turn's model");
    }
    a.close();
}

#[test]
fn an_mcp_tool_call_asks_in_ask_mode() {
    let Some(l) = live("mcp") else { return };
    let marker = l.dir.join("marker.txt");
    let server = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/marker_mcp.py");
    let mut a = Agent::spawn(&l.argv, &[], &l.dir);
    a.initialize();
    let id = a.request(
        "session/new",
        json!({"cwd": l.dir, "mcpServers": [{
            "name": "marker", "command": "python3", "args": [server],
            "env": [{"name": "MARKER_FILE", "value": marker}],
        }]}),
    );
    let sid = a.result(id, TURN)["sessionId"].as_str().unwrap().to_owned();
    set_mode(&mut a, &sid, "ask");
    a.policy = pick("reject_once");
    let (r, from) = a.prompt(
        &sid,
        "Call the write_marker tool of the marker MCP server once with the text hello. If it is refused, reply refused and stop.",
        TURN,
    );
    let _ = stop_reason(&r);
    let asked: Vec<Value> = a.log[from..]
        .iter()
        .filter(|(_, m)| m["method"] == "session/request_permission")
        .map(|(_, m)| m["params"].clone())
        .collect();
    let called = a.updates(from, &sid).iter().any(|u| u["_meta"]["codexAppServer"]["itemType"] == "mcpToolCall");
    assert!(called, "the model never called the MCP tool");
    assert!(!asked.is_empty(), "an MCP tool call in ask mode ran without a permission request");
    assert!(!marker.exists(), "the denied MCP tool call ran");
    // Allowed, it runs.
    a.policy = pick("allow_once");
    let (r, from) =
        a.prompt(&sid, "Call the write_marker tool of the marker MCP server once with the text hello.", TURN);
    assert_eq!(stop_reason(&r), "end_turn");
    assert!(
        a.log[from..].iter().any(|(_, m)| m["method"] == "session/request_permission"),
        "the second MCP tool call ran without a permission request"
    );
    assert_eq!(
        std::fs::read_to_string(&marker).ok().as_deref(),
        Some("hello"),
        "the allowed MCP tool call did not run"
    );
    a.close();
}
