//! `read-only` runs in a permission profile with a deny entry, so a command a
//! Codex rule allows skips the prompt but not the sandbox. Every path that
//! starts work in a read-only session runs in that profile.

use super::*;

const CURRENT: &str = "acpmux/0.164.0 (Ubuntu 24.4.0; x86_64) unknown (acpmux; 1)";

fn server_requests(outs: &[Out], method: &str) -> Vec<Value> {
    outs.iter()
        .filter_map(|o| match o {
            Out::Server(m) if m["method"] == method => Some(m.clone()),
            _ => None,
        })
        .collect()
}

fn server_request(outs: Vec<Out>, method: &str) -> Value {
    server_requests(&outs, method).pop().unwrap_or_else(|| panic!("no {method} sent: {outs:?}"))
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

/// The id of the read-only profile a thread request defines (its
/// `permissions.<id>` config entry).
fn profile_of(params: &Value) -> String {
    let config = params["config"].as_object().unwrap_or_else(|| panic!("no config: {params}"));
    let ids: Vec<&str> = config.keys().filter_map(|k| k.strip_prefix("permissions.")).collect();
    assert_eq!(ids.len(), 1, "one profile: {params}");
    ids[0].to_owned()
}

/// The `thread/start` of a `session/new`, and the session once Codex answers
/// it, in the profile or sandbox it asked for.
fn new_session(translator: &Translator, mcp: Value) -> Value {
    let start = server_request(
        translator.from_client(&json!({"jsonrpc": "2.0", "id": 1, "method": "session/new",
            "params": {"cwd": "/w", "mcpServers": mcp}})),
        "thread/start",
    );
    let params = &start["params"];
    let mut r = json!({"thread": {"id": "t", "cwd": "/w"}});
    if let Some(id) = params.get("permissions") {
        r["activePermissionProfile"] = json!({"id": id, "extends": ":read-only"});
        r["sandbox"] = json!({"type": "readOnly"});
    } else if params["sandbox"] == "read-only" {
        r["sandbox"] = json!({"type": "readOnly"});
    }
    translator.from_server(&json!({"jsonrpc": "2.0", "id": start["id"], "result": r}));
    params.clone()
}

/// What a `session/set_mode` sent the app-server.
fn set_mode(translator: &Translator, mode: &str) -> Vec<Out> {
    translator.from_client(&json!({"jsonrpc": "2.0", "id": 2, "method": "session/set_mode",
        "params": {"sessionId": "t", "modeId": mode}}))
}

/// What a prompt of `text` sent the app-server; the turn is then over.
fn prompt(translator: &Translator, text: &str) -> Vec<Out> {
    let outs = translator.from_client(&json!({"jsonrpc": "2.0", "id": 3, "method": "session/prompt",
        "params": {"sessionId": "t", "prompt": [{"type": "text", "text": text}]}}));
    let mut state = translator.state.lock().unwrap();
    let t = state.threads.get_mut("t").expect("thread t");
    (t.starting, t.prompt, t.command_turn) = (false, None, false);
    outs
}

fn turn(translator: &Translator) -> Value {
    server_request(prompt(translator, "go"), "turn/start")["params"].clone()
}

fn assert_defines_profile(params: &Value, id: &str) {
    let profile = &params["config"][format!("permissions.{id}")];
    assert_eq!(profile["extends"], ":read-only", "profile: {params}");
    let filesystem = profile["filesystem"].as_object().expect("the profile's filesystem entries");
    assert!(filesystem.values().any(|access| access == "deny"), "no deny entry: {params}");
}

/// The `thread/settings/update` a mode change sent, which must select the
/// read-only profile with the mode's approval policy.
fn assert_selects_profile(outs: &[Out], id: &str) -> Value {
    let update = server_requests(outs, "thread/settings/update").pop().expect("a thread/settings/update");
    let params = &update["params"];
    assert_eq!(params["threadId"], "t");
    assert_eq!(params["permissions"], id, "{params}");
    assert_eq!(params["approvalPolicy"], "on-request");
    assert!(params.get("sandboxPolicy").is_none(), "`permissions` cannot go with `sandboxPolicy`: {params}");
    update
}

#[test]
fn a_read_only_session_starts_in_the_profile() {
    let translator = translator(CURRENT, "read-only");
    let params = new_session(&translator, json!([]));
    let id = profile_of(&params);
    assert!(id.starts_with(config::READ_ONLY_PROFILE), "{id}");
    assert_eq!(params["permissions"], id, "thread/start: {params}");
    assert!(params.get("sandbox").is_none(), "`permissions` cannot go with `sandbox`: {params}");
    assert_eq!(params["approvalPolicy"], "on-request");
    assert_defines_profile(&params, &id);
}

#[test]
fn no_config_can_add_to_the_profile() {
    // Codex merges config tables: a `[permissions.<id>]` of the user's or a
    // project's would add its grants to a profile of the same id.
    let (a, b) = (translator(CURRENT, "read-only"), translator(CURRENT, "read-only"));
    let (a, b) = (profile_of(&new_session(&a, json!([]))), profile_of(&new_session(&b, json!([]))));
    assert_ne!(a, b);
    assert_ne!(a, config::READ_ONLY_PROFILE);
}

#[test]
fn every_session_defines_the_profile_beside_its_mcp_servers() {
    let translator = translator(CURRENT, "agent");
    let params = new_session(&translator, json!([{"name": "gh", "command": "/bin/gh", "args": ["mcp"], "env": []}]));
    assert_eq!(params["sandbox"], "workspace-write");
    assert!(params.get("permissions").is_none());
    assert_defines_profile(&params, &profile_of(&params));
    assert_eq!(params["config"]["mcp_servers.gh"]["command"], "/bin/gh");
}

#[test]
fn switching_to_read_only_selects_the_profile_at_once_and_back_selects_the_sandbox() {
    let translator = translator(CURRENT, "agent");
    let id = profile_of(&new_session(&translator, json!([])));
    let update = assert_selects_profile(&set_mode(&translator, "read-only"), &id);
    // Until Codex took it, a turn carries it too.
    assert_eq!(turn(&translator)["permissions"], id);
    translator.from_server(&json!({"jsonrpc": "2.0", "id": update["id"], "result": {}}));
    // Codex has it: the turn sends no override.
    let params = turn(&translator);
    assert!(params.get("permissions").is_none() && params.get("sandboxPolicy").is_none(), "{params}");
    // The same mode again sends nothing.
    assert!(server_requests(&set_mode(&translator, "read-only"), "thread/settings/update").is_empty());
    let outs = set_mode(&translator, "agent");
    let params = &server_requests(&outs, "thread/settings/update")[0]["params"];
    assert!(params.get("permissions").is_none(), "{params}");
    assert_eq!(params["sandboxPolicy"]["type"], "workspaceWrite");
}

#[test]
fn a_switch_codex_refused_goes_with_the_next_turn() {
    let translator = translator(CURRENT, "agent");
    let id = profile_of(&new_session(&translator, json!([])));
    let update = assert_selects_profile(&set_mode(&translator, "read-only"), &id);
    translator.from_server(&json!({"jsonrpc": "2.0", "id": update["id"], "error": {"code": -32600, "message": "no"}}));
    let params = turn(&translator);
    assert_eq!(params["permissions"], id, "turn/start: {params}");
    assert_eq!(params["approvalPolicy"], "on-request");
    assert!(params.get("sandboxPolicy").is_none(), "{params}");
}

/// The work a command prompt sent the app-server (`review/start`,
/// `thread/goal/set`), if any.
fn work(outs: &[Out]) -> Option<Value> {
    outs.iter().find_map(|o| match o {
        Out::Server(m) if m["method"] == "review/start" || m["method"] == "thread/goal/set" => Some(m.clone()),
        _ => None,
    })
}

fn answer(translator: &Translator, request: &Value, ok: bool) -> Vec<Out> {
    let reply = if ok {
        json!({"jsonrpc": "2.0", "id": request["id"], "result": {}})
    } else {
        json!({"jsonrpc": "2.0", "id": request["id"], "error": {"code": -32600, "message": "profile refused"}})
    };
    translator.from_server(&reply)
}

/// The prompt's answer among `outs` (request id 3).
fn prompt_answer(outs: &[Out]) -> Option<Value> {
    outs.iter().find_map(|o| match o {
        Out::Client(m) if m["id"] == 3 && m.get("method").is_none() => Some(m.clone()),
        _ => None,
    })
}

#[test]
fn a_review_or_goal_starts_only_once_codex_took_the_mode() {
    for command in ["/review", "/goal tidy up"] {
        let translator = translator(CURRENT, "agent");
        let id = profile_of(&new_session(&translator, json!([])));
        // The switch is still on its way: the work waits for it.
        let update = assert_selects_profile(&set_mode(&translator, "read-only"), &id);
        let outs = translator.from_client(&json!({"jsonrpc": "2.0", "id": 3, "method": "session/prompt",
            "params": {"sessionId": "t", "prompt": [{"type": "text", "text": command}]}}));
        assert!(work(&outs).is_none(), "{command} started before Codex took the mode: {outs:?}");
        let outs = answer(&translator, &update, true);
        assert!(work(&outs).is_some(), "{command} did not start once Codex took the mode: {outs:?}");
    }
}

#[test]
fn a_review_or_goal_never_starts_in_the_old_mode() {
    for command in ["/review", "/goal tidy up"] {
        let translator = translator(CURRENT, "agent");
        let id = profile_of(&new_session(&translator, json!([])));
        let update = assert_selects_profile(&set_mode(&translator, "read-only"), &id);
        answer(&translator, &update, false);
        // The command sends the mode again and waits for it.
        let outs = translator.from_client(&json!({"jsonrpc": "2.0", "id": 3, "method": "session/prompt",
            "params": {"sessionId": "t", "prompt": [{"type": "text", "text": command}]}}));
        assert!(work(&outs).is_none(), "{command} started before Codex took the mode: {outs:?}");
        let retry = assert_selects_profile(&outs, &id);
        // Refused again: the prompt fails and nothing starts.
        let outs = answer(&translator, &retry, false);
        assert!(work(&outs).is_none(), "{command} started in the old mode: {outs:?}");
        let answer = prompt_answer(&outs).unwrap_or_else(|| panic!("{command}: the prompt was not answered: {outs:?}"));
        assert!(answer.get("error").is_some(), "{command}: {answer}");
        let t = translator.state.lock().unwrap();
        let t = &t.threads["t"];
        assert!(t.prompt.is_none() && !t.starting, "{command}: {t:?}");
    }
}

#[test]
fn a_cancel_drops_work_that_waits_for_the_mode() {
    let translator = translator(CURRENT, "agent");
    let id = profile_of(&new_session(&translator, json!([])));
    let update = assert_selects_profile(&set_mode(&translator, "read-only"), &id);
    translator.from_client(&json!({"jsonrpc": "2.0", "id": 3, "method": "session/prompt",
        "params": {"sessionId": "t", "prompt": [{"type": "text", "text": "/review"}]}}));
    let outs =
        translator.from_client(&json!({"jsonrpc": "2.0", "method": "session/cancel", "params": {"sessionId": "t"}}));
    let answer_ = prompt_answer(&outs).expect("the prompt's answer");
    assert_eq!(answer_["result"]["stopReason"], "cancelled");
    assert!(work(&answer(&translator, &update, true)).is_none(), "a cancelled review started");
}

#[test]
fn work_waits_for_the_last_mode_picked_not_an_older_one() {
    // Read-only, then back to agent, both on their way; a review waits.
    let translator = translator(CURRENT, "agent");
    let id = profile_of(&new_session(&translator, json!([])));
    let older = assert_selects_profile(&set_mode(&translator, "read-only"), &id);
    let newer = server_request(set_mode(&translator, "agent"), "thread/settings/update");
    let outs = translator.from_client(&json!({"jsonrpc": "2.0", "id": 3, "method": "session/prompt",
        "params": {"sessionId": "t", "prompt": [{"type": "text", "text": "/review"}]}}));
    assert!(work(&outs).is_none(), "the review started before Codex took the mode: {outs:?}");
    // The older pick is refused: the review still waits for the newer one.
    let outs = answer(&translator, &older, false);
    assert!(prompt_answer(&outs).is_none() && work(&outs).is_none(), "{outs:?}");
    let outs = answer(&translator, &newer, true);
    assert!(work(&outs).is_some(), "the review did not start once Codex took the mode: {outs:?}");
}

/// A `session/resume` of `t` sent now, answered by `answer_resume` with
/// Codex's view of the thread.
fn resume_request(translator: &Translator) -> Value {
    server_request(
        translator.from_client(&json!({"jsonrpc": "2.0", "id": 4, "method": "session/resume",
            "params": {"sessionId": "t", "cwd": "/w", "mcpServers": []}})),
        "thread/resume",
    )
}

fn answer_resume(translator: &Translator, request: &Value, view: Value) -> Vec<Out> {
    let mut r = json!({"thread": {"id": "t", "cwd": "/w"}});
    r.as_object_mut().unwrap().extend(view.as_object().unwrap().clone());
    translator.from_server(&json!({"jsonrpc": "2.0", "id": request["id"], "result": r}))
}

#[test]
fn a_mode_picked_while_a_resume_is_on_its_way_stays_the_sessions_mode() {
    // Read-only, resumed; full access picked before the resume answers.
    let translator = translator(CURRENT, "read-only");
    let id = profile_of(&new_session(&translator, json!([])));
    let resume = resume_request(&translator);
    let update = server_request(set_mode(&translator, "agent-full-access"), "thread/settings/update");
    let outs = answer_resume(
        &translator,
        &resume,
        json!({"approvalPolicy": "on-request",
        "sandbox": {"type": "readOnly"}, "activePermissionProfile": {"id": id, "extends": ":read-only"}}),
    );
    assert!(server_requests(&outs, "thread/settings/update").is_empty(), "a second update: {outs:?}");
    answer(&translator, &update, true);
    assert_eq!(translator.state.lock().unwrap().threads["t"].mode, "agent-full-access");

    // Full access, resumed; read-only picked before the resume answers with
    // the thread as it was: the session is read-only, and Codex has it once
    // the update is through.
    let translator = self::translator(CURRENT, "agent-full-access");
    new_session(&translator, json!([]));
    let resume = resume_request(&translator);
    let update = assert_selects_profile(&set_mode(&translator, "read-only"), &id_of(&translator));
    let outs = answer_resume(
        &translator,
        &resume,
        json!({"approvalPolicy": "never",
        "sandbox": {"type": "dangerFullAccess"}}),
    );
    assert!(server_requests(&outs, "thread/settings/update").is_empty(), "a second update: {outs:?}");
    answer(&translator, &update, true);
    assert_eq!(translator.state.lock().unwrap().threads["t"].mode, "read-only");
    let params = turn(&translator);
    assert!(params.get("permissions").is_none() && params.get("sandboxPolicy").is_none(), "{params}");
}

/// The read-only profile id `translator` defines.
fn id_of(translator: &Translator) -> String {
    translator.state.lock().unwrap().read_only_profile.clone()
}

#[test]
fn a_resumed_or_forked_read_only_session_stays_in_the_profile() {
    let translator = translator(CURRENT, "read-only");
    let id = profile_of(&new_session(&translator, json!([])));
    for method in ["session/resume", "session/load", "session/fork"] {
        let params = server_request(
            translator.from_client(&json!({"jsonrpc": "2.0", "id": 4, "method": method,
                "params": {"sessionId": "t", "cwd": "/w", "mcpServers": []}})),
            if method == "session/fork" { "thread/fork" } else { "thread/resume" },
        )["params"]
            .clone();
        assert_eq!(params["permissions"], id, "{method}: {params}");
        assert!(params.get("sandbox").is_none(), "{method}: {params}");
        assert_defines_profile(&params, &id);
    }
}

#[test]
fn a_resume_codex_did_not_apply_is_applied_after_it() {
    let translator = translator(CURRENT, "read-only");
    let id = profile_of(&new_session(&translator, json!([])));
    let resume = |result: Value| {
        let request = server_request(
            translator.from_client(&json!({"jsonrpc": "2.0", "id": 4, "method": "session/resume",
                "params": {"sessionId": "t", "cwd": "/w", "mcpServers": []}})),
            "thread/resume",
        );
        let mut r = json!({"thread": {"id": "t", "cwd": "/w"}});
        r.as_object_mut().unwrap().extend(result.as_object().unwrap().clone());
        translator.from_server(&json!({"jsonrpc": "2.0", "id": request["id"], "result": r}))
    };
    // Still loaded in Codex in another mode: it kept that one.
    let outs = resume(json!({"approvalPolicy": "on-request", "sandbox": {"type": "workspaceWrite"}}));
    assert_selects_profile(&outs, &id);
    // Resumed in the profile: nothing more to send.
    let outs = resume(json!({"approvalPolicy": "on-request", "sandbox": {"type": "readOnly"},
        "activePermissionProfile": {"id": id, "extends": ":read-only"}}));
    assert!(server_requests(&outs, "thread/settings/update").is_empty(), "{outs:?}");
}

#[test]
fn a_resume_never_widens_what_codex_runs() {
    // An agent session whose thread Codex still runs read-only stays so.
    let translator = translator(CURRENT, "agent");
    new_session(&translator, json!([]));
    let request = server_request(
        translator.from_client(&json!({"jsonrpc": "2.0", "id": 4, "method": "session/resume",
            "params": {"sessionId": "t", "cwd": "/w", "mcpServers": []}})),
        "thread/resume",
    );
    let outs = translator.from_server(&json!({"jsonrpc": "2.0", "id": request["id"], "result": {
        "thread": {"id": "t", "cwd": "/w"}, "approvalPolicy": "untrusted", "sandbox": {"type": "readOnly"}}}));
    assert!(server_requests(&outs, "thread/settings/update").is_empty(), "{outs:?}");
}

#[test]
fn a_start_or_fork_codex_did_not_put_in_the_profile_is_moved_into_it() {
    // Managed requirements can make Codex start a thread in another profile
    // than the one asked for, with no error.
    let translator = translator(CURRENT, "read-only");
    let start = server_request(
        translator.from_client(&json!({"jsonrpc": "2.0", "id": 1, "method": "session/new",
            "params": {"cwd": "/w", "mcpServers": []}})),
        "thread/start",
    );
    let id = profile_of(&start["params"]);
    let elsewhere = json!({"approvalPolicy": "on-request", "sandbox": {"type": "readOnly"},
        "activePermissionProfile": {"id": ":read-only"}});
    let mut r = json!({"thread": {"id": "t", "cwd": "/w"}});
    r.as_object_mut().unwrap().extend(elsewhere.as_object().unwrap().clone());
    let outs = translator.from_server(&json!({"jsonrpc": "2.0", "id": start["id"], "result": r}));
    let update = assert_selects_profile(&outs, &id);
    answer(&translator, &update, true);

    let fork = server_request(
        translator.from_client(&json!({"jsonrpc": "2.0", "id": 4, "method": "session/fork",
            "params": {"sessionId": "t", "cwd": "/w", "mcpServers": []}})),
        "thread/fork",
    );
    let mut r = json!({"thread": {"id": "f", "cwd": "/w"}});
    r.as_object_mut().unwrap().extend(elsewhere.as_object().unwrap().clone());
    let outs = translator.from_server(&json!({"jsonrpc": "2.0", "id": fork["id"], "result": r}));
    let update = server_request(outs, "thread/settings/update");
    assert_eq!(update["params"]["threadId"], "f", "{update}");
    assert_eq!(update["params"]["permissions"], id, "{update}");
}

#[test]
fn a_title_thread_codex_did_not_put_in_the_profile_never_runs() {
    for (active, runs) in [(None, false), (Some(":read-only"), false), (Some("the profile"), true)] {
        let translator = translator(CURRENT, "agent");
        let id = profile_of(&new_session(&translator, json!([])));
        let mut outs = Vec::new();
        translator.state.lock().unwrap().generate_title("t", Some("fix the build".into()), &mut outs);
        let start = server_request(outs, "thread/start");
        let mut r = json!({"thread": {"id": "title", "cwd": "/w"}, "approvalPolicy": "never",
            "sandbox": {"type": "readOnly"}});
        if let Some(active) = active {
            r["activePermissionProfile"] = json!({"id": if runs { id.as_str() } else { active }});
        }
        let outs = translator.from_server(&json!({"jsonrpc": "2.0", "id": start["id"], "result": r}));
        assert_eq!(!server_requests(&outs, "turn/start").is_empty(), runs, "{active:?}: {outs:?}");
    }
}

#[test]
fn the_title_thread_runs_read_only_and_asks_nothing() {
    let translator = translator(CURRENT, "agent-full-access");
    let id = profile_of(&new_session(&translator, json!([])));
    let mut outs = Vec::new();
    translator.state.lock().unwrap().generate_title("t", Some("fix the build".into()), &mut outs);
    let params = server_request(outs, "thread/start")["params"].clone();
    assert_eq!(params["ephemeral"], true);
    assert_eq!(params["approvalPolicy"], "never", "{params}");
    assert_eq!(params["permissions"], id, "{params}");
    assert_defines_profile(&params, &id);
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
    let outs = set_mode(&translator, "read-only");
    let params = &server_requests(&outs, "thread/settings/update")[0]["params"];
    assert_eq!(params["sandboxPolicy"]["type"], "readOnly", "{params}");
    assert!(params.get("permissions").is_none());
}
