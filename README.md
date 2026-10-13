# cmux-codex-acp

Codex as an [Agent Client Protocol](https://agentclientprotocol.com) (ACP)
agent, built directly on `codex app-server`.

- **Library** (`cmux_codex_acp::Translator`): translates between ACP and the
  Codex app-server JSON-RPC protocol. It does no I/O; the caller owns the
  `codex app-server` process and passes lines in and out. acpmux (cmux's
  agent session daemon) links it in process, so there is no extra process
  between the client and Codex.
- **Binary** (`cmux-codex-acp`): the translator over stdio for any ACP client.

```sh
cargo install --git https://github.com/manaflow-ai/cmux-codex-acp
cmux-codex-acp --codex "$(command -v codex)"
```

Options: `--codex PATH` (else `$CODEX_PATH`, else `codex` on `PATH`),
`-c key=value` (passed to `codex app-server`), `--model`, `--effort`,
`--mode`.

Tested with Codex `0.162.1`.

## Design

One `codex app-server` process carries every session of a translator. Each
ACP session is one Codex thread; the ACP session id is the thread id. Text,
reasoning and command output stream to the client as each delta arrives.

Subagents are native: when Codex starts a subagent (a child thread), the
client gets `subagent_spawned` under the parent session, every update of the
child under the child's own session id, and `subagent_state_update` when it
ends (ACP draft `subagents` capability).

Approvals go through the client: Codex's command, file change, permission
and question requests become `session/request_permission`, with options
built from the decisions Codex offers for that request.

## Sessions and modes

| Mode | Approval policy | Sandbox |
| --- | --- | --- |
| `ask` | untrusted: asks before every command that is not a known-safe read, and before every edit | workspace-write |
| `read-only` | on-request | read-only |
| `agent` (default) | on-request | workspace-write |
| `agent-full-access` | never | danger-full-access |

Config options: `model` (from `model/list`), `reasoning_effort` (the model's
supported efforts), `mode`. A change applies from the next turn: Codex takes
model, effort, approval policy and sandbox per turn.

## ACP coverage

| ACP | Codex app-server |
| --- | --- |
| `initialize` | `initialize` + `initialized` (sent at spawn, before the client asks) and `model/list` |
| `session/new` | `thread/start` (cwd, approval policy, sandbox, MCP servers as `mcp_servers` config) |
| `session/load` | `thread/resume`, then the stored turns replayed as updates |
| `session/resume` | `thread/resume` with `excludeTurns` |
| `session/fork` | `thread/fork` (optional `_meta.lastTurnId`) |
| `session/list` | `thread/list` |
| `session/close` | `thread/unsubscribe` |
| `session/prompt` | `turn/start`; answered at `turn/completed` with the turn's token usage |
| `session/prompt` with `_meta.steer` | `turn/steer` into the running turn; answered `steered` |
| `session/cancel` | `turn/interrupt` for the session and its running subagents |
| `session/set_mode`, `set_model`, `set_config_option` | per-turn overrides on the next `turn/start` |

Prompt content: text, images (data or file URI), resource links (as
`@path` mentions), embedded resources (quoted with their path).

## App-server coverage

Requests the translator sends: `initialize`, `model/list`, `thread/start`,
`thread/resume`, `thread/fork`, `thread/list`, `thread/unsubscribe`,
`turn/start`, `turn/steer`, `turn/interrupt`.

Notifications:

| Notification | ACP |
| --- | --- |
| `item/agentMessage/delta` | `agent_message_chunk` (with `messageId`) |
| `item/reasoning/summaryTextDelta`, `textDelta`, `summaryPartAdded` | `agent_thought_chunk` |
| `item/started`, `item/completed` | `tool_call`, `tool_call_update`; messages and reasoning that did not stream |
| `item/commandExecution/outputDelta` | `tool_call_update` (output tail, plus `_meta.terminal_output_delta`) |
| `item/fileChange/patchUpdated` | `tool_call_update` with diffs |
| `turn/plan/updated` | `plan` |
| `thread/tokenUsage/updated` | `usage_update` (context used and window) |
| `account/rateLimits/updated` | `session_info_update` `_meta.codexAppServer.rateLimits` (merged, on change) |
| `thread/name/updated` | `session_info_update` title |
| `model/rerouted` | `config_option_update` |
| `turn/started`, `turn/completed` | turn state; the prompt's answer |
| `serverRequest/resolved` | drops a permission request Codex no longer waits on |

Items: `commandExecution` (execute, or read/search from Codex's parsed
command actions), `fileChange` (edit, with whole-file before and after text
from the file and the unified diff), `mcpToolCall`, `dynamicToolCall`,
`webSearch`, `imageView`, `imageGeneration`, `collabAgentToolCall`,
`contextCompaction`, `sleep` as tool calls; `subAgentActivity` and
`collabAgentToolCall` as subagent events; `agentMessage`, `plan`,
`reasoning` as message and thought chunks.

Server requests:

| Request | Answer |
| --- | --- |
| `item/commandExecution/requestApproval` | `session/request_permission` with Codex's `availableDecisions` (allow, allow for session, allow the proposed command prefix, network host rules, deny, deny and stop) |
| `item/fileChange/requestApproval` | `session/request_permission` with the diff |
| `item/permissions/requestApproval` | `session/request_permission`: grant for the turn, for the session, or deny |
| `item/tool/requestUserInput` | one question with choices: `session/request_permission` with one option per choice; otherwise answered empty |
| `mcpServer/elicitation/request` | an MCP tool approval (`codex_approval_kind: mcp_tool_call`): `session/request_permission` (allow, allow for the session, always allow, deny); other forms are declined (ACP has no elicitation form yet) |
| others (`item/tool/call`, `account/chatgptAuthTokens/refresh`, `attestation/generate`, legacy v1 approvals) | method-not-found error |

Not mapped, by design: thread management outside a session (archive,
delete, sections, goals, attachments, metadata), skills, plugins,
marketplace, apps, file system and `command/exec` RPCs, account login and
usage, feedback, config writes, realtime audio, fuzzy file search, Windows
sandbox setup, review mode, compaction requests. They have no ACP
counterpart or belong to the client's own surfaces.

## Tests

Behavior tests drive the binary over stdio against a real `codex
app-server`:

```sh
CMUX_CODEX_ACP_CODEX=$(command -v codex) cargo test --test live -- --test-threads 4
```

They cover a multi-turn session, a command with approval and streamed
output, a patch with approval and before/after diff, a denied patch, cancel
in the middle of a turn, two subagents with their own session ids, resume in
a new process, and a model switch. Without `CMUX_CODEX_ACP_CODEX` they
report that and pass.

`cargo run --release --example bench -- --runs 5 --cwd DIR -- <agent argv>`
measures cold start, first-token latency and memory of any ACP agent.

## License

Copyright (c) 2026-present Manaflow, Inc. Other contributors and third
parties retain copyright in their material.

GPL-3.0-or-later. See [LICENSE](LICENSE).
