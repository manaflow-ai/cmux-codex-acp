//! In-process cost of the translator on a recorded-shape turn, as acpmux
//! links it: each app-server line is parsed, translated and each ACP message
//! serialized, with no process or pipe in between.
//!
//! ```text
//! cargo run --release --example replay -- [--deltas N] [--rounds R]
//! ```
//!
//! Prints one JSON line: ns, allocations and bytes per streamed delta for
//! agent message, reasoning and command output deltas.

use cmux_codex_acp::{Options, Out, Translator};
use serde_json::{Value, json};
use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::time::Instant;

struct Counting;
static ALLOCS: AtomicU64 = AtomicU64::new(0);
static BYTES: AtomicU64 = AtomicU64::new(0);
static LIVE: AtomicU64 = AtomicU64::new(0);

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        ALLOCS.fetch_add(1, Relaxed);
        BYTES.fetch_add(l.size() as u64, Relaxed);
        LIVE.fetch_add(l.size() as u64, Relaxed);
        unsafe { System.alloc(l) }
    }
    unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
        LIVE.fetch_sub(l.size() as u64, Relaxed);
        unsafe { System.dealloc(p, l) }
    }
    unsafe fn realloc(&self, p: *mut u8, l: Layout, n: usize) -> *mut u8 {
        ALLOCS.fetch_add(1, Relaxed);
        BYTES.fetch_add(n as u64, Relaxed);
        LIVE.fetch_add(n as u64, Relaxed);
        LIVE.fetch_sub(l.size() as u64, Relaxed);
        unsafe { System.realloc(p, l, n) }
    }
}

#[global_allocator]
static A: Counting = Counting;

const THREAD: &str = "019a0000-0000-7000-8000-000000000001";

/// Feeds `outs` back: answers every server request the way the fake
/// app-server does, returns what went to the client.
fn pump(tr: &Translator, outs: Vec<Out>, client: &mut Vec<Value>) {
    let mut queue = outs;
    while let Some(o) = queue.pop() {
        match o {
            Out::Client(v) => client.push(v),
            Out::Server(v) => {
                let (Some(id), Some(method)) = (v.get("id"), v.get("method").and_then(Value::as_str)) else {
                    continue;
                };
                let result = match method {
                    "initialize" => json!({"userAgent": "codex_cli_rs/0.162.1 (Linux; x86_64) bench"}),
                    "model/list" => json!({"data": [{"id": "gpt-6-astra", "model": "gpt-6-astra", "displayName": "gpt-6-astra",
                        "supportedReasoningEfforts": [{"reasoningEffort": "medium"}], "defaultReasoningEffort": "medium", "isDefault": true}], "nextCursor": null}),
                    "thread/start" => json!({"thread": {"id": THREAD, "cwd": "/w", "turns": []}, "model": "gpt-6-astra", "reasoningEffort": "medium"}),
                    "turn/start" => json!({"turn": {"id": "turn-1", "status": "inProgress", "items": []}}),
                    _ => json!({}),
                };
                queue.extend(tr.from_server(&json!({"id": id, "result": result})));
            }
        }
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let get = |k: &str| args.iter().position(|a| a == k).and_then(|i| args.get(i + 1)?.parse().ok());
    let deltas: usize = get("--deltas").unwrap_or(20_000);
    let rounds: usize = get("--rounds").unwrap_or(5);
    let words: Vec<&str> = "## Plan\n\nThe **daemon** keeps one `Translator` per app-server; each delta becomes one \
        `session/update`. See [the spec](https://agentclientprotocol.com) and run `cargo test -p acpmux`.\n\n- first \
        item\n- second item with `code`\n\n```rust\nfn main() { println!(\"hi\"); }\n```\n"
        .split(' ')
        .collect();

    let live0 = LIVE.load(Relaxed);
    let tr = Translator::new(Options::default());
    let mut sink = Vec::new();
    pump(&tr, tr.start(), &mut sink);
    let init = json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {"protocolVersion": 1}});
    pump(&tr, tr.from_client(&init), &mut sink);
    let new = json!({"jsonrpc": "2.0", "id": 2, "method": "session/new", "params": {"cwd": "/w", "mcpServers": []}});
    pump(&tr, tr.from_client(&new), &mut sink);
    let prompt = json!({"jsonrpc": "2.0", "id": 3, "method": "session/prompt",
        "params": {"sessionId": THREAD, "prompt": [{"type": "text", "text": "go"}]}});
    pump(&tr, tr.from_client(&prompt), &mut sink);
    let started = json!({"method": "turn/started", "params": {"threadId": THREAD, "turn": {"id": "turn-1", "status": "inProgress", "items": []}}});
    pump(&tr, tr.from_server(&started), &mut sink);
    let item = |kind: &str, id: &str| json!({"method": "item/started", "params": {"threadId": THREAD, "turnId": "turn-1", "item": {"type": kind, "id": id, "text": ""}}});
    pump(&tr, tr.from_server(&item("agentMessage", "m1")), &mut sink);
    pump(&tr, tr.from_server(&item("reasoning", "r1")), &mut sink);
    let exec = json!({"method": "item/started", "params": {"threadId": THREAD, "turnId": "turn-1", "item": {"type": "commandExecution",
        "id": "c1", "command": "cargo test", "cwd": "/w", "status": "inProgress", "commandActions": []}}});
    pump(&tr, tr.from_server(&exec), &mut sink);
    drop(sink);
    // The heap one translator with one ready session holds (what a session costs in process).
    let session_live = LIVE.load(Relaxed) as i64 - live0 as i64;

    // The recorded-shape lines, serialized once as the app-server writes them.
    let line = |method: &str, item: &str, delta: String| {
        json!({"method": method, "params": {"threadId": THREAD, "turnId": "turn-1", "itemId": item, "delta": delta}})
            .to_string()
    };
    let kinds: [(&str, &str, &str); 3] = [
        ("agent", "item/agentMessage/delta", "m1"),
        ("reasoning", "item/reasoning/summaryTextDelta", "r1"),
        ("output", "item/commandExecution/outputDelta", "c1"),
    ];
    let mut report = serde_json::Map::new();
    report.insert("deltas".into(), json!(deltas));
    report.insert("session_live_bytes".into(), json!(session_live));
    for (name, method, item) in kinds {
        let lines: Vec<String> = (0..deltas)
            .map(|i| {
                let d = if name == "output" { format!("test case_{i:04} ... ok\n") } else { format!("{} ", words[i % words.len()]) };
                line(method, item, d)
            })
            .collect();
        let in_bytes: usize = lines.iter().map(|l| l.len() + 1).sum();
        let mut best = f64::MAX;
        let (mut allocs, mut alloc_bytes, mut out_bytes, mut out_msgs) = (0, 0, 0usize, 0usize);
        for _ in 0..rounds {
            let (a0, b0) = (ALLOCS.load(Relaxed), BYTES.load(Relaxed));
            let mut wire = 0usize;
            let mut msgs = 0usize;
            let t = Instant::now();
            for l in &lines {
                let raw: Value = serde_json::from_str(l).expect("line");
                for o in tr.from_server(&raw) {
                    if let Out::Client(v) = o {
                        wire += serde_json::to_string(&v).expect("json").len() + 1;
                        msgs += 1;
                    }
                }
            }
            let ns = t.elapsed().as_nanos() as f64 / deltas as f64;
            if ns < best {
                best = ns;
            }
            allocs = ALLOCS.load(Relaxed) - a0;
            alloc_bytes = BYTES.load(Relaxed) - b0;
            out_bytes = wire;
            out_msgs = msgs;
        }
        report.insert(
            name.into(),
            json!({
                "ns_per_delta": (best * 10.0).round() / 10.0,
                "allocs_per_delta": allocs as f64 / deltas as f64,
                "alloc_bytes_per_delta": alloc_bytes as f64 / deltas as f64,
                "in_bytes_per_delta": in_bytes as f64 / deltas as f64,
                "out_bytes_per_delta": out_bytes as f64 / deltas as f64,
                "out_msgs_per_delta": out_msgs as f64 / deltas as f64,
            }),
        );
    }
    report.insert("live_bytes_after_turn".into(), json!(LIVE.load(Relaxed) as i64 - live0 as i64));
    println!("{}", Value::Object(report));
}
