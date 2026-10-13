//! A small ACP client over a child process's stdio, for behavior tests and
//! the benchmark: it speaks to any ACP agent command line.

#![allow(dead_code)]

use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{Receiver, RecvTimeoutError, channel};
use std::time::{Duration, Instant};

/// Answers a permission request: the option id to pick, or None for `cancelled`.
pub type Policy = Box<dyn FnMut(&Value) -> Option<String> + Send>;

pub struct Agent {
    pub child: Child,
    stdin: ChildStdin,
    rx: Receiver<(Instant, Value)>,
    next_id: i64,
    /// Every message the agent sent, with its arrival time.
    pub log: Vec<(Instant, Value)>,
    pub started: Instant,
    /// How permission requests are answered: the option id to pick, or None
    /// to answer `cancelled`.
    pub policy: Policy,
}

/// Picks the first option of `kind`.
pub fn pick(kind: &'static str) -> Policy {
    Box::new(move |params: &Value| {
        params
            .get("options")?
            .as_array()?
            .iter()
            .find(|o| o["kind"] == kind)?
            .get("optionId")?
            .as_str()
            .map(str::to_owned)
    })
}

impl Agent {
    pub fn spawn(argv: &[String], envs: &[(String, String)], cwd: &std::path::Path) -> Agent {
        let started = Instant::now();
        let mut cmd = Command::new(&argv[0]);
        cmd.args(&argv[1..]).current_dir(cwd).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::inherit());
        for (k, v) in envs {
            cmd.env(k, v);
        }
        let mut child = cmd.spawn().unwrap_or_else(|e| panic!("spawn {argv:?}: {e}"));
        let stdin = child.stdin.take().expect("stdin");
        let stdout = child.stdout.take().expect("stdout");
        let (tx, rx) = channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else { break };
                if let Ok(v) = serde_json::from_str::<Value>(&line)
                    && tx.send((Instant::now(), v)).is_err()
                {
                    break;
                }
            }
        });
        Agent { child, stdin, rx, next_id: 1, log: Vec::new(), started, policy: pick("allow_once") }
    }

    pub fn send(&mut self, v: Value) {
        let mut s = v.to_string();
        s.push('\n');
        self.stdin.write_all(s.as_bytes()).expect("write to agent");
        self.stdin.flush().expect("flush agent stdin");
    }

    pub fn notify(&mut self, method: &str, params: Value) {
        self.send(json!({"jsonrpc": "2.0", "method": method, "params": params}));
    }

    /// Sends a request; returns its id.
    pub fn request(&mut self, method: &str, params: Value) -> i64 {
        let id = self.next_id;
        self.next_id += 1;
        self.send(json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}));
        id
    }

    /// Reads messages until `stop` says so, answering permission requests by
    /// the policy. Returns the message that matched.
    pub fn until(&mut self, timeout: Duration, mut stop: impl FnMut(&Value) -> bool) -> Value {
        let deadline = Instant::now() + timeout;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            let (at, msg) = match self.rx.recv_timeout(left) {
                Ok(m) => m,
                Err(RecvTimeoutError::Timeout) => {
                    panic!("timed out after {timeout:?}; last messages: {:#?}", self.tail(8))
                }
                Err(RecvTimeoutError::Disconnected) => {
                    panic!("agent closed its stdout; last messages: {:#?}", self.tail(8))
                }
            };
            self.log.push((at, msg.clone()));
            if msg["method"] == "session/request_permission" && msg.get("id").is_some() {
                let outcome = match (self.policy)(&msg["params"]) {
                    Some(option) => json!({"outcome": "selected", "optionId": option}),
                    None => json!({"outcome": "cancelled"}),
                };
                let id = msg["id"].clone();
                self.send(json!({"jsonrpc": "2.0", "id": id, "result": {"outcome": outcome}}));
            }
            if stop(&msg) {
                return msg;
            }
        }
    }

    /// Waits for the answer to request `id`.
    pub fn response(&mut self, id: i64, timeout: Duration) -> Value {
        self.until(timeout, |m| m.get("id") == Some(&json!(id)) && m.get("method").is_none())
    }

    pub fn result(&mut self, id: i64, timeout: Duration) -> Value {
        let r = self.response(id, timeout);
        assert!(r.get("error").is_none(), "request {id} failed: {r}");
        r["result"].clone()
    }

    fn tail(&self, n: usize) -> Vec<Value> {
        self.log.iter().rev().take(n).rev().map(|(_, v)| v.clone()).collect()
    }

    /// The session updates logged since index `from`, for `session`.
    pub fn updates(&self, from: usize, session: &str) -> Vec<Value> {
        self.log[from..]
            .iter()
            .filter(|(_, m)| m["method"] == "session/update" && m["params"]["sessionId"] == session)
            .map(|(_, m)| m["params"]["update"].clone())
            .collect()
    }

    /// Concatenated agent message text in updates since `from`.
    pub fn text(&self, from: usize, session: &str) -> String {
        self.updates(from, session)
            .iter()
            .filter(|u| u["sessionUpdate"] == "agent_message_chunk")
            .filter_map(|u| u["content"]["text"].as_str().map(str::to_owned))
            .collect()
    }

    pub fn initialize(&mut self) -> Value {
        let id = self.request(
            "initialize",
            json!({"protocolVersion": 1, "clientCapabilities": {"fs": {"readTextFile": false, "writeTextFile": false}, "terminal": false}, "clientInfo": {"name": "cmux-codex-acp-test", "version": "0"}}),
        );
        self.result(id, Duration::from_secs(60))
    }

    pub fn new_session(&mut self, cwd: &std::path::Path) -> Value {
        let id = self.request("session/new", json!({"cwd": cwd, "mcpServers": []}));
        self.result(id, Duration::from_secs(120))
    }

    /// Runs one prompt to its end; returns (result, index of the first log
    /// entry of the turn).
    pub fn prompt(&mut self, session: &str, text: &str, timeout: Duration) -> (Value, usize) {
        let from = self.log.len();
        let id =
            self.request("session/prompt", json!({"sessionId": session, "prompt": [{"type": "text", "text": text}]}));
        let r = self.response(id, timeout);
        (r, from)
    }

    /// Resident memory of this agent and every descendant, in KiB (`ps`).
    pub fn tree_rss_kib(&self) -> u64 {
        let out = Command::new("ps").args(["-axo", "pid=,ppid=,rss="]).output().expect("ps");
        let rows: Vec<(u32, u32, u64)> = String::from_utf8_lossy(&out.stdout)
            .lines()
            .filter_map(|l| {
                let mut it = l.split_whitespace();
                Some((it.next()?.parse().ok()?, it.next()?.parse().ok()?, it.next()?.parse().ok()?))
            })
            .collect();
        let mut tree = vec![self.child.id()];
        let mut i = 0;
        while i < tree.len() {
            let p = tree[i];
            tree.extend(rows.iter().filter(|r| r.1 == p).map(|r| r.0));
            i += 1;
        }
        rows.iter().filter(|r| tree.contains(&r.0)).map(|r| r.2).sum()
    }

    pub fn close(mut self) {
        drop(self.stdin);
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if let Ok(Some(_)) = self.child.try_wait() {
                return;
            }
            std::thread::park_timeout(Duration::from_millis(50));
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
