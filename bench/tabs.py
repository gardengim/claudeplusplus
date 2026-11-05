#!/usr/bin/env python3
"""Three conversations left open: three `claude` windows vs three claude++ tabs (stdlib, macOS, haiku).
Each conversation gets one turn, then everything sits idle for IDLE seconds and the process trees are measured
with macOS `footprint`. claude++ runs with `--idle-suspend 20`, so its two background tabs stop their backend.
Then tab 1 (suspended) gets a question that needs the earlier turn, to show the context survived, and the
resumed request's prompt-cache use is read from the session transcript.
Usage: python3 bench/tabs.py [IDLE seconds, default 45]"""
from _term import *
IDLE = int(sys.argv[1]) if len(sys.argv) > 1 else 45
PROMPTS = [f"What is {a} times {b}? Reply with the number followed by the letter Z and nothing else." for a, b in [(6, 7), (5, 9), (8, 8)]]
ANSWERS = [b"42Z", b"45Z", b"64Z"]

os.makedirs(W, exist_ok=True)
res = {}

# Three windows of stock claude, one conversation each.
ts = [Term(["claude", "--model", "haiku"]) for _ in PROMPTS]
for t in ts: t.mark = 0; t.drain(10)
ok = [t.ask(p, a) for t, p, a in zip(ts, PROMPTS, ANSWERS)]
for t in ts: t.drain(IDLE / len(ts))
ps = [p for t in ts for p in tree(t.pid)]
res["claude, 3 windows"] = {"answered": sum(ok), "procs": names(ps), "mb": footprint(ps)}
for t in ts: t.close()

# One claude++ with three tabs.
t = Term([CPP, "--model", "haiku", "--idle-suspend", "20"])
t.mark = 0; t.drain(3)
ok = []
for i, (p, a) in enumerate(zip(PROMPTS, ANSWERS)):
    if i: os.write(t.fd, b"\x14"); t.drain(1)  # ctrl+t: new tab
    ok.append(t.ask(p, b"Worked for"))
t.drain(max(IDLE, 25))  # past --idle-suspend, so the two background tabs stop their backend
ps = tree(t.pid)
res["claude++, 3 tabs"] = {"answered": sum(ok), "procs": names(ps), "mb": footprint(ps)}
# Back to tab 1 (its backend was stopped): does it still know the earlier answer?
os.write(t.fd, b"\x1b1"); t.drain(1)
since = time.time()
t.ask("What number did you answer in your previous reply? Reply with just that number followed by the letter Q.", b"Worked for")
recalled = b"42" in ANSI.sub(b"", bytes(t.buf[t.mark:]))
t.drain(2)
t.close()
# The resumed request's usage, from the newest transcript in this folder.
hit = None  # the resumed request's usage, from whichever transcript got it
for f in glob.glob(os.path.expanduser("~/.claude/projects/-private-tmp-cppbench-w/*.jsonl")):
    if os.path.getmtime(f) < since: continue
    for l in open(f):
        v = json.loads(l)
        u = (v.get("message") or {}).get("usage")
        if v.get("type") == "assistant" and u and v.get("timestamp", "") >= time.strftime("%Y-%m-%dT%H:%M:%S", time.gmtime(since)):
            hit = {"cache_read": u.get("cache_read_input_tokens"), "cache_write": u.get("cache_creation_input_tokens"), "input": u.get("input_tokens")}
res["claude++, 3 tabs"].update({"tab 1 resumed and recalled 42": recalled, "resumed request": hit})
print(json.dumps(res, indent=1))
os.makedirs("bench/results", exist_ok=True)
json.dump(res, open("bench/results/" + time.strftime("%Y-%m-%d") + "-tabs.json", "w"), indent=1)
