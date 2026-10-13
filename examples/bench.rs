//! Cold start, first-token latency and memory of any ACP agent command.
//!
//! ```text
//! cargo run --example bench -- --runs 3 --cwd DIR -- <agent argv...>
//! ```
//!
//! Each run: spawn, `initialize`, `session/new`, one prompt. It prints one
//! JSON line per run and a median line.

#[path = "../tests/support/acp.rs"]
mod acp;

use serde_json::json;
use std::time::{Duration, Instant};

fn median(mut v: Vec<f64>) -> f64 {
    v.sort_by(|a, b| a.total_cmp(b));
    v[v.len() / 2]
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let split = args.iter().position(|a| a == "--").expect("-- <agent argv>");
    let (opts, argv) = (&args[..split], &args[split + 1..]);
    let get = |k: &str| opts.iter().position(|a| a == k).and_then(|i| opts.get(i + 1)).cloned();
    let runs: usize = get("--runs").and_then(|r| r.parse().ok()).unwrap_or(3);
    let cwd = std::path::PathBuf::from(get("--cwd").unwrap_or_else(|| ".".into()));
    let prompt = get("--prompt").unwrap_or_else(|| "Reply with exactly: ok".into());
    let mut rows = Vec::new();
    for run in 0..runs {
        let mut a = acp::Agent::spawn(argv, &[], &cwd);
        let t0 = a.started;
        a.initialize();
        let init_ms = t0.elapsed().as_secs_f64() * 1e3;
        let s = a.new_session(&cwd);
        let ready_ms = t0.elapsed().as_secs_f64() * 1e3;
        let sid = s["sessionId"].as_str().unwrap().to_owned();
        let idle_rss = a.tree_rss_kib();
        let id = a.request("session/prompt", json!({"sessionId": sid, "prompt": [{"type": "text", "text": prompt}]}));
        let p0 = Instant::now();
        a.until(Duration::from_secs(300), |m| {
            m["method"] == "session/update" && m["params"]["update"]["sessionUpdate"] == "agent_message_chunk"
        });
        let first_token_ms = p0.elapsed().as_secs_f64() * 1e3;
        let r = a.response(id, Duration::from_secs(300));
        let turn_ms = p0.elapsed().as_secs_f64() * 1e3;
        let rss = a.tree_rss_kib();
        let row = json!({"run": run, "init_ms": init_ms, "session_ready_ms": ready_ms, "first_token_ms": first_token_ms, "turn_ms": turn_ms, "idle_rss_kib": idle_rss, "rss_kib": rss, "stop": r["result"]["stopReason"]});
        println!("{row}");
        rows.push(row);
        a.close();
    }
    let m = |k: &str| median(rows.iter().filter_map(|r| r[k].as_f64()).collect());
    println!(
        "{}",
        json!({"median": true, "runs": runs, "init_ms": m("init_ms"), "session_ready_ms": m("session_ready_ms"), "first_token_ms": m("first_token_ms"), "turn_ms": m("turn_ms"), "idle_rss_kib": m("idle_rss_kib"), "rss_kib": m("rss_kib")})
    );
}
