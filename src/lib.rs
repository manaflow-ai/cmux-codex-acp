//! Codex app-server presented as an ACP agent.
//!
//! `codex app-server` speaks its own JSON-RPC protocol over stdio: threads,
//! turns, items and server requests for approvals. An ACP client speaks the
//! Agent Client Protocol: sessions, prompts, `session/update` notifications
//! and `session/request_permission`. [`Translator`] stands between the two.
//! It does no I/O: the caller owns the app-server process, feeds every line
//! it reads to the translator and writes every [`Out`] it gets back.
//!
//! ```text
//!   ACP client  --from_client-->  Translator  --Out::Server-->  codex app-server
//!   ACP client  <--Out::Client--  Translator  <--from_server--  codex app-server
//! ```
//!
//! One app-server process carries every session of one translator: each ACP
//! session is one Codex thread, and the ACP session id is the thread id. A
//! subagent that Codex spawns is a child thread on the same connection; its
//! updates arrive under its own session id (the child thread id), announced
//! by `subagent_spawned` and ended by `subagent_state_update` under the
//! parent's session.
//!
//! The `cmux-codex-acp` binary runs the translator over stdio for any ACP
//! client. acpmux links the library and runs it in process.

mod config;
mod content;
mod items;
mod patch;
mod translator;

pub use translator::{Options, Out, Translator};

/// The app-server version this translator was written and tested against.
pub const CODEX_TESTED_VERSION: &str = "0.162.1";

/// The prefix of the error a steered prompt gets when no turn is running.
/// A client that steers can tell it apart and send the prompt as a new turn.
pub const STEER_NO_TURN: &str = "steer: no turn is running";

/// Whether an app-server line is a streaming delta: frequent, small, and
/// carried again in the translated update. A caller that logs raw lines may
/// skip these.
pub fn is_delta(method: &str) -> bool {
    method.ends_with("Delta") || method.ends_with("/delta")
}
