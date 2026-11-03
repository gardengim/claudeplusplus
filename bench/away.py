#!/usr/bin/env python3
"""One claude++ tab, one turn, then you walk away: is the tab in front given back (not just background tabs)?
Measures the process tree right after the turn and after 5x --idle-suspend with no input, then types again and
checks the conversation survived the stop. Usage: python3 bench/away.py   (one haiku turn + one resume, ~1 min)"""
import sys
_src = open(__file__.replace("away.py", "tabs.py")).read().split("os.makedirs(W")[0]  # reuse tabs.py's Term/tree/footprint helpers
sys.argv = sys.argv[:1]
exec(_src)
os.makedirs(W, exist_ok=True)
SUSPEND = 4  # seconds; the front tab stops after 5x this with no input
t = Term([CPP, "--model", "haiku", "--idle-suspend", str(SUSPEND)])
t.mark = 0; t.drain(3)
ok = t.ask(PROMPTS[0], b"Worked for")
t.drain(1)
ps = tree(t.pid); busy = {"procs": names(ps), "mb": footprint(ps)}
t.drain(SUSPEND * 5 + 20)  # no input: tick samples every 15 s
ps = tree(t.pid); away = {"procs": names(ps), "mb": footprint(ps)}
t.ask("What number did you answer in your previous reply? Reply with just that number followed by the letter Q.", b"Worked for")
recalled = b"42" in ANSI.sub(b"", bytes(t.buf[t.mark:]))
t.drain(1); t.close()
res = {"answered": ok, "after the turn": busy, "away": away, "recalled 42 after the stop": recalled}
print(json.dumps(res, indent=1))
json.dump(res, open(__file__.replace("away.py", "results/%s-away.json" % time.strftime("%Y-%m-%d")), "w"), indent=1)
good = ok and recalled and away["mb"] < 20 and "claude" not in away["procs"]
print("PASS" if good else "FAIL")
sys.exit(0 if good else 1)
