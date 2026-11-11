#!/usr/bin/env python3
"""Latency, overhead, memory and tokens of ONE conversation per arm, arms interleaved (stdlib, macOS; haiku, cents).
Arms: claude (stock) · claude/flags (stock given the same trimming as the project's optimize.json: --tools, --strict-mcp-config,
--settings with its skill overrides; this separates "claude++" from "the trimming") · claude++ · claude++/opt.
Per run: open, prime, one warm-up turn, then TURNS timed turns 4 s apart. A turn is measured three ways:
  pre   send -> the user line appears in the session transcript (what the frontend does before the model sees it)
  model user line -> the answer in the transcript (the API; identical for every arm that sends the same request)
  seen  send -> the answer is on the terminal (claude: its text; claude++: its "Worked for" line, drawn once the reply is)
Medians with a bootstrap 95% interval; differences against stock claude with theirs. Waits for the machine to calm down
(load < LOAD, at most 2 minutes) and records the load of every run. Usage (repo root): python3 bench/latency.py [rounds]"""
import sys, os, json, time, random, shutil, subprocess, select, glob, datetime, statistics as st
sys.path.insert(0, os.path.dirname(__file__))
from _term import Term, tree, footprint, ANSI, CPP

ROUNDS = int(sys.argv[1]) if len(sys.argv) > 1 else 4
TURNS, LOAD = 6, 12.0
ROOT = "/private/tmp/cppbench/w/multi"
PROJ = os.path.expanduser("~/.claude/projects")
OPT = json.load(open(".claude++/optimize.json"))
FLAGS = ["--tools", ",".join(OPT["tools"]), "--strict-mcp-config", "--settings", json.dumps({"skillListingMaxDescChars": OPT["skillListingMaxDescChars"], "skillOverrides": OPT["skillOverrides"]})]
ARMS = {
    "claude":       dict(argv=["claude", "--model", "haiku"], dir="claude/a", cpp=False, opt=False),
    "claude/flags": dict(argv=["claude", "--model", "haiku"] + FLAGS, dir="claude_flags/a", cpp=False, opt=False),
    "claude++":     dict(argv=[CPP, "--model", "haiku"], dir="claudepp/a", cpp=True, opt=False),
    "claude++/opt": dict(argv=[CPP, "--model", "haiku"], dir="claudepp_opt/a", cpp=True, opt=True),
}
ENV = {"OPENVIKING_MEMORY_ENABLED": "0"}

def encode(p): return "".join(c if c.isalnum() else "-" for c in p)
def ts(v): return datetime.datetime.fromisoformat(v["timestamp"].replace("Z", "+00:00")).timestamp()

def pump(t, secs, stop=None, log=None):
    end = time.time() + secs
    while time.time() < end:
        r, _, _ = select.select([t.fd], [], [], 0.005)
        if r:
            try: d = os.read(t.fd, 65536)
            except OSError: return False
            now = time.time(); t.buf.extend(d)
            if log is not None: log.append((now, bytes(d)))
            if b"\x1b[6n" in d: os.write(t.fd, b"\x1b[1;1R")
            if b"\x1b[c" in d or b"\x1b[0c" in d: os.write(t.fd, b"\x1b[?62;c")
        if stop and stop(): return True
    return False

def prepare(arm):
    a = ARMS[arm]; d = f"{ROOT}/{a['dir']}"
    os.makedirs(d, exist_ok=True)
    if not os.path.exists(d + "/README.md"):
        open(d + "/README.md", "w").write("# bench\nA tiny project for a benchmark.\n"); subprocess.run(["git", "init", "-q"], cwd=d)
    f = d + "/.claude++/optimize.json"
    if a["opt"]: os.makedirs(d + "/.claude++", exist_ok=True); open(f, "w").write(json.dumps(OPT))
    elif os.path.exists(f): os.remove(f)
    try: trusted = json.load(open(os.path.expanduser("~/.claude.json"))).get("projects", {}).get(d, {}).get("hasTrustDialogAccepted")
    except Exception: trusted = False
    if not trusted:   # the folder was made by this script; accept stock claude's one-time question for it
        t = Term(["claude", "--model", "haiku"], cwd=d, env_extra=ENV); pump(t, 40, stop=lambda: b"trust" in bytes(t.buf).replace(b" ", b"").lower())
        pump(t, 1); os.write(t.fd, b"\x1b[B"); pump(t, 0.4); os.write(t.fd, b"\r"); pump(t, 4); t.close()
    return d

def entries(d, since):
    out = []
    for f in glob.glob(f"{PROJ}/{encode(d)}/*.jsonl"):
        if os.path.getmtime(f) < since - 1: continue
        for l in open(f, errors="replace"):
            try: out.append(json.loads(l))
            except ValueError: pass
    return out

def find_turn(d, since, prompt):
    es = entries(d, since); u = next((v for v in es if v.get("type") == "user" and prompt in json.dumps(v.get("message") or {})), None)
    if not u: return None, None, None
    a = None
    for v in es:
        m = v.get("message") or {}
        if v.get("type") == "assistant" and ts(v) >= ts(u) and isinstance(m.get("content"), list):
            txt = "".join(b.get("text") or "" for b in m["content"] if b.get("type") == "text")
            if txt and (a is None or ts(v) < a[0]): a = (ts(v), txt)
    return ts(u), (a[0] if a else None), (a[1] if a else None)

def turn(t, d, since, i, cpp):
    x, y = 11 + i, 23 + 2 * i
    prompt = f"What is {x} plus {y}? Reply with the number followed by the letter Q and nothing else."
    t.buf.clear(); log = []
    t_before = ANSI.sub(b"", bytes(t.buf)).count(b"Worked for")
    os.write(t.fd, prompt.encode()); pump(t, 0.4, log=log)
    sent = time.time(); os.write(t.fd, b"\r")
    def done():
        u, a, _ = find_turn(d, since, prompt); return a is not None
    ok = pump(t, 90, stop=lambda: (time.sleep(0.05), done())[1], log=log)
    u, a, txt = find_turn(d, since, prompt)
    r = {"answered": bool(a)}
    if a: r.update(pre=round(u - sent, 3), model=round(a - u, 3), total=round(a - sent, 3))
    # when it was on the terminal: claude++ draws "Worked for" at the end of the reply; stock claude prints the answer
    seen = None; acc = bytearray(); mark = sent
    pump(t, 1.5, log=log)
    for now, chunk in log:
        if now < sent: continue
        acc.extend(chunk); s = ANSI.sub(b"", bytes(acc))
        if (cpp and b"Worked for" in s) or (not cpp and txt and txt.strip().encode() in s.split(prompt.encode()[-12:])[-1]):
            seen = round(now - sent, 3); break
    r["seen"] = seen
    return r

def wait_calm():
    t = time.time()
    while os.getloadavg()[0] > LOAD and time.time() - t < 120: time.sleep(3)
    return round(os.getloadavg()[0], 1)

def run(arm, rnd):
    a = ARMS[arm]; d = prepare(arm); load = wait_calm()
    base = f"{ROOT}/_runs/latency-{arm.replace('+', 'p').replace('/', '_')}-{rnd}"; shutil.rmtree(base, ignore_errors=True); os.makedirs(base + "/xdg/claude++")
    env = dict(ENV)
    if a["cpp"]:
        c = json.load(open(os.path.expanduser("~/.config/claude++/config.json"))); c["input_mode"] = "default"
        json.dump(c, open(base + "/xdg/claude++/config.json", "w")); env.update(XDG_CONFIG_HOME=base + "/xdg", CLAUDEPP_SUP_DIR=base + "/sup")
    since = time.time(); t0 = time.time()
    t = Term(a["argv"], cwd=d, env_extra=env); first = [None]
    log = []; pump(t, 15, log=log); first_paint = round(log[0][0] - t0, 3) if log else None
    os.write(t.fd, b"x"); pump(t, 0.3); os.write(t.fd, b"\x7f"); pump(t, 12)
    r = {"arm": arm, "round": rnd, "load": load, "first_paint_s": first_paint}
    warm = turn(t, d, since, 0, a["cpp"]); r["warmup"] = warm
    r["turns"] = []
    for i in range(1, TURNS + 1):
        pump(t, 4); r["turns"].append(turn(t, d, since, i, a["cpp"]))
    pids = tree(t.pid); r["mb"] = footprint(pids); r["procs"] = len(pids)
    t.close(); time.sleep(1)
    msgs = {}
    for v in entries(d, since):
        m = v.get("message") or {}
        if v.get("type") == "assistant" and m.get("usage"): msgs[m.get("id")] = m["usage"]
    n = max(1, len(msgs)); r["requests"] = len(msgs)
    r["ctx_per_request"] = round(sum((u.get("input_tokens") or 0) + (u.get("cache_creation_input_tokens") or 0) + (u.get("cache_read_input_tokens") or 0) for u in msgs.values()) / n)
    return r

def boot(v, f=st.median, n=2000):
    rnd = random.Random(5); xs = sorted(f(rnd.choices(v, k=len(v))) for _ in range(n)); return xs[int(n * .025)], xs[int(n * .975)]
def diff_ci(a, b, n=2000):
    rnd = random.Random(6); xs = sorted(st.median(rnd.choices(a, k=len(a))) - st.median(rnd.choices(b, k=len(b))) for _ in range(n)); return xs[int(n * .025)], xs[int(n * .975)]

if __name__ == "__main__":
    random.seed(3); res = []; order = []
    for rnd in range(ROUNDS):
        arms = list(ARMS); random.shuffle(arms); order += [(a, rnd) for a in arms]
    out = "bench/results/" + time.strftime("%Y-%m-%d") + "-latency.json"
    for arm, rnd in order:
        r = run(arm, rnd); res.append(r); json.dump(res, open(out, "w"), indent=1)
        ok = [x for x in r["turns"] if x["answered"]]
        print(json.dumps({"arm": arm, "load": r["load"], "mb": r["mb"], "ctx": r["ctx_per_request"], "answered": f"{len(ok)}/{TURNS}", "median_total": round(st.median(x["total"] for x in ok), 2) if ok else None}), flush=True)
    print("\n%-14s %4s %6s  %-22s %-22s %-22s %-22s %8s %8s %7s" % ("arm", "n", "load", "pre s (95% ci)", "model s", "total s", "seen s", "ctx k", "MB", "paint"))
    base = {}
    for arm in ARMS:
        ts_ = [x for r in res if r["arm"] == arm for x in r["turns"] if x["answered"]]
        col = lambda k: [x[k] for x in ts_ if x.get(k) is not None]
        fmt = lambda k: "%.2f (%.2f-%.2f)" % ((st.median(col(k)),) + boot(col(k))) if col(k) else "-"
        base[arm] = {k: col(k) for k in ("pre", "model", "total", "seen")}
        rs = [r for r in res if r["arm"] == arm]
        print("%-14s %4d %6.1f  %-22s %-22s %-22s %-22s %8.1f %8.0f %7.3f" % (arm, len(ts_), st.mean(r["load"] for r in rs), fmt("pre"), fmt("model"), fmt("total"), fmt("seen"),
              st.median(r["ctx_per_request"] for r in rs) / 1000, st.median(r["mb"] for r in rs), st.median(r["first_paint_s"] or 0 for r in rs)))
    print("\ndifference of medians vs stock claude, s (95% bootstrap interval; negative = faster than claude):")
    for arm in ARMS:
        if arm == "claude": continue
        print("  %-14s" % arm + "  ".join("%s %+.2f [%+.2f, %+.2f]" % ((k,) + ((st.median(base[arm][k]) - st.median(base["claude"][k]),) + diff_ci(base[arm][k], base["claude"][k])) if base[arm][k] and base["claude"][k] else (k, 0, 0, 0)) for k in ("pre", "model", "total", "seen")))
