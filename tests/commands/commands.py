#!/usr/bin/env python3
"""Slash commands at the ACP boundary, for one adapter.

commands.py --label NAME --out FILE -- <adapter argv...>
Runs each Codex slash command on the fixture app-server (fake_app_server.py,
CODEX_PATH) and writes one JSON object: per step the stop reason, the agent
text, titles, config changes, permission requests and the app-server
command calls the fixture logged. The node adapter's file is the oracle for
cmux-codex-acp's (compare.py).
"""
import json, os, subprocess, sys, time, signal

a = sys.argv[1:]; split = a.index("--"); opts, argv = a[:split], a[split + 1:]
def opt(k, d=None): return opts[opts.index(k) + 1] if k in opts else d
label, out_path = opt("--label", "adapter"), opt("--out")
log_path = "/tmp/fixture-%s.log" % label
if os.path.exists(log_path): os.remove(log_path)
env = dict(os.environ, FIX_LOG=log_path)
signal.signal(signal.SIGALRM, lambda *_: (sys.stderr.write("%s: timed out\n" % label), os._exit(3)))
signal.alarm(int(opt("--timeout", "120")))

class Agent:
    def __init__(self):
        self.p = subprocess.Popen(argv, stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                                  stderr=open("/tmp/adapter-cmd-%s.err" % label, "ab"), env=env)
        self.next = 1; self.log = []; self.answers = {}
    def send(self, m): self.p.stdin.write((json.dumps(m) + "\n").encode()); self.p.stdin.flush()
    def req(self, method, params):
        i = self.next; self.next += 1
        self.send({"jsonrpc": "2.0", "id": i, "method": method, "params": params}); return i
    def recv(self):
        line = self.p.stdout.readline()
        if not line: raise SystemExit("%s: adapter closed stdout" % label)
        m = json.loads(line); self.log.append(m)
        if m.get("method") == "session/request_permission" and "id" in m:
            ops = m["params"].get("options", [])
            want = self.answers.get("permission", "allow_once")
            pick = next((o for o in ops if o.get("kind") == want), ops[0])
            self.send({"jsonrpc": "2.0", "id": m["id"], "result": {"outcome": {"outcome": "selected", "optionId": pick["optionId"]}}})
        elif "method" in m and "id" in m:
            self.send({"jsonrpc": "2.0", "id": m["id"], "result": {}})
        return m
    def wait(self, i):
        while True:
            m = self.recv()
            if m.get("id") == i and "method" not in m: return m
    def until(self, pred):
        for m in self.log:
            if pred(m): return m
        while True:
            m = self.recv()
            if pred(m): return m
    def close(self):
        self.p.stdin.close()
        try: self.p.wait(5)
        except subprocess.TimeoutExpired: self.p.kill()

def fixture_lines():
    try: return open(log_path).read().splitlines()
    except FileNotFoundError: return []

COMMAND_METHODS = ("thread/compact/start", "review/start", "thread/name/set", "account/logout", "skills/list",
                   "mcpServerStatus/list", "account/rateLimits/read", "thread/goal/set", "thread/goal/clear",
                   "thread/settings/update", "turn/start")

def step(x, sid, text, settle=None):
    """One prompt: what the client saw and what the app-server was asked."""
    l0, f0 = len(x.log), len(fixture_lines())
    i = x.req("session/prompt", {"sessionId": sid, "prompt": [{"type": "text", "text": text}]})
    r = x.wait(i)
    if settle: x.until(settle)
    msgs = x.log[l0:]
    ups = [m["params"]["update"] for m in msgs if m.get("method") == "session/update"]
    calls = [l for l in fixture_lines()[f0:] if l.split(" ")[0] in COMMAND_METHODS]
    return {
        "stop": (r.get("result") or {}).get("stopReason"), "error": (r.get("error") or {}).get("message"),
        "text": "".join(u["content"].get("text", "") for u in ups if u.get("sessionUpdate") == "agent_message_chunk" and isinstance(u.get("content"), dict)),
        "titles": [u.get("title") for u in ups if u.get("sessionUpdate") == "session_info_update" and "title" in u],
        "goal_updates": [(u.get("_meta") or {}).get("goal", {}) and ((u.get("_meta") or {}).get("goal") or {}).get("status") for u in ups
                         if u.get("sessionUpdate") == "session_info_update" and "goal" in (u.get("_meta") or {})],
        "collaboration": [next((o.get("currentValue") for o in u.get("configOptions", []) if o.get("id") == "collaboration_mode"), None)
                          for u in ups if u.get("sessionUpdate") == "config_option_update"],
        "permissions": [m["params"]["toolCall"].get("title") for m in msgs if m.get("method") == "session/request_permission"],
        "calls": calls,
    }

res = {"label": label}
x = Agent()
x.wait(x.req("initialize", {"protocolVersion": 1, "clientCapabilities": {}, "clientInfo": {"name": "oracle", "version": "1"}}))
r = x.wait(x.req("session/new", {"cwd": os.getcwd(), "mcpServers": []}))["result"]
sid = r["sessionId"]
res["config_ids"] = [o.get("id") for o in r.get("configOptions", [])]
cmds = x.until(lambda m: m.get("method") == "session/update" and m["params"]["update"].get("sessionUpdate") == "available_commands_update")
res["commands"] = [(c["name"], (c.get("input") or {}).get("hint"), ((c.get("_meta") or {}).get("commandAction") or {}).get("kind"))
                   for c in cmds["params"]["update"]["availableCommands"]]
steps = {}
steps["first prompt (fallback title)"] = step(x, sid, "hello   world\n from the oracle")
steps["/status"] = step(x, sid, "/status")
steps["/skills"] = step(x, sid, "/skills")
steps["/mcp"] = step(x, sid, "/mcp")
steps["/rename"] = step(x, sid, "/rename  Release prep ")
steps["/rename (no name)"] = step(x, sid, "/rename")
steps["/compact"] = step(x, sid, "/compact")
steps["/review"] = step(x, sid, "/review")
steps["/review custom"] = step(x, sid, "/review focus on locking")
steps["/review-branch"] = step(x, sid, "/review-branch main")
steps["/review-branch (no branch)"] = step(x, sid, "/review-branch")
steps["/review-commit"] = step(x, sid, "/review-commit abc1234")
steps["/goal (no args)"] = step(x, sid, "/goal")
steps["/goal objective"] = step(x, sid, "/goal Ship the release")
steps["/goal pause"] = step(x, sid, "/goal pause")
steps["/goal clear"] = step(x, sid, "/goal clear")
steps["/plan"] = step(x, sid, "/plan")
steps["plan-mode prompt, approve"] = step(x, sid, "plan-turn: change the parser")
steps["/plan again (on)"] = step(x, sid, "/plan")
x.answers["permission"] = "reject_once"
steps["plan-mode prompt, revise"] = step(x, sid, "plan-turn: change the lexer")
x.answers["permission"] = "allow_once"
steps["/plan off"] = step(x, sid, "/plan")
steps["$skill prompt"] = step(x, sid, "/$deploy now")
steps["/logout"] = step(x, sid, "/logout")
x.close()
# History title on session/load in a new process.
y = Agent()
y.wait(y.req("initialize", {"protocolVersion": 1, "clientCapabilities": {}, "clientInfo": {"name": "oracle", "version": "1"}}))
l0 = len(y.log)
y.wait(y.req("session/load", {"sessionId": sid, "cwd": os.getcwd(), "mcpServers": []}))
ups = [m["params"]["update"] for m in y.log[l0:] if m.get("method") == "session/update"]
res["load_titles"] = [u.get("title") for u in ups if u.get("sessionUpdate") == "session_info_update" and "title" in u]
y.close()
res["steps"] = steps
if "--robustness" in opts:
    # Not compared with the node adapter (it waits 1 s on a timer and sends a
    # continuation prompt where this ends at once): asserted by the caller.
    rob = {}
    z = Agent()
    z.wait(z.req("initialize", {"protocolVersion": 1, "clientCapabilities": {}, "clientInfo": {"name": "oracle", "version": "1"}}))
    zs = z.wait(z.req("session/new", {"cwd": os.getcwd(), "mcpServers": []}))["result"]["sessionId"]
    i = z.req("session/prompt", {"sessionId": zs, "prompt": [{"type": "text", "text": "/goal no-turn objective"}]})
    z.until(lambda m: m.get("method") == "session/update" and "goal" in (m["params"]["update"].get("_meta") or {}))
    z.send({"jsonrpc": "2.0", "method": "session/cancel", "params": {"sessionId": zs}})
    rob["goal without a turn, then cancel"] = (z.wait(i).get("result") or {}).get("stopReason")
    rob["next prompt after it"] = step(z, zs, "/skills")["stop"]
    i = z.req("session/prompt", {"sessionId": zs, "prompt": [{"type": "text", "text": "/rename die"}]})
    r = z.wait(i)
    rob["app-server exits during /rename"] = "error" if "error" in r else (r.get("result") or {}).get("stopReason")
    z.close()
    env["FIX_REJECT_SETTINGS"] = "1"
    w = Agent()
    w.wait(w.req("initialize", {"protocolVersion": 1, "clientCapabilities": {}, "clientInfo": {"name": "oracle", "version": "1"}}))
    ws = w.wait(w.req("session/new", {"cwd": os.getcwd(), "mcpServers": []}))["result"]["sessionId"]
    r = w.wait(w.req("session/set_config_option", {"sessionId": ws, "configId": "collaboration_mode", "value": "plan"}))
    rob["rejected plan mode: set_config_option"] = "error" if "error" in r else "ok"
    st = step(w, ws, "/plan")
    rob["rejected plan mode: /plan"] = "error" if st["error"] else st["stop"]
    rob["rejected plan mode: next turn's mode"] = [c for c in st["calls"] if c.startswith("turn/start ")] or None
    st = step(w, ws, "hello after")
    rob["rejected plan mode: turn runs in"] = [c.split(" ")[1] for c in st["calls"] if c.startswith("turn/start ")]
    w.close()
    res["robust"] = rob
# The project folder differs per run: write it as <cwd>.
text = json.dumps(res, indent=1).replace(json.dumps(os.getcwd())[1:-1], "<cwd>")
open(out_path, "w").write(text)
print("%s: %d steps" % (label, len(steps)))
