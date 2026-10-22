//! `cmux-codex-acp`: Codex as an ACP agent over stdio.
//!
//! Spawns `codex app-server` and translates between it and the ACP client
//! on stdin/stdout. One app-server process serves every session the client
//! opens.
//!
//! ```text
//! cmux-codex-acp [--codex PATH] [-c key=value]... [--model M] [--mode MODE]
//! ```
//!
//! The codex binary is `--codex`, else `$CODEX_PATH`, else `codex` on PATH.
//! `-c` overrides are passed to `codex app-server` as they are.

use cmux_codex_acp::{Options, Out, Translator};
use serde_json::Value;
use std::process::{ExitCode, Stdio};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

struct Args {
    codex: String,
    overrides: Vec<String>,
    opts: Options,
}

fn usage() -> ! {
    eprintln!(
        "usage: cmux-codex-acp [--codex PATH] [-c key=value]... [--model MODEL] [--effort EFFORT] [--mode MODE] [--no-titles]"
    );
    std::process::exit(2)
}

fn parse_args() -> Args {
    let mut args = Args {
        codex: std::env::var("CODEX_PATH").ok().filter(|p| !p.is_empty()).unwrap_or_else(|| "codex".into()),
        overrides: Vec::new(),
        opts: Options::default(),
    };
    let mut it = std::env::args().skip(1);
    while let Some(a) = it.next() {
        let mut value = || it.next().unwrap_or_else(|| usage());
        match a.as_str() {
            "--codex" => args.codex = value(),
            "-c" | "--config" => args.overrides.push(value()),
            "--model" => args.opts.model = Some(value()),
            "--effort" => args.opts.effort = Some(value()),
            "--mode" => args.opts.default_mode = value(),
            "--no-titles" => args.opts.generate_titles = false,
            "--version" => {
                println!("cmux-codex-acp {}", env!("CARGO_PKG_VERSION"));
                std::process::exit(0)
            }
            "-h" | "--help" => usage(),
            _ => usage(),
        }
    }
    args
}

fn line(v: &Value) -> Vec<u8> {
    let mut s = v.to_string().into_bytes();
    s.push(b'\n');
    s
}

/// A writer task: lines from `rx` to `w`, flushed when the queue drains, so
/// neither side's back-pressure stalls the read loop. The lines queued at
/// that moment go out in one write: tokio's stdout runs each write on its
/// blocking pool, so a write per streamed delta cost most of the binary's CPU.
fn writer<W: tokio::io::AsyncWrite + Unpin + Send + 'static>(
    mut w: W,
) -> (tokio::sync::mpsc::UnboundedSender<Vec<u8>>, tokio::task::JoinHandle<()>) {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
    let task = tokio::spawn(async move {
        while let Some(mut batch) = rx.recv().await {
            while batch.len() < MAX_BATCH
                && let Ok(more) = rx.try_recv()
            {
                batch.extend_from_slice(&more);
            }
            if w.write_all(&batch).await.is_err() || w.flush().await.is_err() {
                return;
            }
        }
    });
    (tx, task)
}

/// The most bytes one coalesced write carries before it goes out.
const MAX_BATCH: usize = 1 << 20;

#[tokio::main(flavor = "current_thread")]
async fn main() -> ExitCode {
    let args = parse_args();
    let mut cmd = tokio::process::Command::new(&args.codex);
    cmd.arg("app-server");
    for o in &args.overrides {
        cmd.arg("-c").arg(o);
    }
    cmd.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::inherit()).kill_on_drop(true);
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("cmux-codex-acp: cannot start {} app-server: {e}", args.codex);
            return ExitCode::from(1);
        }
    };
    let (to_server, server_writer) = writer(child.stdin.take().expect("piped stdin"));
    let (to_client, client_writer) = writer(tokio::io::stdout());
    let mut server_out = BufReader::new(child.stdout.take().expect("piped stdout")).lines();
    let mut client_in = BufReader::new(tokio::io::stdin()).lines();
    let tr = Translator::new(args.opts);
    let send = |outs: Vec<Out>| {
        for o in outs {
            let _ = match o {
                Out::Client(v) => to_client.send(line(&v)),
                Out::Server(v) => to_server.send(line(&v)),
            };
        }
    };
    send(tr.start());
    let code = loop {
        tokio::select! {
            next = client_in.next_line() => match next {
                Ok(Some(l)) if l.trim().is_empty() => {}
                Ok(Some(l)) => match serde_json::from_str::<Value>(&l) {
                    Ok(v) => send(tr.from_client(&v)),
                    Err(e) => {
                        let reply = serde_json::json!({"jsonrpc": "2.0", "id": null, "error": {"code": -32700, "message": format!("parse error: {e}")}});
                        let _ = to_client.send(line(&reply));
                    }
                },
                // The client went away: stop codex and exit.
                _ => break ExitCode::SUCCESS,
            },
            next = server_out.next_line() => match next {
                Ok(Some(l)) if l.trim().is_empty() => {}
                Ok(Some(l)) => match serde_json::from_str::<Value>(&l) {
                    Ok(v) => send(tr.from_server(&v)),
                    Err(_) => eprintln!("codex app-server: {l}"),
                },
                _ => {
                    let status = child.wait().await.ok().and_then(|s| s.code());
                    send(tr.server_closed(&format!("exit code {status:?}")));
                    break ExitCode::from(1);
                }
            },
        }
    };
    drop(to_server);
    drop(to_client);
    // Codex exits when its stdin closes; the bound and kill_on_drop end a hang.
    let _ = tokio::time::timeout(std::time::Duration::from_secs(3), server_writer).await;
    let _ = tokio::time::timeout(std::time::Duration::from_secs(3), child.wait()).await;
    let _ = tokio::time::timeout(std::time::Duration::from_secs(3), client_writer).await;
    code
}
