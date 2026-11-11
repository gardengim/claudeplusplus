#!/usr/bin/env python3
"""Summarize bench/results/<date>-multi.json (from multi.py): mean over reps per scenario x arm, with min-max for latency."""
import json, sys, statistics as st
res = json.load(open(sys.argv[1] if len(sys.argv) > 1 else "bench/results/2026-11-09-multi.json"))
ORDER = ["sep-same", "sep-diff", "tabs-same", "tabs-diff"]; ARMS = ["claude", "claude++", "claude++/opt"]
def mean(v): v = [x for x in v if x is not None]; return round(st.mean(v), 2) if v else None
def rng(v): v = [x for x in v if x is not None]; return f"{min(v):.1f}-{max(v):.1f}" if v else "-"
def row(rs):
    lat = lambda t: [x["latency_s"] for r in rs for x in r[t] if x.get("answered")]
    pre = lambda t: [x["pre_model_s"] for r in rs for x in r[t] if x.get("pre_model_s") is not None]
    ctx = [(r["usage"]["input"] + r["usage"]["cache_write"] + r["usage"]["cache_read"]) / max(1, r["usage"]["requests"]) / 1000 for r in rs]
    ans = sum(x.get("answered", False) for r in rs for t in ("turn1", "turn2") for x in r[t]); tot = sum(len(r["turn1"]) + len(r["turn2"]) for r in rs)
    return {"n": len(rs), "load": mean([r["load"] for r in rs]), "paint": mean([r["first_paint_s"] for r in rs]),
            "S0": mean([r["S0"]["mb"] for r in rs]), "S1": mean([r["S1"]["mb"] for r in rs]), "S2": mean([r["S2"]["mb"] for r in rs]),
            "S3": mean([r["S3"]["mb"] for r in rs]), "S4": mean([r["S4"]["mb"] for r in rs]),
            "procs": mean([r["S2"]["procs"] for r in rs]),
            "cpu_idle": mean([(r["S3"]["cpu_s"] - r["S2"]["cpu_s"]) for r in rs]), "cpu_tot": mean([r["S4"]["cpu_s"] for r in rs]),
            "t1": mean(lat("turn1")), "t1r": rng(lat("turn1")), "t2": mean(lat("turn2")), "t2r": rng(lat("turn2")),
            "pre1": mean(pre("turn1")), "pre2": mean(pre("turn2")), "wall1": mean([r["turn1_wall_s"] for r in rs]), "wall2": mean([r["turn2_wall_s"] for r in rs]),
            "reqs": mean([r["usage"]["requests"] for r in rs]), "ctx_k": mean(ctx), "out": mean([r["usage"]["output"] for r in rs]),
            "cw": mean([r["usage"]["cache_write"] / 1000 for r in rs]), "ok": f"{ans}/{tot}", "leak": sum(r["leaked_procs"] for r in rs)}
cols = [("n", "n"), ("load", "load"), ("paint", "paint s"), ("S0", "open MB"), ("S1", "ready MB"), ("S2", "turn1 MB"), ("S3", "idle MB"), ("S4", "turn2 MB"), ("procs", "procs"), ("cpu_idle", "cpu/idle"), ("cpu_tot", "cpu tot s"),
        ("t1", "t1 s"), ("t1r", "t1 range"), ("t2", "t2 s"), ("t2r", "t2 range"), ("pre1", "pre1"), ("pre2", "pre2"), ("wall1", "wall1"), ("wall2", "wall2"), ("reqs", "reqs"), ("ctx_k", "ctx k/req"), ("cw", "cache-wr k"), ("out", "out tok"), ("ok", "answered"), ("leak", "leak")]
print("%-10s %-13s " % ("scenario", "arm") + " ".join("%9s" % c[1] for c in cols))
for s in ORDER:
    for a in ARMS:
        rs = [r for r in res if r["scenario"] == s and r["arm"] == a]
        if rs: x = row(rs); print("%-10s %-13s " % (s, a) + " ".join("%9s" % x[c[0]] for c in cols))
print("\nt1/t2 = send -> answer in transcript, mean over sessions and reps (range = min-max over every session); pre = send -> user line in transcript\n(claude++ start/resume + MCP wait); wall = first send -> last answer; ctx = (input+cache write+cache read)/request")
