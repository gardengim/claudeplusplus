#!/usr/bin/env python3
"""Real-binary checks of the supervisor's window logic (stdlib, macOS; haiku, cents). Run from the repo root.
  focus: three separate windows, real terminal focus events (CSI I / CSI O) through the pty.
         A: window 1 focused, 2 and 3 not -> only window 1 keeps its claude.   B: window 1 loses focus too -> none keeps one.
         C: focus returns to window 3; messages to 3 and to the stopped window 1 are both answered.
  busy:  an unfocused window in the middle of a long answer is never stopped mid-turn, even with --idle-suspend 10.
Needs the folders multi.py trusted (/private/tmp/cppbench/w/multi/claudepp_opt/{a,b,c}). Each test uses its own
supervisor directory (CLAUDEPP_SUP_DIR), so nothing here talks to the supervisor your real windows use.
Usage: python3 bench/focus.py [focus|busy ...]"""
import sys, os, time, json, glob, select, shutil
sys.path.insert(0, os.path.dirname(__file__))
from _term import Term, tree, footprint, names, CPP

ROOT = "/private/tmp/cppbench/w/multi/claudepp_opt"
PROJ = os.path.expanduser("~/.claude/projects")
IN, OUT = b"\x1b[I", b"\x1b[O"   # what a terminal sends when its window gains / loses focus

def pump(ts, secs, stop=None):
    end = time.time() + secs
    while time.time() < end:
        for t in ts:
            if not t.alive: continue
            r, _, _ = select.select([t.fd], [], [], 0.02)
            if r:
                try: d = os.read(t.fd, 65536)
                except OSError: t.alive = False; continue
                t.buf.extend(d)
                if b"\x1b[6n" in d: os.write(t.fd, b"\x1b[1;1R")
                if b"\x1b[c" in d or b"\x1b[0c" in d: os.write(t.fd, b"\x1b[?62;c")
        if stop and stop(): return True
    return False

def w(t, b):
    try: os.write(t.fd, b)
    except OSError: t.alive = False

def launch(tag, idle, folders, extra=()):
    base = f"/private/tmp/cppbench/w/multi/_runs/{tag}"; shutil.rmtree(base, ignore_errors=True); os.makedirs(base)
    cfg = base + "/xdg/claude++"; os.makedirs(cfg)
    c = json.load(open(os.path.expanduser("~/.config/claude++/config.json"))); c["input_mode"] = "default"
    json.dump(c, open(cfg + "/config.json", "w"))
    env = {"CLAUDEPP_SUP_DIR": base + "/sup", "OPENVIKING_MEMORY_ENABLED": "0", "XDG_CONFIG_HOME": base + "/xdg"}
    ts = []
    for f in folders:
        t = Term([CPP, "--model", "haiku", "--idle-suspend", str(idle), *extra], cwd=f"{ROOT}/{f}", env_extra=env); t.alive = True; ts.append(t)
    pump(ts, 15)
    for t in ts: w(t, b"x"); pump(ts, 0.3); w(t, b"\x7f"); pump(ts, 0.3)
    pump(ts, 12)
    return ts

def snap(ts):
    pids = sorted({p for t in ts for p in tree(t.pid)})
    import subprocess
    rows = [l.split(None, 3) for l in subprocess.run(["ps", "-A", "-o", "pid=,ppid=,stat=,command="], capture_output=True, text=True).stdout.splitlines()]
    z = [r[0] for r in rows if len(r) > 2 and r[2].startswith("Z") and int(r[0]) in pids]
    return {"claude": names(pids).get("claude", 0), "per_window": [names(tree(t.pid)).get("claude", 0) for t in ts], "mb": footprint(pids), "zombies": z}

def answered(folder, text, since):
    for f in glob.glob(f"{PROJ}/{''.join(c if c.isalnum() else '-' for c in ROOT + '/' + folder)}/*.jsonl"):
        if os.path.getmtime(f) < since - 1: continue
        for l in open(f, errors="replace"):
            try: v = json.loads(l)
            except ValueError: continue
            m = v.get("message") or {}
            if v.get("type") == "assistant" and isinstance(m.get("content"), list) and any(b.get("type") == "text" and text in (b.get("text") or "") for b in m["content"]): return True
    return False

def ask(ts, i, prompt):
    w(ts[i], prompt.encode()); pump(ts, 0.5); w(ts[i], b"\r")

def wait(ts, checks, limit=150):
    return pump(ts, limit, stop=lambda: (time.sleep(0.2), all(c() for c in checks))[1])

def focus():
    t0 = time.time(); ts = launch("focus", 40, "abc"); res = {}
    q = [("What is 6 times 7? Reply with the number followed by the letter Z and nothing else.", "42Z"), ("What is 5 times 9? Reply with the number followed by the letter Z and nothing else.", "45Z"), ("What is 8 times 8? Reply with the number followed by the letter Z and nothing else.", "64Z")]
    for i in range(3): ask(ts, i, q[i][0])
    wait(ts, [lambda i=i: answered("abc"[i], q[i][1], t0) for i in range(3)])
    res["after_turn1"] = snap(ts)
    w(ts[0], IN); w(ts[1], OUT); w(ts[2], OUT); pump(ts, 75); res["A_one_focused"] = snap(ts)
    w(ts[0], OUT); pump(ts, 75); res["B_none_focused"] = snap(ts)
    w(ts[2], IN); pump(ts, 1); t1 = time.time()
    ask(ts, 2, "Now add 6 to your previous answer. Reply with the new number followed by the letter Q and nothing else."); ask(ts, 0, "Now add 8 to your previous answer. Reply with the new number followed by the letter Q and nothing else.")
    ok = wait(ts, [lambda: answered("c", "70Q", t1), lambda: answered("a", "50Q", t1)])
    res["C_both_resumed_and_answered"] = ok; res["after_C"] = snap(ts)
    for t in ts: t.close()
    verdict = {"A: only the focused window keeps its claude": res["A_one_focused"]["per_window"] == [1, 0, 0],
               "B: with no window focused none keeps one": res["B_none_focused"]["claude"] == 0,
               "C: focus back, stopped windows answer": ok, "no zombies": not any(res[k]["zombies"] for k in ("after_turn1", "A_one_focused", "B_none_focused", "after_C"))}
    return res, verdict

def busy():
    # A long generation keeps the tab busy for far longer than --idle-suspend 10 (no tools, no permission changes).
    t0 = time.time(); ts = launch("busy", 10, "ab"); res = {}
    ask(ts, 0, "Write the whole numbers from 1 to 6000 separated by commas, then a final line containing only ENDMARK9. Output nothing else.")
    pump(ts, 3); w(ts[0], OUT); w(ts[1], IN)
    checks = []   # (turn still running at this moment, claude processes in window 1)
    for _ in range(8):
        pump(ts, 6)
        running = not answered("a", "ENDMARK9", t0)
        checks.append((running, names(tree(ts[0].pid)).get("claude", 0)))
        if not running: break
    during = [n for r, n in checks if r]
    res["checks_running_then_claude_procs"] = checks; res["seconds_busy_at_least"] = 3 + 6 * len(during)
    ok = wait(ts, [lambda: answered("a", "ENDMARK9", t0)], limit=200)
    res["answered"] = ok
    for t in ts: t.close()
    return res, {"the busy window was never stopped": bool(during) and all(n >= 1 for n in during), "it was busy for at least 30 s while unfocused with --idle-suspend 10 (test is meaningful)": len(during) >= 5, "its turn finished": ok}

if __name__ == "__main__":
    out = {}
    for name in (sys.argv[1:] or ["focus", "busy"]):
        res, verdict = {"focus": focus, "busy": busy}[name]()
        out[name] = {"data": res, "verdict": verdict}
        print(name, json.dumps(res)); [print(("PASS " if v else "FAIL ") + k) for k, v in verdict.items()]
    json.dump(out, open("bench/results/" + time.strftime("%Y-%m-%d") + "-focus.json", "w"), indent=1)
