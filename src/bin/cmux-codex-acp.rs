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
    eprintln!("usage: cmux-codex-acp [--codex PATH] [-c key=value]... [--model MODEL] [--effort EFFORT] [--mode MODE]");
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
    let mut server_in = child.stdin.take().expect("piped stdin");
    let mut server_out = BufReader::new(child.stdout.take().expect("piped stdout")).lines();
    let mut client_in = BufReader::new(tokio::io::stdin()).lines();
    let mut client_out = tokio::io::stdout();
    let tr = Translator::new(args.opts);

    let mut outs = tr.start();
    loop {
        // Write what the last message produced; one flush per batch.
        let (mut to_client, mut to_server) = (Vec::new(), Vec::new());
        for o in outs.drain(..) {
            match o {
                Out::Client(v) => to_client.extend(line(&v)),
                Out::Server(v) => to_server.extend(line(&v)),
            }
        }
        if !to_server.is_empty() && (server_in.write_all(&to_server).await.is_err() || server_in.flush().await.is_err())
        {
            outs = tr.server_closed("its stdin closed");
            for o in outs.drain(..) {
                if let Out::Client(v) = o {
                    to_client.extend(line(&v));
                }
            }
            let _ = client_out.write_all(&to_client).await;
            let _ = client_out.flush().await;
            return ExitCode::from(1);
        }
        if !to_client.is_empty()
            && (client_out.write_all(&to_client).await.is_err() || client_out.flush().await.is_err())
        {
            break;
        }
        tokio::select! {
            next = client_in.next_line() => match next {
                Ok(Some(l)) if l.trim().is_empty() => {}
                Ok(Some(l)) => match serde_json::from_str::<Value>(&l) {
                    Ok(v) => outs = tr.from_client(&v),
                    Err(e) => eprintln!("cmux-codex-acp: bad client line: {e}"),
                },
                // The client went away: let codex finish writing and exit.
                _ => break,
            },
            next = server_out.next_line() => match next {
                Ok(Some(l)) if l.trim().is_empty() => {}
                Ok(Some(l)) => match serde_json::from_str::<Value>(&l) {
                    Ok(v) => outs = tr.from_server(&v),
                    Err(_) => eprintln!("codex app-server: {l}"),
                },
                _ => {
                    let status = child.wait().await.ok().and_then(|s| s.code());
                    outs = tr.server_closed(&format!("exit code {status:?}"));
                    let mut to_client = Vec::new();
                    for o in outs.drain(..) {
                        if let Out::Client(v) = o {
                            to_client.extend(line(&v));
                        }
                    }
                    let _ = client_out.write_all(&to_client).await;
                    let _ = client_out.flush().await;
                    return ExitCode::from(1);
                }
            },
        }
    }
    drop(server_in);
    // Bounded: codex exits when its stdin closes; kill_on_drop ends a hang.
    let _ = tokio::time::timeout(std::time::Duration::from_secs(3), child.wait()).await;
    ExitCode::SUCCESS
}
