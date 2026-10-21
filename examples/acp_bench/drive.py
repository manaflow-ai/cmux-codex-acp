#!/usr/bin/env python3
"""ACP client that runs one scenario against an adapter and reports cost.

drive.py --label NAME [--deltas N] -- <adapter argv...>
Scenario (all over stdio, recorded fixture, no model): initialize,
session/new, prompt (reasoning, a command that asks permission, output, a
long Markdown reply), prompt "cancel-me" + session/cancel, then a new
process: initialize + session/load (resume). Prints one JSON line.
Cost is the adapter process only (its children are the fake app-server).
"""
import json, os, subprocess, sys, time, glob, signal

a = sys.argv[1:]; split = a.index("--"); opts, argv = a[:split], a[split + 1:]
def opt(k, d=None): return opts[opts.index(k) + 1] if k in opts else d
label = opt("--label", argv[0])
LAST = []
def _timeout(*_):
    sys.stderr.write("%s: timed out; last messages: %s\n" % (label, json.dumps(LAST[-3:])[:2000])); os._exit(3)
signal.signal(signal.SIGALRM, _timeout); signal.alarm(int(opt("--timeout", "90")))

def cpu_ns(pid):
    t = 0
    for f in glob.glob("/proc/%d/task/*/schedstat" % pid):
        try: t += int(open(f).read().split()[0])
        except Exception: pass
    return t
def status(pid, key):
    for l in open("/proc/%d/status" % pid):
        if l.startswith(key + ":"): return int(l.split()[1])
def threads(pid): return status(pid, "Threads")

class Agent:
    def __init__(self):
        self.t0 = time.perf_counter()
        self.p = subprocess.Popen(argv, stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=open("/tmp/adapter-%s.err" % label, "ab"))
        self.next = 1; self.bytes = 0; self.msgs = 0; self.log = []
    def send(self, m):
        self.p.stdin.write((json.dumps(m) + "\n").encode()); self.p.stdin.flush()
    def req(self, method, params):
        i = self.next; self.next += 1
        self.send({"jsonrpc": "2.0", "id": i, "method": method, "params": params}); return i
    def recv(self):
        line = self.p.stdout.readline()
        if not line: raise SystemExit("%s: adapter closed stdout (see /tmp/adapter-%s.err)" % (label, label))
        self.bytes += len(line); self.msgs += 1
        m = json.loads(line); self.log.append(m); LAST.append(m)
        if m.get("method") == "session/request_permission" and "id" in m:
            opts_ = m["params"].get("options", [])
            pick = next((o for o in opts_ if o.get("kind") == "allow_once"), opts_[0] if opts_ else None)
            self.send({"jsonrpc": "2.0", "id": m["id"], "result": {"outcome": {"outcome": "selected", "optionId": pick["optionId"]}}})
        elif "method" in m and "id" in m:
            self.send({"jsonrpc": "2.0", "id": m["id"], "result": {}})
        return m
    def wait(self, i, on=None):
        while True:
            m = self.recv()
            if on: on(m)
            if m.get("id") == i and "method" not in m:
                if "error" in m: raise SystemExit("%s: error %s" % (label, m["error"]))
                return m["result"]
    def close(self):
        self.p.stdin.close()
        try: self.p.wait(5)
        except subprocess.TimeoutExpired: self.p.kill()

cwd = os.getcwd()
def updates(log):
    return [m["params"]["update"] for m in log if m.get("method") == "session/update"]
def text(us, kind): return "".join(u.get("content", {}).get("text", "") for u in us if u.get("sessionUpdate") == kind and isinstance(u.get("content"), dict))

r = {"label": label}
x = Agent(); pid = x.p.pid
i = x.req("initialize", {"protocolVersion": 1, "clientCapabilities": {"fs": {"readTextFile": False, "writeTextFile": False}, "terminal": False}, "clientInfo": {"name": "bench", "version": "1"}})
init = x.wait(i); r["init_ms"] = round((time.perf_counter() - x.t0) * 1e3, 1)
i = x.req("session/new", {"cwd": cwd, "mcpServers": []}); s = x.wait(i); sid = s["sessionId"]
r["ready_ms"] = round((time.perf_counter() - x.t0) * 1e3, 1)
r["idle_rss_kib"] = status(pid, "VmRSS"); r["threads"] = threads(pid)
r["startup_cpu_ms"] = round(cpu_ns(pid) / 1e6, 1)
c0, b0, m0, l0 = cpu_ns(pid), x.bytes, x.msgs, len(x.log)
t1 = time.perf_counter()
first = {}
def on(m):
    if "first" not in first and m.get("method") == "session/update" and m["params"]["update"].get("sessionUpdate") == "agent_message_chunk":
        first["first"] = time.perf_counter()
i = x.req("session/prompt", {"sessionId": sid, "prompt": [{"type": "text", "text": "run the tests and explain"}]})
pr = x.wait(i, on)
r["turn_ms"] = round((time.perf_counter() - t1) * 1e3, 1)
c1 = cpu_ns(pid); turn_log = x.log[l0:]
us = updates(turn_log)
chunks = [u for u in us if u.get("sessionUpdate") == "agent_message_chunk"]
n = int(os.environ.get("FIX_DELTAS", "4000"))
r["turn_cpu_ms"] = round((c1 - c0) / 1e6, 1)
r["cpu_us_per_delta"] = round((c1 - c0) / 1e3 / max(1, n), 2)
r["turn_bytes"] = x.bytes - b0; r["turn_msgs"] = x.msgs - m0
r["chunk_msgs"] = len(chunks)
r["bytes_per_chunk"] = round(sum(len(json.dumps({"jsonrpc": "2.0", "method": "session/update", "params": {"sessionId": sid, "update": u}}, separators=(",", ":"))) for u in chunks) / max(1, len(chunks)), 1)
r["peak_rss_kib"] = status(pid, "VmHWM"); r["rss_after_kib"] = status(pid, "VmRSS")
# behavior summary (the oracle comparison)
b = {"stop": pr.get("stopReason"), "agent_text_len": len(text(us, "agent_message_chunk")),
     "agent_text_sha": __import__("hashlib").sha256(text(us, "agent_message_chunk").encode()).hexdigest()[:12],
     "thought_text_len": len(text(us, "agent_thought_chunk")),
     "permission_asked": any(m.get("method") == "session/request_permission" for m in turn_log),
     "tool_call": any(u.get("sessionUpdate") == "tool_call" for u in us),
     "tool_completed": any(u.get("sessionUpdate") in ("tool_call", "tool_call_update") and u.get("status") == "completed" for u in us),
     "kinds": sorted(set(u.get("sessionUpdate") for u in us))}
# cancel
l0 = len(x.log)
i = x.req("session/prompt", {"sessionId": sid, "prompt": [{"type": "text", "text": "cancel-me please"}]})
while True:
    m = x.recv()
    if m.get("method") == "session/update" and m["params"]["update"].get("sessionUpdate") == "agent_message_chunk": break
tc = time.perf_counter()
x.send({"jsonrpc": "2.0", "method": "session/cancel", "params": {"sessionId": sid}})
cr = x.wait(i); b["cancel_stop"] = cr.get("stopReason"); r["cancel_ms"] = round((time.perf_counter() - tc) * 1e3, 1)
r["cpu_total_ms"] = round(cpu_ns(pid) / 1e6, 1)
x.close()
# resume in a new process
y = Agent()
i = y.req("initialize", {"protocolVersion": 1, "clientCapabilities": {}, "clientInfo": {"name": "bench", "version": "1"}})
init2 = y.wait(i)
b["load_session_cap"] = bool((init2.get("agentCapabilities") or {}).get("loadSession"))
if b["load_session_cap"]:
    i = y.req("session/load", {"sessionId": sid, "cwd": cwd, "mcpServers": []}); y.wait(i)
    rus = updates(y.log)
    b["resume_replay"] = [u.get("sessionUpdate") for u in rus if u.get("sessionUpdate") in ("user_message_chunk", "agent_message_chunk")]
    b["resume_text"] = text(rus, "user_message_chunk") + "|" + text(rus, "agent_message_chunk")
y.close()
r["behavior"] = b
print(json.dumps(r))
