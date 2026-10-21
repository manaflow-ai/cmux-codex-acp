# Slash command oracle

`node-oracle.json` is what `@agentclientprotocol/codex-acp` 1.10.0 does on
the fixture app-server. Re-record it (Linux, Node 22+):

```sh
npm i @agentclientprotocol/codex-acp@1.10.0
export CODEX_PATH=$PWD/tests/commands/codex
python3 -I tests/commands/commands.py --label node --out tests/commands/node-oracle.json -- \
  node node_modules/@agentclientprotocol/codex-acp/dist/index.js
```

`compare.py` lists the intended differences it ignores (config option order,
skills/list before every prompt, a command approval's title, when the
generated title arrives).
