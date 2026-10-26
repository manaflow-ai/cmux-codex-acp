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

/// ACP `mcpServers` as the app-server's `mcp_servers` config override.
pub fn mcp_config(servers: Option<&Value>) -> Option<Value> {
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
                let mut e = json!({"url": url});
                if !headers.is_empty() {
                    e["http_headers"] = Value::Object(headers);
                }
                e
            }
            _ => {
                let Some(command) = s.get("command").and_then(Value::as_str) else { continue };
                let env = pairs("env", "name", "value");
                let mut e = json!({"command": command, "args": s.get("args").cloned().unwrap_or_else(|| json!([]))});
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
