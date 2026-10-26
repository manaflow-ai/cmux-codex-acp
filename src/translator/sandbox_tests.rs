//! `read-only` runs in a permission profile with a deny entry, so a command a
//! Codex rule allows skips the prompt but not the sandbox.

use super::*;

const CURRENT: &str = "acpmux/0.164.0 (Ubuntu 24.4.0; x86_64) unknown (acpmux; 1)";
const PROFILE: &str = "cmux-read-only";

fn server_request(outs: Vec<Out>, method: &str) -> Value {
    outs.into_iter()
        .find_map(|o| match o {
            Out::Server(m) if m["method"] == method => Some(m),
            _ => None,
        })
        .unwrap_or_else(|| panic!("no {method} sent"))
}

/// A translator past `initialize` and `model/list` from an app-server whose
/// user agent is `user_agent`, new sessions starting in `mode`.
fn translator(user_agent: &str, mode: &str) -> Translator {
    let translator = Translator::new(Options { default_mode: mode.into(), ..Options::default() });
    let init = server_request(translator.start(), "initialize")["id"].clone();
    let list = server_request(
        translator.from_server(&json!({"jsonrpc": "2.0", "id": init, "result": {"userAgent": user_agent}})),
        "model/list",
    )["id"]
        .clone();
    translator.from_server(&json!({"jsonrpc": "2.0", "id": list, "result": {"data": []}}));
    translator
}

/// The `thread/start` of a `session/new`, and the session once Codex answers.
fn new_session(translator: &Translator, mcp: Value) -> Value {
    let start = server_request(
        translator.from_client(&json!({"jsonrpc": "2.0", "id": 1, "method": "session/new",
            "params": {"cwd": "/w", "mcpServers": mcp}})),
        "thread/start",
    );
    translator
        .from_server(&json!({"jsonrpc": "2.0", "id": start["id"], "result": {"thread": {"id": "t", "cwd": "/w"}}}));
    start["params"].clone()
}

fn set_mode(translator: &Translator, mode: &str) {
    translator.from_client(&json!({"jsonrpc": "2.0", "id": 2, "method": "session/set_mode",
        "params": {"sessionId": "t", "modeId": mode}}));
}

/// The `turn/start` params of a prompt; the turn is then over.
fn turn(translator: &Translator) -> Value {
    let start = server_request(
        translator.from_client(&json!({"jsonrpc": "2.0", "id": 3, "method": "session/prompt",
            "params": {"sessionId": "t", "prompt": [{"type": "text", "text": "go"}]}})),
        "turn/start",
    );
    let mut state = translator.state.lock().unwrap();
    let t = state.threads.get_mut("t").expect("thread t");
    (t.starting, t.prompt) = (false, None);
    start["params"].clone()
}

fn assert_defines_profile(params: &Value) {
    let profile = &params["config"][format!("permissions.{PROFILE}")];
    assert_eq!(profile["extends"], ":read-only", "profile: {params}");
    let filesystem = profile["filesystem"].as_object().expect("the profile's filesystem entries");
    assert!(filesystem.values().any(|access| access == "deny"), "no deny entry: {params}");
}

#[test]
fn a_read_only_session_starts_in_the_profile() {
    let translator = translator(CURRENT, "read-only");
    let params = new_session(&translator, json!([]));
    assert_eq!(params["permissions"], PROFILE, "thread/start: {params}");
    assert!(params.get("sandbox").is_none(), "`permissions` cannot go with `sandbox`: {params}");
    assert_eq!(params["approvalPolicy"], "on-request");
    assert_defines_profile(&params);
}

#[test]
fn every_session_defines_the_profile_beside_its_mcp_servers() {
    let translator = translator(CURRENT, "agent");
    let params = new_session(&translator, json!([{"name": "gh", "command": "/bin/gh", "args": ["mcp"], "env": []}]));
    assert_eq!(params["sandbox"], "workspace-write");
    assert!(params.get("permissions").is_none());
    assert_defines_profile(&params);
    assert_eq!(params["config"]["mcp_servers.gh"]["command"], "/bin/gh");
}

#[test]
fn switching_to_read_only_selects_the_profile_and_back_selects_the_sandbox() {
    let translator = translator(CURRENT, "agent");
    new_session(&translator, json!([]));
    set_mode(&translator, "read-only");
    let params = turn(&translator);
    assert_eq!(params["permissions"], PROFILE, "turn/start: {params}");
    assert_eq!(params["approvalPolicy"], "on-request");
    assert!(params.get("sandboxPolicy").is_none(), "`permissions` cannot go with `sandboxPolicy`: {params}");
    // No mode change: no override either.
    let params = turn(&translator);
    assert!(params.get("permissions").is_none() && params.get("sandboxPolicy").is_none(), "{params}");
    set_mode(&translator, "agent");
    let params = turn(&translator);
    assert!(params.get("permissions").is_none(), "{params}");
    assert_eq!(params["sandboxPolicy"]["type"], "workspaceWrite");
}

#[test]
fn a_resumed_or_forked_read_only_session_stays_in_the_profile() {
    let translator = translator(CURRENT, "read-only");
    new_session(&translator, json!([]));
    for method in ["session/resume", "session/load", "session/fork"] {
        let params = server_request(
            translator.from_client(&json!({"jsonrpc": "2.0", "id": 4, "method": method,
                "params": {"sessionId": "t", "cwd": "/w", "mcpServers": []}})),
            if method == "session/fork" { "thread/fork" } else { "thread/resume" },
        )["params"]
            .clone();
        assert_eq!(params["permissions"], PROFILE, "{method}: {params}");
        assert!(params.get("sandbox").is_none(), "{method}: {params}");
        assert_defines_profile(&params);
    }
}

#[test]
fn an_app_server_without_the_profile_check_keeps_the_sandbox_mode() {
    // Before 0.136.0 a deny entry did not keep a rules-allowed command in the
    // sandbox, and `extends` arrived in 0.133.0: such a server gets today's form.
    let translator = translator("codex_cli_rs/0.135.0 (Linux; x86_64)", "read-only");
    let params = new_session(&translator, json!([]));
    assert_eq!(params["sandbox"], "read-only", "{params}");
    assert!(params.get("permissions").is_none() && params.get("config").is_none(), "{params}");
    set_mode(&translator, "agent");
    turn(&translator);
    set_mode(&translator, "read-only");
    let params = turn(&translator);
    assert_eq!(params["sandboxPolicy"]["type"], "readOnly", "{params}");
    assert!(params.get("permissions").is_none());
}
