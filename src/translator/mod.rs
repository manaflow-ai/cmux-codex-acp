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
