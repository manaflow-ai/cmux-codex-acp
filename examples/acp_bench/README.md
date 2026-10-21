# ACP adapter bench

One recorded-shape Codex turn, replayed by a fixture `codex app-server`
(`fake_app_server.py`, no model), driven over stdio by an ACP client
(`drive.py`) that reports the adapter process's startup, RSS, CPU per delta,
bytes per message, and a behavior summary (stop reason, text hashes,
permission, tool call, cancel, `session/load`) to compare adapters.

```sh
cargo build --release --bin cmux-codex-acp
export CODEX_PATH=$PWD/examples/acp_bench/codex FIX_DELTAS=20000
python3 -I examples/acp_bench/drive.py --label rust -- target/release/cmux-codex-acp
python3 -I examples/acp_bench/drive.py --label node -- node path/to/@agentclientprotocol/codex-acp/dist/index.js
```

Linux only (it reads `/proc`).
