#!/usr/bin/env python3
"""Where do your tokens go? Reads only the token counts in your own Claude Code transcripts
(~/.claude/projects/*/*.jsonl; no message text is read into the output, nothing leaves the machine).
Every request re-reads the whole conversation from the prompt cache, so long conversations dominate:
this shows how much of that reading happens at what context size, and what a compaction size would have changed.
  python3 bench/context_tax.py            # all transcripts
  python3 bench/context_tax.py 30         # only requests from the last 30 days"""
import datetime, glob, json, os, sys, collections

days = int(sys.argv[1]) if len(sys.argv) > 1 else 0
since = (datetime.datetime.now(datetime.timezone.utc) - datetime.timedelta(days=days)).isoformat() if days else ""
reqs = {}
for f in glob.glob(os.path.expanduser("~/.claude/projects/*/*.jsonl")):
    try:
        for l in open(f, errors="ignore"):
            if '"usage"' not in l: continue
            try: v = json.loads(l)
            except ValueError: continue
            m, u = v.get("message") or {}, (v.get("message") or {}).get("usage")
            if v.get("type") != "assistant" or not u or v.get("isSidechain") or (since and (v.get("timestamp") or "") < since): continue
            reqs[m.get("id") or f + l[:30]] = (f, u.get("input_tokens") or 0, u.get("cache_creation_input_tokens") or 0, u.get("cache_read_input_tokens") or 0, u.get("output_tokens") or 0)
    except OSError:
        pass
if not reqs: sys.exit("no transcripts found")
R = list(reqs.values())
rd, wr, out = sum(r[3] for r in R), sum(r[2] for r in R), sum(r[4] for r in R)
print(f"{len(R)} requests in {len({r[0] for r in R})} conversations: cache reads {rd/1e6:.1f}M, cache writes {wr/1e6:.1f}M, output {out/1e6:.2f}M tokens")
# Relative cost in units of one fresh input token: reads 0.1, writes 1.25, output 5 (the usual ratios).
cost = {"reads": rd * 0.1, "writes": wr * 1.25, "output": out * 5.0}
t = sum(cost.values())
print("share of spend (usual price ratios):", ", ".join(f"{k} {100 * v / t:.0f}%" for k, v in cost.items()))
print("\ncontext size of the request     requests   cache reads   share of all reads")
for lo, hi in [(0, 50e3), (50e3, 100e3), (100e3, 200e3), (200e3, 400e3), (400e3, 1e12)]:
    s = [r for r in R if lo <= r[1] + r[2] + r[3] < hi]
    x = sum(r[3] for r in s)
    print(f"  {lo/1e3:>4.0f}k - {('%.0fk' % (hi/1e3)) if hi < 1e12 else 'more':>5}   {len(s):>12}   {x/1e6:>9.1f}M   {100*x/max(rd,1):>8.1f}%")
# What would a compaction size have changed? After a compaction the context grows again from about 40k, so a request
# that was at c > N would have read about (40k + N) / 2 on average instead. Each compaction costs one read of the context,
# a write of the new ~40k start and a ~5k summary of output. Rough, and blind to the real price: working from a summary.
print("\nif Claude Code compacted at N (rough estimate, compactions' own cost included):")
for n in (200e3, 300e3, 400e3):
    saved = sum((c - (40e3 + n) / 2) * 0.1 for c in (r[1] + r[2] + r[3] for r in R) if c > n)
    cycles = 0
    for f in {r[0] for r in R}:
        peak = max(r[1] + r[2] + r[3] for r in R if r[0] == f)
        cycles += max(0, -(-(peak - n) // (n - 40e3))) if peak > n else 0
    paid = cycles * (0.1 * n + 1.25 * 40e3 + 5 * 5e3)
    print(f"  at {n/1e3:>3.0f}k: ~{int(cycles)} compactions; net saving {100 * (saved - paid) / t:2.0f}% of all spend (reads alone {100 * saved / t:2.0f}%)")
