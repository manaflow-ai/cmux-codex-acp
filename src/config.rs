//! Session modes and config options: what a client may pick, and what each
//! pick sends to the app-server.

use serde_json::{Value, json};

/// One session mode: an approval policy and a sandbox.
pub struct Mode {
    pub id: &'static str,
    pub name: &'static str,
    pub description: &'static str,
    /// `AskForApproval` for `thread/start` and `turn/start`.
    pub approval: &'static str,
    /// `SandboxMode` for `thread/start`.
    pub sandbox: &'static str,
}

/// The modes, in picker order. `read-only`, `agent` and `agent-full-access`
/// keep the ids the codex-acp adapter used, so a session saved there keeps
/// its mode here. `ask` is new: Codex asks before every command that is not
/// a known-safe read and before every file change, and the client's
/// permission policy answers each.
pub const MODES: [Mode; 4] = [
    Mode {
        id: "ask",
        name: "Ask",
        description: "Asks before every command and every edit",
        approval: "untrusted",
        sandbox: "workspace-write",
    },
    Mode {
        id: "read-only",
        name: "Read Only",
        description: "Reads files; asks before any edit or command that needs more",
        approval: "on-request",
        sandbox: "read-only",
    },
    Mode {
        id: "agent",
        name: "Agent",
        description: "Edits and runs commands in the workspace; asks to go outside it",
        approval: "on-request",
        sandbox: "workspace-write",
    },
    Mode {
        id: "agent-full-access",
        name: "Agent (full access)",
        description: "Edits and runs anything without asking",
        approval: "never",
        sandbox: "danger-full-access",
    },
];

pub const DEFAULT_MODE: &str = "agent";

pub fn mode(id: &str) -> Option<&'static Mode> {
    MODES.iter().find(|m| m.id == id)
}

/// The permission profile `read-only` selects on an app-server that has it
/// ([`READ_ONLY_PROFILE_VERSION`]): Codex's `:read-only` plus one deny entry.
/// Its id is [`READ_ONLY_PROFILE`] plus a random suffix per translator, so no
/// profile of the user's or a project's config can merge grants into it.
///
/// A command that a Codex rule (`~/.codex/rules`) explicitly allows skips the
/// approval prompt, and with the plain `read-only` sandbox mode it also runs
/// outside the sandbox. A profile with a deny entry keeps every command in the
/// sandbox (Codex's `unsandboxed_execution_allowed`), so the rule skips only
/// the prompt. The entry is a glob under a folder that does not exist: it
/// hides nothing, and Linux's bubblewrap skips a glob with no matches (a
/// missing exact path would be mounted over, and fail, in an escalated run).
///
/// Codex keeps a deny entry for the rest of the thread: after `read-only`,
/// `ask` and `agent` also run rules-allowed commands in their sandbox, and an
/// approved escalation there writes anywhere but has no network.
/// `agent-full-access` drops it.
pub const READ_ONLY_PROFILE: &str = "cmux-read-only";
const READ_ONLY_PROFILE_DENY: &str = "/nonexistent-cmux-codex-acp/*";

/// The first app-server in which a profile's deny entry keeps a
/// rules-allowed command in the sandbox. An older one gets the `read-only`
/// sandbox mode.
pub const READ_ONLY_PROFILE_VERSION: (u64, u64, u64) = (0, 136, 0);

/// A fresh id for the read-only profile.
pub fn read_only_profile_id() -> String {
    use std::hash::{BuildHasher, Hasher};
    // `RandomState` is seeded from the OS's randomness.
    let random = std::collections::hash_map::RandomState::new().build_hasher().finish();
    format!("{READ_ONLY_PROFILE}-{random:016x}")
}

/// The `config` entry that defines the read-only profile `id`. Every thread
/// gets it, so a later turn can switch into `read-only`.
pub fn read_only_profile(id: &str) -> (String, Value) {
    (format!("permissions.{id}"), json!({"extends": ":read-only", "filesystem": {READ_ONLY_PROFILE_DENY: "deny"}}))
}

/// `SandboxPolicy` for `turn/start` (the per-turn override takes the policy
/// form, not the mode name).
pub fn sandbox_policy(mode: &Mode) -> Value {
    match mode.sandbox {
        "read-only" => json!({"type": "readOnly", "networkAccess": false}),
        "danger-full-access" => json!({"type": "dangerFullAccess"}),
        _ => {
            json!({"type": "workspaceWrite", "writableRoots": [], "networkAccess": false, "excludeTmpdirEnvVar": false, "excludeSlashTmp": false})
        }
    }
}

pub fn modes_value(current: &str) -> Value {
    json!({
        "currentModeId": current,
        "availableModes": MODES.iter().map(|m| json!({"id": m.id, "name": m.name, "description": m.description})).collect::<Vec<_>>(),
    })
}

/// A model as `model/list` reports it.
#[derive(Debug, Clone)]
pub struct ModelInfo {
    pub id: String,
    pub name: String,
    pub description: String,
    pub efforts: Vec<(String, String)>,
    pub is_default: bool,
    /// The effort the model runs at when none is picked (`defaultReasoningEffort`).
    pub default_effort: Option<String>,
    /// What the model takes as input (`inputModalities`: `text`, `image`,
    /// `audio`); empty when the app-server does not say.
    pub input_modalities: Vec<String>,
    pub service_tiers: Vec<Value>,
    pub default_service_tier: Option<String>,
}

impl ModelInfo {
    /// Whether the model hears audio input. Codex replaces audio with an
    /// "unsupported media" note for any model that does not list it.
    pub fn takes_audio(&self) -> bool {
        self.input_modalities.iter().any(|m| m == "audio")
    }
}

pub fn parse_models(result: &Value) -> Vec<ModelInfo> {
    let s = |v: &Value, k: &str| v.get(k).and_then(Value::as_str).unwrap_or("").to_owned();
    result
        .get("data")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|m| m.get("hidden").and_then(Value::as_bool) != Some(true))
        .map(|m| ModelInfo {
            id: s(m, "id"),
            name: Some(s(m, "displayName")).filter(|n| !n.is_empty()).unwrap_or_else(|| s(m, "id")),
            description: s(m, "description"),
            efforts: m
                .get("supportedReasoningEfforts")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(|e| Some((e.get("reasoningEffort")?.as_str()?.to_owned(), s(e, "description"))))
                .collect(),
            is_default: m.get("isDefault").and_then(Value::as_bool).unwrap_or(false),
            default_effort: m.get("defaultReasoningEffort").and_then(Value::as_str).map(str::to_owned),
            input_modalities: m
                .get("inputModalities")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(|v| v.as_str().map(str::to_owned))
                .collect(),
            service_tiers: crate::speed::model_tiers(m),
            default_service_tier: m.get("defaultServiceTier").and_then(Value::as_str).map(str::to_owned),
        })
        .collect()
}

/// The effort id that means "the model's own default".
pub const DEFAULT_EFFORT: &str = "default";

fn effort_name(id: &str) -> String {
    match id {
        "xhigh" => "Extra High".into(),
        DEFAULT_EFFORT => "Default".into(),
        other => {
            let mut c = other.chars();
            c.next().map(|f| f.to_uppercase().chain(c).collect()).unwrap_or_default()
        }
    }
}

/// The config options of one session: model, reasoning effort, mode.
pub fn config_options(models: &[ModelInfo], model: &str, effort: &str, mode: &str) -> Value {
    // Each option names its input modalities, so a client can tell which
    // models hear the audio `promptCapabilities.audio` lets it send.
    let mut model_opts: Vec<Value> = models
        .iter()
        .map(|m| {
            let mut opt = json!({"value": m.id, "name": m.name, "description": m.description});
            if !m.input_modalities.is_empty() {
                opt["_meta"] = json!({"inputModalities": m.input_modalities});
            }
            opt
        })
        .collect();
    if !model.is_empty() && !models.iter().any(|m| m.id == model) {
        model_opts.insert(0, json!({"value": model, "name": model}));
    }
    let mut efforts = vec![json!({"value": DEFAULT_EFFORT, "name": "Default", "description": "The model's own level"})];
    if let Some(m) = models.iter().find(|m| m.id == model) {
        efforts.extend(m.efforts.iter().map(|(id, d)| json!({"value": id, "name": effort_name(id), "description": d})));
    } else if effort != DEFAULT_EFFORT {
        efforts.push(json!({"value": effort, "name": effort_name(effort)}));
    }
    json!([
        {"id": "model", "name": "Model", "type": "select", "category": "model", "currentValue": model, "options": model_opts},
        {"id": "reasoning_effort", "name": "Reasoning effort", "type": "select", "category": "thought_level", "currentValue": effort, "options": efforts},
        {"id": "mode", "name": "Approval mode", "type": "select", "category": "mode", "currentValue": mode,
         "options": MODES.iter().map(|m| json!({"value": m.id, "name": m.name, "description": m.description})).collect::<Vec<_>>()},
    ])
}

/// The legacy `models` field (`session/set_model`) of a session response.
pub fn models_value(models: &[ModelInfo], model: &str) -> Value {
    json!({
        "currentModelId": model,
        "availableModels": models.iter().map(|m| json!({"modelId": m.id, "name": m.name, "description": m.description})).collect::<Vec<_>>(),
    })
}

/// A model pick as `(model, effort)`. The codex-acp adapter named a model
/// and effort together as `model/effort`; such a saved value still works.
pub fn split_model(value: &str, models: &[ModelInfo]) -> (String, Option<String>) {
    // A known model id keeps its slash (`provider/model`).
    if models.iter().any(|m| m.id == value) {
        return (value.to_owned(), None);
    }
    const EFFORTS: [&str; 8] = ["none", "minimal", "low", "medium", "high", "xhigh", "max", "ultra"];
    match value.rsplit_once('/') {
        Some((m, e)) if !m.is_empty() && EFFORTS.contains(&e) => (m.to_owned(), Some(e.to_owned())),
        _ => (value.to_owned(), None),
    }
}

/// ACP `mcpServers` as the app-server's `mcp_servers` config override, in the session's folder
/// `cwd`. Codex merges the override into an entry of the same name its config layers make, key by
/// key, so each server also gets `enabled = true` and a stdio one `cwd`: a person's own entry of
/// that name, turned off or with a folder of its own, never keeps the session's server off or
/// starts it elsewhere. Their other keys still merge (`enabled_tools`, `disabled_tools`, timeouts,
/// extra `env`). (Codex refuses a command beside a `url` of theirs; the client leaves such a
/// server out.)
pub fn mcp_config(servers: Option<&Value>, cwd: Option<&Value>) -> Option<Value> {
    let servers = servers?.as_array()?;
    let mut out = serde_json::Map::new();
    for s in servers {
        let Some(name) = s.get("name").and_then(Value::as_str) else { continue };
        let pairs = |key: &str, k: &str, v: &str| -> serde_json::Map<String, Value> {
            s.get(key)
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(|e| Some((e.get(k)?.as_str()?.to_owned(), json!(e.get(v)?.as_str()?))))
                .collect()
        };
        let entry = match s.get("type").and_then(Value::as_str) {
            Some("http") | Some("sse") => {
                let Some(url) = s.get("url").and_then(Value::as_str) else { continue };
                let headers = pairs("headers", "name", "value");
                let mut e = json!({"url": url, "enabled": true});
                if !headers.is_empty() {
                    e["http_headers"] = Value::Object(headers);
                }
                e
            }
            _ => {
                let Some(command) = s.get("command").and_then(Value::as_str) else { continue };
                let env = pairs("env", "name", "value");
                let mut e = json!({"command": command, "args": s.get("args").cloned().unwrap_or_else(|| json!([])), "enabled": true});
                if let Some(cwd) = cwd.filter(|c| c.is_string()) {
                    e["cwd"] = cwd.clone();
                }
                if !env.is_empty() {
                    e["env"] = Value::Object(env);
                }
                e
            }
        };
        // Dotted keys add these servers beside the user's own; a whole
        // `mcp_servers` table would replace them.
        out.insert(format!("mcp_servers.{name}"), entry);
    }
    (!out.is_empty()).then_some(Value::Object(out))
}

/// `_meta.cmux.mcpServerEnv` on a session request (`{name: {KEY: value}}`) as `mcp_servers.<name>.env.<KEY>`
/// config overrides, added to `base` (from [`mcp_config`]): env keys for a person's own server of
/// that name, which Codex merges into its entry. The client sends it only for a server it gives no
/// definition of, and only where the person's config has a stdio entry of that name (an override
/// with no entry to merge into, or into an HTTP entry, fails the whole thread: Codex refuses a
/// server with no command, and `env` beside a `url`; that check is the client's). Only string values;
/// a name or key that is not a plain identifier (`[A-Za-z_][A-Za-z0-9_-]*` for a name,
/// `[A-Za-z_][A-Za-z0-9_]*` for a key) is skipped, so neither can name another config path.
pub fn mcp_server_env(base: Option<Value>, meta: Option<&Value>) -> Option<Value> {
    let mut out = base.and_then(|b| b.as_object().cloned()).unwrap_or_default();
    let ident = |s: &str, dash: bool| {
        let mut chars = s.chars();
        chars.next().is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
            && chars.all(|c| c.is_ascii_alphanumeric() || c == '_' || (dash && c == '-'))
    };
    for (name, env) in meta.and_then(Value::as_object).into_iter().flatten() {
        if !ident(name, true) || out.contains_key(&format!("mcp_servers.{name}")) {
            continue;
        }
        for (key, value) in env.as_object().into_iter().flatten() {
            if let (true, Some(value)) = (ident(key, false), value.as_str()) {
                out.insert(format!("mcp_servers.{name}.env.{key}"), json!(value));
            }
        }
    }
    (!out.is_empty()).then_some(Value::Object(out))
}

/// `_meta.cmux.mcpServerEntry` on a session request (`{name: {command, args, env: {KEY: value}}}`)
/// as `mcp_servers.<name>.command`, `.args` and `.env.<KEY>` config overrides, added to `base`:
/// a person's own stdio server of that name, as the client read it from the config files Codex
/// loads, with env keys added. Where the entry loads, Codex merges the same `command` and `args`
/// and the keys into it; where it does not, the overrides are still a whole stdio server, never
/// one with no command, which Codex refuses ("invalid transport"). Its other keys (`cwd`,
/// `enabled`, timeouts) stay the person's. Skipped: a server the session defines itself, an
/// entry with no string `command`, `args` that is not a list of strings, and names and keys as
/// for [`mcp_server_env`].
pub fn mcp_server_entry(base: Option<Value>, meta: Option<&Value>) -> Option<Value> {
    let mut out = base.and_then(|b| b.as_object().cloned()).unwrap_or_default();
    let ident = |s: &str, dash: bool| {
        let mut chars = s.chars();
        chars.next().is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
            && chars.all(|c| c.is_ascii_alphanumeric() || c == '_' || (dash && c == '-'))
    };
    for (name, entry) in meta.and_then(Value::as_object).into_iter().flatten() {
        if !ident(name, true) || out.contains_key(&format!("mcp_servers.{name}")) {
            continue;
        }
        let Some(command) = entry.get("command").and_then(Value::as_str) else { continue };
        let args = match entry.get("args") {
            None => Some(vec![]),
            Some(Value::Array(list)) => list.iter().map(|a| a.as_str().map(str::to_owned)).collect(),
            Some(_) => None,
        };
        let Some(args) = args else { continue };
        out.insert(format!("mcp_servers.{name}.command"), json!(command));
        out.insert(format!("mcp_servers.{name}.args"), json!(args));
        for (key, value) in entry.get("env").and_then(Value::as_object).into_iter().flatten() {
            if let (true, Some(value)) = (ident(key, false), value.as_str()) {
                out.insert(format!("mcp_servers.{name}.env.{key}"), json!(value));
            }
        }
    }
    (!out.is_empty()).then_some(Value::Object(out))
}

#[cfg(test)]
mod mcp_server_entry_tests {
    use super::*;

    /// A person's own entry goes in whole: its command, args and the env keys; a server the
    /// session defines, an entry with no string command or with non-string args is skipped.
    #[test]
    fn a_persons_own_entry_goes_in_whole_with_its_env_keys() {
        let meta = json!({"cmux": {"command": "/opt/cmux", "args": ["mcp", "serve"],
                                   "env": {"CMUX_AGENT_TOOLS_ANSWER": "off", "A.B": "x", "N": 1}},
                          "nocmd": {"args": []}, "badargs": {"command": "/x", "args": [1]}});
        let cfg = mcp_server_entry(None, Some(&meta)).unwrap();
        assert_eq!(
            cfg,
            json!({"mcp_servers.cmux.command": "/opt/cmux", "mcp_servers.cmux.args": ["mcp", "serve"],
                   "mcp_servers.cmux.env.CMUX_AGENT_TOOLS_ANSWER": "off"})
        );
        let servers = json!([{"name": "cmux", "command": "/bin/c", "args": [], "env": []}]);
        let both = mcp_server_entry(mcp_config(Some(&servers), None), Some(&meta)).unwrap();
        assert!(both.get("mcp_servers.cmux.command").is_none(), "{both}");
        assert_eq!(
            mcp_server_entry(None, Some(&json!({"cmux": {"command": "/x"}}))).unwrap()["mcp_servers.cmux.args"],
            json!([])
        );
        assert_eq!(mcp_server_entry(None, None), None);
    }
}

#[cfg(test)]
mod mcp_server_env_tests {
    use super::*;

    /// Env keys for a person's own server become dotted `env` overrides; a server the session
    /// defines itself, a dotted name or key, and a non-string value are skipped.
    #[test]
    fn env_keys_merge_into_a_persons_own_entry() {
        let meta = json!({"cmux": {"CMUX_AGENT_SESSION": "s-1", "CMUX_AGENT_TOOLS_ANSWER": "off", "N": 1, "A.B": "x"},
                          "a.b": {"K": "v"}});
        let cfg = mcp_server_env(None, Some(&meta)).unwrap();
        assert_eq!(
            cfg,
            json!({"mcp_servers.cmux.env.CMUX_AGENT_SESSION": "s-1", "mcp_servers.cmux.env.CMUX_AGENT_TOOLS_ANSWER": "off"})
        );
        // A server the session gives keeps its own definition; nothing is added for it.
        let servers = json!([{"name": "cmux", "command": "/bin/c", "args": [], "env": []}]);
        let both = mcp_server_env(mcp_config(Some(&servers), None), Some(&meta)).unwrap();
        assert!(both.get("mcp_servers.cmux.env.CMUX_AGENT_SESSION").is_none(), "{both}");
        assert_eq!(both["mcp_servers.cmux"]["command"], "/bin/c");
        assert_eq!(mcp_server_env(None, None), None);
        assert_eq!(mcp_server_env(None, Some(&json!({"cmux": {}}))), None);
        // No name or key that is not a plain identifier, no env that is not an object.
        let odd =
            json!({"cmux": {"": "x", "K\"": "x", "K=V": "x", "1K": "x", "K K": "x"}, "a\"b": {"K": "v"}, "c": "x"});
        assert_eq!(mcp_server_env(None, Some(&odd)), None);
        // An HTTP server the session gives under that name keeps its definition too.
        let http = json!([{"name": "cmux", "type": "http", "url": "http://127.0.0.1:1/mcp", "headers": []}]);
        let cfg = mcp_server_env(mcp_config(Some(&http), None), Some(&meta)).unwrap();
        assert_eq!(cfg, json!({"mcp_servers.cmux": {"url": "http://127.0.0.1:1/mcp", "enabled": true}}));
    }
}

#[cfg(test)]
mod mcp_config_tests {
    use super::*;

    /// Without a session folder no `cwd` is sent (the person's entry keeps its own), and an `sse`
    /// server, like an `http` one, gets `enabled` but never a `cwd`.
    #[test]
    fn no_session_folder_sends_no_cwd_and_remote_servers_never_get_one() {
        let servers = json!([
            {"name": "a", "command": "/bin/a", "args": [], "env": []},
            {"name": "s", "type": "sse", "url": "http://127.0.0.1:1/sse", "headers": []},
        ]);
        let none = mcp_config(Some(&servers), None).unwrap();
        assert_eq!(none["mcp_servers.a"], json!({"command": "/bin/a", "args": [], "enabled": true}));
        let with = mcp_config(Some(&servers), Some(&json!("/w"))).unwrap();
        assert_eq!(with["mcp_servers.a"]["cwd"], "/w");
        assert_eq!(with["mcp_servers.s"], json!({"url": "http://127.0.0.1:1/sse", "enabled": true}));
        assert!(mcp_config(Some(&servers), Some(&json!(7))).unwrap()["mcp_servers.a"].get("cwd").is_none());
    }
}
