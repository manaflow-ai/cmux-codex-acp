#!/usr/bin/env python3
"""compare.py ORACLE.json CANDIDATE.json: per step, the fields that differ."""
import json, sys
o, c = (json.load(open(f)) for f in sys.argv[1:3])
bad = 0
# Known, intended differences (named in the PR):
# - config option order: the bridge keeps its order (model, effort, mode) and appends collaboration_mode.
# - the node adapter re-reads skills/list before every prompt; the bridge on session open and skills/changed.
# - a command approval's title: the bridge names the command, the adapter says "Run command".
# - the generated title arrives asynchronously in both: compared across the run, not per step.
GEN = "Greeting from the oracle"
def gen_titles(d):
    return sorted(t for st in d["steps"].values() for t in st.get("titles", []) if t == GEN)
def norm_step(st, step):
    st = dict(st)
    st["titles"] = [t for t in st.get("titles", []) if t != GEN]
    st["calls"] = [c for c in st.get("calls", []) if c != "turn/start" and "generate a very short title" not in c and GEN not in c]
    if step != "/skills":
        st["calls"] = [c for c in st.get("calls", []) if not c.startswith("skills/list")]
    st["permissions"] = [p if p == "Implement this plan?" else "<tool approval>" for p in st.get("permissions", [])]
    return st
o["config_ids"] = sorted(o.get("config_ids") or []); c["config_ids"] = sorted(c.get("config_ids") or [])
o["generated_titles"] = gen_titles(o); c["generated_titles"] = gen_titles(c)
for k in ("commands", "config_ids", "load_titles", "generated_titles"):
    same = o.get(k) == c.get(k)
    print(("SAME " if same else "DIFF ") + k + ("" if same else "\n  oracle:    %s\n  candidate: %s" % (o.get(k), c.get(k))))
    bad += not same
for name, os_ in o["steps"].items():
    os_ = norm_step(os_, name); cs = norm_step(c["steps"].get(name, {}), name)
    diffs = [f for f in os_ if os_.get(f) != cs.get(f)]
    if not diffs:
        print("SAME " + name); continue
    bad += 1
    print("DIFF " + name)
    for f in diffs:
        print("  %s\n    oracle:    %s\n    candidate: %s" % (f, json.dumps(os_.get(f))[:600], json.dumps(cs.get(f))[:600]))
print("%d differing" % bad)
