//! The translator: state of one app-server connection and every session on
//! it.

mod client;
mod history;
mod requests;
mod server;

use crate::config::{self, ModelInfo};
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};
use std::sync::Mutex;

/// A message to write: to the app-server's stdin, or to the ACP client.
#[derive(Debug, Clone, PartialEq)]
pub enum Out {
    Server(Value),
    Client(Value),
}

/// How the translator presents itself and starts sessions.
#[derive(Debug, Clone)]
pub struct Options {
    /// `clientInfo` for the app-server's `initialize` (its user agent and
    /// the `originator` of every thread it records).
    pub client_name: String,
    pub client_title: String,
    pub client_version: String,
    /// The mode a new session starts in (`config::MODES`).
    pub default_mode: String,
    /// The model a new session starts with; None leaves Codex's default.
    pub model: Option<String>,
    /// The reasoning effort a new session starts with.
    pub effort: Option<String>,
    /// Advertise and translate one-shot ACP audio input. This is opt-in, and
    /// it applies only to an app-server whose `userAgent` names a version at
    /// or after [`AUDIO_INPUT_VERSION`]: older ones reject the `audio` input
    /// variant, so an unknown version gets none.
    pub audio_input: bool,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            client_name: "cmux-codex-acp".into(),
            client_title: "cmux".into(),
            client_version: env!("CARGO_PKG_VERSION").into(),
            default_mode: config::DEFAULT_MODE.into(),
            model: None,
            effort: None,
            audio_input: false,
        }
    }
}

/// What a request the translator sent to the app-server is for.
#[derive(Debug, Clone)]
enum Pending {
    Initialize,
    ModelList,
    /// `session/new`: answer the client once the thread exists.
    ThreadStart {
        client: Value,
        mode: String,
        effort: String,
    },
    /// `session/load` (replay) or `session/resume` (no replay).
    ThreadResume {
        client: Value,
        session: String,
        mode: String,
        replay: bool,
    },
    ThreadFork {
        client: Value,
        mode: String,
    },
    ThreadList {
        client: Value,
    },
    TurnStart {
        client: Value,
        thread: String,
    },
    TurnSteer {
        client: Value,
    },
    /// A request whose answer only needs forwarding as `{}` (or its error).
    Ack {
        client: Value,
    },
    /// Fire and forget (`turn/interrupt`, `thread/unsubscribe` on close).
    Ignore,
}

/// A permission request the translator sent to the client for a server
/// request.
#[derive(Debug, Clone)]
struct Asked {
    /// The thread (ACP session) it was asked in.
    thread: String,
    /// The app-server request this answers.
    server_id: Value,
    /// ACP option id -> the decision value sent back.
    decisions: Vec<(String, Value)>,
    /// The decision for a cancelled prompt.
    on_cancel: Value,
    /// How the answer is shaped: `{"decision": d}` or a whole result.
    shape: Shape,
}

#[derive(Debug, Clone)]
enum Shape {
    Decision,
    /// `item/permissions/requestApproval`: grant the asked profile.
    Permissions {
        asked: Value,
    },
    /// `item/tool/requestUserInput` with one question: the answer label.
    Question {
        question_id: String,
    },
    /// The decision is the whole answer (an MCP tool approval elicitation).
    Whole,
}

/// One Codex thread: an ACP session, or a subagent of one.
#[derive(Debug, Default)]
struct Thread {
    /// The parent thread when this is a subagent.
    parent: Option<String>,
    mode: String,
    /// The mode the app-server applies now (set at thread start and on each
    /// turn that changed it): a turn sends approval and sandbox overrides
    /// only when `mode` differs, so the user's own sandbox settings (extra
    /// writable roots, network) stay in force otherwise.
    applied_mode: String,
    model: String,
    effort: String,
    /// The running turn.
    turn: Option<String>,
    /// The ACP request of the running turn's own prompt.
    prompt: Option<Value>,
    /// `turn/start` was sent and has no answer yet.
    starting: bool,
    /// A cancel arrived before the turn had an id.
    cancel_pending: bool,
    /// Steers that arrived before the turn had an id: (ACP request, input).
    queued_steers: Vec<(Value, Vec<Value>)>,
    /// Agent message and reasoning items that streamed deltas.
    streamed: HashSet<String>,
    /// Running tool items -> their `tool_call` fields at start (a file
    /// change's diffs are computed once, from the file before the change).
    tools: HashMap<String, Value>,
    /// Running command items -> their output tail and how much of it the
    /// client got in content so far.
    output: HashMap<String, (String, usize)>,
    /// Token totals at the running turn's start, and the latest.
    usage_at_start: Option<Value>,
    usage: Option<Value>,
    /// A subagent's last reported state.
    sub_state: Option<&'static str>,
}

struct State {
    opts: Options,
    next_server_id: i64,
    next_client_id: i64,
    initialized: bool,
    init_failed: Option<String>,
    /// Client `initialize` requests waiting for the server's.
    waiting_init: Vec<Value>,
    /// Client messages that arrived before the server was initialized.
    queued: Vec<Value>,
    pending: HashMap<String, Pending>,
    asked: HashMap<String, Asked>,
    models: Vec<ModelInfo>,
    threads: HashMap<String, Thread>,
    rate_limits: Option<Value>,
    user_agent: Option<String>,
}

/// The first Codex app-server whose `turn/start` takes `{"type": "audio"}` input.
pub const AUDIO_INPUT_VERSION: (u64, u64, u64) = (0, 145, 0);

/// The version in an app-server `userAgent` (`<originator>/<x.y.z>[-pre] (os; arch) ...`).
fn agent_version(user_agent: &str) -> Option<(u64, u64, u64)> {
    let (_, version) = user_agent.split_whitespace().next()?.split_once('/')?;
    let core = version.split(['-', '+']).next()?;
    let mut parts = core.split('.').map(|p| p.parse::<u64>().ok());
    let version = (parts.next()??, parts.next()??, parts.next()??);
    parts.next().is_none().then_some(version)
}

impl State {
    /// Whether this app-server takes audio input: the caller opted in and the
    /// server's version is known to accept it.
    pub(super) fn audio_input(&self) -> bool {
        self.opts.audio_input
            && self.user_agent.as_deref().and_then(agent_version).is_some_and(|v| v >= AUDIO_INPUT_VERSION)
    }
}

/// The Codex app-server <-> ACP translator. Every method takes `&self`;
/// one mutex guards the state, and no method blocks or does I/O beyond
/// reading a file a change is about to edit.
pub struct Translator {
    state: Mutex<State>,
}

fn key(id: &Value) -> String {
    id.to_string()
}

pub(crate) fn request(id: impl Into<Value>, method: &str, params: Value) -> Value {
    json!({"jsonrpc": "2.0", "id": id.into(), "method": method, "params": params})
}

pub(crate) fn notification(method: &str, params: Value) -> Value {
    json!({"jsonrpc": "2.0", "method": method, "params": params})
}

pub(crate) fn ok(id: &Value, result: Value) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "result": result})
}

pub(crate) fn err(id: &Value, code: i64, message: impl Into<String>) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message.into()}})
}

pub(crate) fn update(session: &str, update: Value) -> Out {
    Out::Client(notification("session/update", json!({"sessionId": session, "update": update})))
}

const INVALID_PARAMS: i64 = -32602;
const METHOD_NOT_FOUND: i64 = -32601;
const INTERNAL: i64 = -32603;

impl Translator {
    pub fn new(opts: Options) -> Self {
        Self {
            state: Mutex::new(State {
                opts,
                next_server_id: 1,
                next_client_id: 1,
                initialized: false,
                init_failed: None,
                waiting_init: Vec::new(),
                queued: Vec::new(),
                pending: HashMap::new(),
                asked: HashMap::new(),
                models: Vec::new(),
                threads: HashMap::new(),
                rate_limits: None,
                user_agent: None,
            }),
        }
    }

    /// The first lines for the app-server: its `initialize`. Sent at spawn,
    /// before the client asks, so the client's `initialize` costs no round
    /// trip to the server.
    pub fn start(&self) -> Vec<Out> {
        let mut st = self.lock();
        let info = json!({
            "clientInfo": {"name": st.opts.client_name, "title": st.opts.client_title, "version": st.opts.client_version},
            "capabilities": {"experimentalApi": true},
        });
        vec![st.send(Pending::Initialize, "initialize", info)]
    }

    /// One message from the ACP client.
    pub fn from_client(&self, msg: &Value) -> Vec<Out> {
        let mut st = self.lock();
        let mut out = Vec::new();
        st.client(msg, &mut out);
        out
    }

    /// One line from the app-server.
    pub fn from_server(&self, msg: &Value) -> Vec<Out> {
        let mut st = self.lock();
        let mut out = Vec::new();
        st.server(msg, &mut out);
        out
    }

    /// The app-server exited or its pipe closed: every request still waiting
    /// on it gets an error, so no prompt waits forever.
    pub fn server_closed(&self, why: &str) -> Vec<Out> {
        let mut st = self.lock();
        let message = format!("codex app-server stopped: {why}");
        st.init_failed = Some(message.clone());
        let mut out: Vec<Out> = Vec::new();
        for id in st.waiting_init.drain(..) {
            out.push(Out::Client(err(&id, INTERNAL, &message)));
        }
        let pending: Vec<Pending> = st.pending.drain().map(|(_, p)| p).collect();
        for p in pending {
            match p {
                Pending::ThreadStart { client, .. }
                | Pending::ThreadResume { client, .. }
                | Pending::ThreadFork { client, .. }
                | Pending::ThreadList { client }
                | Pending::TurnSteer { client }
                | Pending::Ack { client }
                | Pending::TurnStart { client, .. } => out.push(Out::Client(err(&client, INTERNAL, &message))),
                _ => {}
            }
        }
        for t in st.threads.values_mut() {
            if let Some(id) = t.prompt.take() {
                out.push(Out::Client(err(&id, INTERNAL, &message)));
            }
            for (id, _) in t.queued_steers.drain(..) {
                out.push(Out::Client(err(&id, INTERNAL, &message)));
            }
            t.turn = None;
            t.starting = false;
        }
        let queued: Vec<Value> = std::mem::take(&mut st.queued);
        for m in queued {
            if let Some(id) = m.get("id").filter(|_| m.get("method").is_some()) {
                out.push(Out::Client(err(id, INTERNAL, &message)));
            }
        }
        out
    }

    /// The Codex thread id of every session and subagent this translator
    /// knows (each is its ACP session id).
    pub fn sessions(&self) -> Vec<String> {
        self.lock().threads.keys().cloned().collect()
    }

    /// Whether a turn runs in `session`.
    pub fn in_turn(&self, session: &str) -> bool {
        self.lock().threads.get(session).is_some_and(|t| t.turn.is_some() || t.starting)
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }
}

impl State {
    /// A request to the app-server, remembered under a fresh id.
    fn send(&mut self, pending: Pending, method: &str, params: Value) -> Out {
        let id = self.next_server_id;
        self.next_server_id += 1;
        self.pending.insert(key(&json!(id)), pending);
        Out::Server(request(id, method, params))
    }

    fn client_id(&mut self) -> Value {
        let id = self.next_client_id;
        self.next_client_id += 1;
        json!(format!("codex-{id}"))
    }

    /// Whether a prompt to `thread` may carry audio: the app-server takes it
    /// and so does the thread's model (the default model for a thread that
    /// has none yet). Audio for any other model is dropped, since Codex would
    /// only tell the model it could not hear it.
    pub(super) fn thread_takes_audio(&self, thread: &str) -> bool {
        let model = self.model_of(thread);
        self.audio_input()
            && self
                .models
                .iter()
                .find(|m| if model.is_empty() { m.is_default } else { m.id == model })
                .is_some_and(config::ModelInfo::takes_audio)
    }

    fn model_of(&self, thread: &str) -> String {
        self.threads.get(thread).map(|t| t.model.clone()).unwrap_or_default()
    }

    fn config_options(&self, thread: &str) -> Value {
        let t = self.threads.get(thread);
        config::config_options(
            &self.models,
            t.map(|t| t.model.as_str()).unwrap_or(""),
            t.map(|t| t.effort.as_str()).unwrap_or(config::DEFAULT_EFFORT),
            t.map(|t| t.mode.as_str()).unwrap_or(config::DEFAULT_MODE),
        )
    }

    /// The `session/new`/`load`/`fork` result for a thread.
    fn session_result(&self, thread: &str, extra: Value) -> Value {
        let t = self.threads.get(thread);
        let mut r = json!({
            "sessionId": thread,
            "modes": config::modes_value(t.map(|t| t.mode.as_str()).unwrap_or(config::DEFAULT_MODE)),
            "models": config::models_value(&self.models, &self.model_of(thread)),
            "configOptions": self.config_options(thread),
        });
        if let (Some(obj), Value::Object(extra)) = (r.as_object_mut(), extra) {
            obj.extend(extra);
        }
        r
    }

    /// `thread` and every subagent under it, at any depth.
    fn family(&self, thread: &str) -> Vec<String> {
        let mut all = vec![thread.to_owned()];
        let mut i = 0;
        while i < all.len() {
            let parent = all[i].clone();
            all.extend(
                self.threads
                    .iter()
                    .filter(|(id, t)| t.parent.as_deref() == Some(parent.as_str()) && !all.contains(id))
                    .map(|(id, _)| id.clone())
                    .collect::<Vec<_>>(),
            );
            i += 1;
        }
        all
    }

    /// Root sessions (not subagents).
    fn roots(&self) -> Vec<String> {
        self.threads.iter().filter(|(_, t)| t.parent.is_none()).map(|(id, _)| id.clone()).collect()
    }

    /// A thread from a `thread/start`/`resume`/`fork` result.
    fn adopt_thread(&mut self, result: &Value, mode: String, effort: Option<String>) -> Option<String> {
        let id = result.pointer("/thread/id").and_then(Value::as_str)?.to_owned();
        let model = result
            .get("model")
            .and_then(Value::as_str)
            .or_else(|| result.pointer("/thread/model").and_then(Value::as_str))
            .unwrap_or("")
            .to_owned();
        let effort = effort
            .or_else(|| result.get("reasoningEffort").and_then(Value::as_str).map(str::to_owned))
            .unwrap_or_else(|| config::DEFAULT_EFFORT.into());
        let t = self.threads.entry(id.clone()).or_default();
        t.parent = None;
        t.applied_mode = mode.clone();
        t.mode = mode;
        t.model = model;
        t.effort = effort;
        Some(id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CURRENT: &str = "acpmux/0.163.0 (Mac OS 27.0.1; arm64) unknown (acpmux; 1)";

    fn advertised_audio(audio_input: bool, user_agent: &str) -> bool {
        let translator = Translator::new(Options { audio_input, ..Options::default() });
        let server_initialize = translator.start();
        let server_id = match &server_initialize[0] {
            Out::Server(message) => message["id"].clone(),
            other => panic!("expected app-server initialize, got {other:?}"),
        };
        translator.from_server(&json!({
            "jsonrpc": "2.0",
            "id": server_id,
            "result": {"userAgent": user_agent},
        }));
        let client_initialize = translator.from_client(&json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {"protocolVersion": 1},
        }));
        let result = client_initialize
            .into_iter()
            .find_map(|message| match message {
                Out::Client(message) => message.get("result").cloned(),
                Out::Server(_) => None,
            })
            .expect("ACP initialize result");
        result["agentCapabilities"]["promptCapabilities"]["audio"].as_bool().expect("audio capability bool")
    }

    #[test]
    fn initialize_does_not_advertise_audio_by_default() {
        assert!(!advertised_audio(false, CURRENT));
    }

    #[test]
    fn initialize_advertises_audio_when_explicitly_enabled() {
        assert!(advertised_audio(true, CURRENT));
        assert!(advertised_audio(true, "codex_cli_rs/0.145.0-alpha.2 (Linux; x86_64)"));
    }

    #[test]
    fn initialize_does_not_advertise_audio_to_an_older_or_unknown_server() {
        assert!(!advertised_audio(true, "acpmux/0.144.9 (Mac OS 27.0.1; arm64)"));
        assert!(!advertised_audio(true, "codex-test"));
        assert!(!advertised_audio(true, "acpmux/main (Mac OS 27.0.1; arm64)"));
    }

    /// A translator past `initialize` and `model/list` from an app-server at
    /// `CURRENT` whose only model, the default, takes `modalities`.
    fn with_default_model(modalities: &[&str]) -> Translator {
        let translator = Translator::new(Options { audio_input: true, ..Options::default() });
        let id = |outs: Vec<Out>| {
            outs.into_iter()
                .find_map(|o| match o {
                    Out::Server(m) if m.get("method").is_some() && m.get("id").is_some() => Some(m["id"].clone()),
                    _ => None,
                })
                .expect("an app-server request")
        };
        let init = id(translator.start());
        let list = id(translator.from_server(&json!({"jsonrpc": "2.0", "id": init, "result": {"userAgent": CURRENT}})));
        translator.from_server(&json!({"jsonrpc": "2.0", "id": list, "result": {"data": [{
            "id": "m", "displayName": "M", "isDefault": true, "inputModalities": modalities,
        }]}}));
        translator
    }

    #[test]
    fn audio_goes_only_to_a_model_that_hears_it() {
        let text_only = with_default_model(&["text", "image"]);
        assert!(!text_only.state.lock().unwrap().thread_takes_audio("new"));
        let hears = with_default_model(&["text", "audio"]);
        assert!(hears.state.lock().unwrap().thread_takes_audio("new"));
    }

    #[test]
    fn model_options_name_their_input_modalities() {
        let models = config::parse_models(&json!({"data": [
            {"id": "a", "inputModalities": ["text", "audio"]},
            {"id": "b"},
        ]}));
        let options = config::config_options(&models, "a", config::DEFAULT_EFFORT, config::DEFAULT_MODE);
        let model = &options[0]["options"];
        assert_eq!(model[0]["_meta"]["inputModalities"], json!(["text", "audio"]));
        assert!(model[1].get("_meta").is_none());
    }
}
