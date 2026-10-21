//! Codex's slash commands and the session title at the ACP boundary: the
//! `cmux-codex-acp` binary over stdio against a fixture `codex app-server`
//! (`tests/commands/fake_app_server.py`), compared with what the
//! `@agentclientprotocol/codex-acp` 1.10.0 adapter does on the same fixture
//! (`tests/commands/node-oracle.json`, recorded by `commands.py` with the node
//! adapter; re-record it with the command in tests/commands/README.md).
//!
//! Covered: the `available_commands_update` list ($skills included), the
//! fallback and generated titles, the title on `session/load`, `/status`,
//! `/skills`, `/mcp`, `/rename`, `/compact`, `/review`, `/review-branch`,
//! `/review-commit`, `/goal` (set, pause, clear), `/plan` with the plan review
//! (approve and revise), `$skill` prompts and `/logout`; and, beyond the
//! oracle, a cancelled `/goal` that starts no turn, the app-server exiting
//! during a command, and Codex refusing a collaboration mode. Needs python3.

use std::path::Path;
use std::process::Command;

#[test]
fn slash_commands_match_the_node_adapter() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/commands");
    let project = std::env::temp_dir().join(format!("cxacp-commands-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&project);
    std::fs::create_dir_all(&project).unwrap();
    let out = project.join("rust.json");
    let codex = dir.join("codex");
    let run = Command::new("python3")
        .arg("-I")
        .arg(dir.join("commands.py"))
        .args(["--label", "rust", "--robustness", "--out"])
        .arg(&out)
        .arg("--")
        .arg(env!("CARGO_BIN_EXE_cmux-codex-acp"))
        .arg("--codex")
        .arg(&codex)
        .env("CODEX_PATH", &codex)
        .current_dir(&project)
        .output()
        .expect("python3");
    assert!(
        run.status.success(),
        "commands.py: {}{}",
        String::from_utf8_lossy(&run.stdout),
        String::from_utf8_lossy(&run.stderr)
    );
    let cmp = Command::new("python3")
        .arg("-I")
        .arg(dir.join("compare.py"))
        .arg(dir.join("node-oracle.json"))
        .arg(&out)
        .output()
        .expect("python3");
    let report = String::from_utf8_lossy(&cmp.stdout);
    // Paths the node adapter does not share (its 1 s timer): asserted here.
    let got: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&out).unwrap()).unwrap();
    let robust = &got["robust"];
    let want = serde_json::json!({
        "goal without a turn, then cancel": "cancelled",
        "next prompt after it": "end_turn",
        "app-server exits during /rename": "error",
        "rejected plan mode: set_config_option": "error",
        "rejected plan mode: /plan": "error",
        "rejected plan mode: next turn's mode": null,
        "rejected plan mode: turn runs in": ["collaborationMode=None"],
        "goal already complete": "end_turn",
        "close while an approved plan waits: answers": ["cancelled"],
        "hidden context: titles": ["hello titled", "Greeting from the oracle"],
        "hidden context: reaches the title model": false,
    });
    assert_eq!(robust, &want, "cancel, exit and refusal paths");
    let _ = std::fs::remove_dir_all(&project);
    assert!(report.trim_end().ends_with("0 differing"), "differs from the node adapter:\n{report}");
}
