//! The translator: state of one app-server connection and every session on
//! it.

mod client;
mod commands;
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
    /// Generate a short title for a new session after its first turn (an
    /// ephemeral Codex thread on the title model, then `thread/name/set`), as
    /// the codex-acp adapter does. Off: a session keeps its fallback title
    /// (the first prompt) and its thread name. A session can also turn it off
    /// for itself with `_meta.cmux.generateTitle: false` on `session/new`,
    /// `load`, `resume` or `fork`.
    pub generate_titles: bool,
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
            generate_titles: true,
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
        /// False: this session asked for no generated title.
        titles: bool,
    },
    /// `session/load` (replay) or `session/resume` (no replay).
    ThreadResume {
        client: Value,
        session: String,
        mode: String,
        replay: bool,
        titles: bool,
    },
    ThreadFork {
        client: Value,
        mode: String,
        titles: bool,
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
    /// A slash command's request (`commands.rs`).
    Command {
        client: Value,
        thread: String,
        wait: commands::CommandWait,
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
    /// The thread's working directory.
    cwd: String,
    /// The collaboration mode the session picked (`default` or `plan`); None
    /// until it picks one, so turns leave Codex's own setting alone.
    collab: Option<String>,
    /// Where the session title came from, and the first prompt's text for it.
    title: commands::TitleSource,
    prompt_title: Option<String>,
    /// The running prompt is a `/compact`: it ends when the compaction does.
    compacting: bool,
    /// The running turn came from a command (no fallback title).
    command_turn: bool,
    /// The plan the running plan-mode turn completed: (item id, text).
    plan: Option<(String, String)>,
    /// A plan review asked of the client: (request id, tool call id).
    plan_review: Option<(Value, String)>,
    /// A `/goal pause` or `/goal clear` prompt, answered with the goal's update.
    goal_prompt: Option<Value>,
    /// A collaboration mode change waits for Codex.
    collab_pending: bool,
    /// A command turn was cancelled before it started: the next `turn/started`
    /// that no prompt asked for is it, and is interrupted.
    stale_command_turn: bool,
    /// That turn, once started: its `turn/completed` ends no prompt.
    ignore_turn: Option<String>,
    /// The title was generated (or asked for) once.
    title_generated: bool,
    /// The running prompt's text, for the title thread.
    prompt_text: Option<String>,
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
    /// The account `/status` read last.
    account: Option<Value>,
    /// Ephemeral title threads: thread id -> (session, agent text so far).
    title_threads: HashMap<String, (String, String)>,
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
                account: None,
                title_threads: HashMap::new(),
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
        // Each waiting client request is answered once, even when two records
        // hold it (a turn's `TurnStart` and its thread's prompt).
        let mut answered = HashSet::new();
        let mut fail = |id: &Value, out: &mut Vec<Out>| {
            if !id.is_null() && answered.insert(key(id)) {
                out.push(Out::Client(err(id, INTERNAL, &message)));
            }
        };
        let pending: Vec<Pending> = st.pending.drain().map(|(_, p)| p).collect();
        for p in pending {
            match p {
                Pending::ThreadStart { client, .. }
                | Pending::ThreadResume { client, .. }
                | Pending::ThreadFork { client, .. }
                | Pending::ThreadList { client }
                | Pending::TurnSteer { client }
                | Pending::Ack { client }
                | Pending::Command { client, .. }
                | Pending::TurnStart { client, .. } => fail(&client, &mut out),
                _ => {}
            }
        }
        for t in st.threads.values_mut() {
            if let Some((ask, _)) = t.plan_review.take() {
                out.push(Out::Client(notification("$/cancel_request", json!({"requestId": ask}))));
            }
            for id in t.prompt.take().into_iter().chain(t.goal_prompt.take()) {
                fail(&id, &mut out);
            }
            for (id, _) in t.queued_steers.drain(..) {
                fail(&id, &mut out);
            }
            t.turn = None;
            t.starting = false;
            t.compacting = false;
            t.command_turn = false;
        }
        st.title_threads.clear();
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
    /// has none yet). A model `model/list` names takes audio only when its
    /// `inputModalities` list `audio` (an empty or missing list takes none),
    /// since Codex would only tell any other model it could not hear it. A
    /// model the list does not name (hidden, or the list has not arrived) is
    /// left to Codex.
    pub(super) fn thread_takes_audio(&self, thread: &str) -> bool {
        let model = self.model_of(thread);
        self.audio_input()
            && self
                .models
                .iter()
                .find(|m| if model.is_empty() { m.is_default } else { m.id == model })
                .is_none_or(config::ModelInfo::takes_audio)
    }

    fn model_of(&self, thread: &str) -> String {
        self.threads.get(thread).map(|t| t.model.clone()).unwrap_or_default()
    }

    fn config_options(&self, thread: &str) -> Value {
        let t = self.threads.get(thread);
        let mut options = config::config_options(
            &self.models,
            t.map(|t| t.model.as_str()).unwrap_or(""),
            t.map(|t| t.effort.as_str()).unwrap_or(config::DEFAULT_EFFORT),
            t.map(|t| t.mode.as_str()).unwrap_or(config::DEFAULT_MODE),
        );
        if let Some(list) = options.as_array_mut() {
            list.push(self.collaboration_option(thread));
        }
        options
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
        let cwd = result
            .pointer("/thread/cwd")
            .or_else(|| result.get("cwd"))
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned();
        let t = self.threads.entry(id.clone()).or_default();
        t.parent = None;
        if !cwd.is_empty() {
            t.cwd = cwd;
        }
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
    fn a_thread_is_judged_by_its_own_model() {
        let translator = with_default_model(&["text", "audio"]);
        let mut state = translator.state.lock().unwrap();
        state.threads.insert("t".into(), Thread { model: "m".into(), ..Thread::default() });
        assert!(state.thread_takes_audio("t"));
        // A model the list does not name (hidden, or rerouted to) is left to Codex.
        state.threads.get_mut("t").unwrap().model = "hidden".into();
        assert!(state.thread_takes_audio("t"));
        drop(state);
        let translator = with_default_model(&["text"]);
        let mut state = translator.state.lock().unwrap();
        state.threads.insert("t".into(), Thread { model: "m".into(), ..Thread::default() });
        assert!(!state.thread_takes_audio("t"));
    }

    /// `with_default_model` plus a second model, `other`, taking `other_modalities`.
    fn with_two_models(modalities: &[&str], other_modalities: Option<&[&str]>) -> Translator {
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
        let mut other = json!({"id": "other", "displayName": "Other"});
        if let Some(m) = other_modalities {
            other["inputModalities"] = json!(m);
        }
        translator.from_server(&json!({"jsonrpc": "2.0", "id": list, "result": {"data": [
            {"id": "m", "displayName": "M", "isDefault": true, "inputModalities": modalities},
            other,
        ]}}));
        translator.state.lock().unwrap().threads.insert("t".into(), Thread::default());
        translator
    }

    /// What a `session/prompt` of `blocks` to thread `t` sends: the
    /// `turn/start` input, or the error, and any notices to the client.
    fn prompt(translator: &Translator, blocks: Value) -> (Result<Value, Value>, Vec<Value>) {
        let outs = translator.from_client(&json!({"jsonrpc": "2.0", "id": 7, "method": "session/prompt",
            "params": {"sessionId": "t", "prompt": blocks}}));
        let mut result = Err(Value::Null);
        let mut notices = Vec::new();
        for o in outs {
            match o {
                Out::Server(m) if m["method"] == "turn/start" => result = Ok(m["params"]["input"].clone()),
                Out::Client(m) if m.get("error").is_some() => result = Err(m["error"]["message"].clone()),
                Out::Client(m) if m["method"] == "session/update" => notices.push(m["params"]["update"].clone()),
                _ => {}
            }
        }
        // Ready for the next prompt.
        let mut state = translator.state.lock().unwrap();
        let t = state.threads.get_mut("t").expect("thread t");
        (t.starting, t.prompt) = (false, None);
        (result, notices)
    }

    const VOICE: &str = "UklGRg==";

    fn text_and_audio() -> Value {
        json!([{"type": "text", "text": "fix the build"}, {"type": "audio", "mimeType": "audio/wav", "data": VOICE}])
    }

    #[test]
    fn a_prompt_with_voice_carries_it_to_a_model_that_hears_it() {
        let translator = with_two_models(&["text", "audio"], None);
        let (input, notices) = prompt(&translator, text_and_audio());
        let input = input.expect("turn/start");
        assert_eq!(input[1], json!({"type": "audio", "url": format!("data:audio/wav;base64,{VOICE}")}));
        assert!(notices.is_empty());
    }

    #[test]
    fn a_prompt_with_voice_goes_as_text_with_a_notice_to_a_model_that_does_not() {
        let translator = with_two_models(&["text", "image"], None);
        let (input, notices) = prompt(&translator, text_and_audio());
        let input = input.expect("turn/start");
        assert_eq!(input.as_array().map(Vec::len), Some(1));
        assert_eq!(input[0]["type"], "text");
        assert_eq!(notices.len(), 1);
        assert_eq!(notices[0]["content"]["text"], crate::AUDIO_DROPPED);
    }

    #[test]
    fn a_rejected_prompt_gives_no_notice() {
        let translator = with_two_models(&["text"], None);
        translator.state.lock().unwrap().threads.get_mut("t").unwrap().starting = true;
        let outs = translator.from_client(&json!({"jsonrpc": "2.0", "id": 7, "method": "session/prompt",
            "params": {"sessionId": "t", "prompt": text_and_audio()}}));
        assert!(outs.iter().all(|o| !matches!(o, Out::Client(m) if m["method"] == "session/update")));
        let outs = translator.from_client(&json!({"jsonrpc": "2.0", "id": 8, "method": "session/prompt",
            "params": {"sessionId": "t", "prompt": text_and_audio(), "_meta": {"steer": true}}}));
        assert_eq!(outs.iter().filter(|o| matches!(o, Out::Client(m) if m["method"] == "session/update")).count(), 1);
        translator.state.lock().unwrap().threads.get_mut("t").unwrap().starting = false;
        let outs = translator.from_client(&json!({"jsonrpc": "2.0", "id": 9, "method": "session/prompt",
            "params": {"sessionId": "t", "prompt": text_and_audio(), "_meta": {"steer": true}}}));
        assert!(outs.iter().all(|o| !matches!(o, Out::Client(m) if m["method"] == "session/update")));
    }

    #[test]
    fn a_voice_only_prompt_to_a_model_that_does_not_hear_it_is_refused() {
        let translator = with_two_models(&["text"], None);
        let (input, _) = prompt(&translator, json!([{"type": "audio", "mimeType": "audio/wav", "data": VOICE}]));
        assert_eq!(input, Err(json!(crate::AUDIO_NOT_TAKEN)));
    }

    #[test]
    fn switching_models_switches_whether_voice_goes() {
        // `other` names no modalities: Codex's own default, no audio.
        let translator = with_two_models(&["text", "audio"], None);
        assert!(prompt(&translator, text_and_audio()).1.is_empty());
        translator.from_client(&json!({"jsonrpc": "2.0", "id": 8, "method": "session/set_config_option",
            "params": {"sessionId": "t", "configId": "model", "value": "other"}}));
        assert_eq!(prompt(&translator, text_and_audio()).1.len(), 1);
        let translator = with_two_models(&["text"], Some(&["text", "audio"]));
        translator.from_client(&json!({"jsonrpc": "2.0", "id": 8, "method": "session/set_config_option",
            "params": {"sessionId": "t", "configId": "model", "value": "other"}}));
        assert!(prompt(&translator, text_and_audio()).1.is_empty());
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

#[cfg(test)]
mod sandbox_tests;
