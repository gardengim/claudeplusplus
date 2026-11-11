#!/usr/bin/env python3
"""claude vs claude++ with several conversations at once (stdlib, macOS; calls the real API with haiku, cents per run).

Scenarios (3 conversations each):
  sep-same   3 separate processes, one shared project folder        (claude, claude++, claude++/opt)
  sep-diff   3 separate processes, 3 different project folders      (claude, claude++, claude++/opt)
  tabs-same  ONE claude++ process, 3 tabs, one shared folder        (claude++, claude++/opt)
  tabs-diff  ONE claude++ process, 3 tabs, /cd to 3 folders          (claude++, claude++/opt)
Arms: claude = stock `claude`; claude++ = your ~/.config/claude++/config.json (input_mode forced to default so the
driver can type); claude++/opt = the same plus the repo's .claude++/optimize.json copied into each project folder.
Every run: open -> idle (S0) -> first keystroke starts claude++'s backend, settle (S1) -> turn 1 sent to all three
at once (S2) -> idle 40 s, claude++ run with --idle-suspend 20 (S3) -> turn 2 sent to all three (S4) -> close.
Latency and tokens come from the session transcripts both frontends write, so both are measured the same way.
Memory = macOS `footprint` summed over every process tree. OpenViking memory capture is switched off for all arms
(OPENVIKING_MEMORY_ENABLED=0) so the bench prompts do not end up in the memory store.
Usage (from the repo root): python3 bench/multi.py [--reps N] [--only scen:arm,...] [--out file]"""
import sys, os, json, time, random, shutil, subprocess, select, statistics as st, datetime, glob, argparse
sys.path.insert(0, os.path.dirname(__file__))
from _term import Term, tree, footprint, names, CPP, ANSI

ap = argparse.ArgumentParser()
ap.add_argument("--reps", type=int, default=2)
ap.add_argument("--only", default="")
ap.add_argument("--out", default="bench/results/" + time.strftime("%Y-%m-%d") + "-multi.json")
ap.add_argument("--idle", type=int, default=40)
ap.add_argument("--model", default="haiku")
args = ap.parse_args()

ROOT = "/private/tmp/cppbench/w/multi"   # under the trusted bench folder
PROJ = os.path.expanduser("~/.claude/projects")
OPT = open(".claude++/optimize.json").read()
BASE_ENV = {"OPENVIKING_MEMORY_ENABLED": "0"}
ARMS = {
    "claude":       dict(argv=["claude", "--model", args.model], cpp=False, opt=False),
    "claude++":     dict(argv=[CPP, "--model", args.model, "--idle-suspend", "20"], cpp=True, opt=False),
    "claude++/opt": dict(argv=[CPP, "--model", args.model, "--idle-suspend", "20"], cpp=True, opt=True),
    # the control for sep-active: the same, with the supervisor off (each window then judges only for itself)
    "claude++/opt-nosup": dict(argv=[CPP, "--model", args.model, "--idle-suspend", "20"], cpp=True, opt=True, env={"CLAUDEPP_SUPERVISOR": "off"}),
}
SCENS = {"sep-same": ["claude", "claude++", "claude++/opt"], "sep-diff": ["claude", "claude++", "claude++/opt"],
         "tabs-same": ["claude++", "claude++/opt"], "tabs-diff": ["claude++", "claude++/opt"],
         # 3 separate windows in 3 folders; you keep typing only in the third, the other two sit idle (see run())
         "sep-active": ["claude++/opt", "claude++/opt-nosup"]}
ACTIVE_IDLE = 120   # sep-active idle phase, s; with --idle-suspend 40 the old per-window rule (5x = 200 s) cannot fire in it
T1 = [("6", "7", "42Z"), ("5", "9", "45Z"), ("8", "8", "64Z")]
P1 = [f"What is {a} times {b}? Reply with the number followed by the letter Z and nothing else." for a, b, _ in T1]
A1 = [x[2] for x in T1]
T2 = [("8", "50Q"), ("9", "54Q"), ("6", "70Q")]
P2 = [f"Now add {n} to your previous answer. Reply with the new number followed by the letter Q and nothing else." for n, _ in T2]
A2 = [x[1] for x in T2]

def encode(path): return "".join(c if c.isalnum() else "-" for c in path)

def mkproj(path, opt):
    """Fixed folders (trusted once, see trust()): only the optimize file changes between runs."""
    os.makedirs(path, exist_ok=True)
    if not os.path.exists(path + "/README.md"):
        open(path + "/README.md", "w").write("# " + os.path.basename(path) + "\nA tiny project for a benchmark.\n")
        subprocess.run(["git", "init", "-q"], cwd=path)
    f = path + "/.claude++/optimize.json"
    if opt: os.makedirs(path + "/.claude++", exist_ok=True); open(f, "w").write(OPT)
    elif os.path.exists(f): os.remove(f)

def trusted(path):
    try: return bool(json.load(open(os.path.expanduser("~/.claude.json"))).get("projects", {}).get(path, {}).get("hasTrustDialogAccepted"))
    except Exception: return False

def trust(path):
    """Stock claude asks once whether to trust a folder. These are folders this script made (a README, nothing else)."""
    if trusted(path): return
    t = Term(["claude", "--model", "haiku"], cwd=path, env_extra=BASE_ENV); t.alive = True; t.died = None; t.first = None; t.t0 = time.time()
    pump([t], 40, stop=lambda: b"trust" in bytes(t.buf).replace(b" ", b"").lower() or b"Tryasking" in bytes(t.buf).replace(b" ", b""))
    pump([t], 1); w(t, b"\x1b[B"); pump([t], 0.4); w(t, b"\r"); pump([t], 4)
    ok = trusted(path); t.close()
    print("trusted" if ok else "TRUST FAILED", path, flush=True)
    if not ok: sys.exit(1)

def cpu_s(pids):
    out = subprocess.run(["ps", "-o", "time=", "-p", ",".join(map(str, pids))], capture_output=True, text=True).stdout.split()
    tot = 0.0
    for t in out:
        s = 0.0
        for x in t.replace("-", ":").split(":"): s = s * 60 + float(x)
        tot += s
    return tot

def pump(terms, secs, stop=None):
    end = time.time() + secs
    while time.time() < end:
        fds = {t.fd: t for t in terms if t.alive}
        if not fds: time.sleep(0.05)
        else:
            r, _, _ = select.select(list(fds), [], [], 0.02)
            for fd in r:
                t = fds[fd]
                try: d = os.read(fd, 65536)
                except OSError: t.alive = False; continue
                if not d: continue
                if t.first is None: t.first = time.time() - t.t0
                t.buf.extend(d)
                if b"\x1b[6n" in d: os.write(fd, b"\x1b[1;1R")
                if b"\x1b[c" in d or b"\x1b[0c" in d: os.write(fd, b"\x1b[?62;c")
        if stop and stop(): return True
    return False

def snap(terms):
    pids = sorted({p for t in terms for p in tree(t.pid)})
    return {"mb": footprint(pids), "procs": len(pids), "names": names(pids), "cpu_s": round(cpu_s(pids), 2), "t": time.time(), "pids": pids}

def transcripts(dirs, since):
    """{file: [json lines]} for every transcript in the run's project dirs touched since `since`."""
    out = {}
    for d in dirs:
        for f in glob.glob(f"{PROJ}/{encode(d)}/*.jsonl"):
            if os.path.getmtime(f) < since - 1: continue
            ls = []
            for l in open(f, errors="replace"):
                try: ls.append(json.loads(l))
                except ValueError: pass
            out[f] = ls
    return out

def find_answer(tr, answer):
    for ls in tr.values():
        for v in ls:
            m = v.get("message") or {}
            if v.get("type") == "assistant" and isinstance(m.get("content"), list):
                if any(b.get("type") == "text" and answer in (b.get("text") or "") for b in m["content"]): return v
    return None

def find_prompt(tr, prompt):
    for ls in tr.values():
        for v in ls:
            if v.get("type") == "user" and prompt in json.dumps(v.get("message") or {}): return v
    return None

def ts(v): return datetime.datetime.fromisoformat(v["timestamp"].replace("Z", "+00:00")).timestamp()

def w(t, b):
    try: os.write(t.fd, b)
    except OSError: t.alive = False; t.died = time.time()

def key(t, s): w(t, s.encode())

def turn(terms, order, dirs, prompts, answers, switch, since):
    """Send each prompt (to its terminal / tab), then wait until every answer is in a transcript. -> per-session timings."""
    sent = {}
    for i in order:
        t = terms[i if len(terms) > 1 else 0]
        if switch: w(t, f"\x1b{i + 1}".encode()); pump(terms, 0.4)
        key(t, prompts[i]); pump(terms, 0.5)
        w(t, b"\r"); sent[i] = time.time(); pump(terms, 0.05)
    got = {}
    def done():
        tr = transcripts(dirs, since)
        for i in order:
            if i not in got and (v := find_answer(tr, answers[i])): got[i] = (time.time(), ts(v))
        return len(got) == len(order)
    pump(terms, 150, stop=lambda: (time.sleep(0.1), done())[1])
    tr = transcripts(dirs, since)
    res = []
    for i in order:
        r = {"session": i, "answered": i in got}
        if i in got:
            r["latency_s"] = round(got[i][0] - sent[i], 2)
            u = find_prompt(tr, prompts[i])
            if u: r["pre_model_s"] = round(ts(u) - sent[i], 2); r["model_s"] = round(got[i][1] - ts(u), 2)
        res.append(r)
    wall = round(max(g[0] for g in got.values()) - min(sent.values()), 2) if len(got) == len(order) else None
    return res, wall

def usage(dirs, since):
    msgs = {}
    for ls in transcripts(dirs, since).values():
        for v in ls:
            m = v.get("message") or {}
            if v.get("type") == "assistant" and m.get("usage"): msgs[m.get("id")] = m["usage"]
    g = lambda k: sum(u.get(k) or 0 for u in msgs.values())
    return {"requests": len(msgs), "input": g("input_tokens"), "cache_write": g("cache_creation_input_tokens"),
            "cache_read": g("cache_read_input_tokens"), "output": g("output_tokens")}

def record(dirs, since):
    """Interactive claude's own totals as it exited (side calls included); only meaningful with distinct folders."""
    try: p = json.load(open(os.path.expanduser("~/.claude.json"))).get("projects", {})
    except Exception: return None
    tot = {"input": 0, "cache_write": 0, "cache_read": 0, "output": 0, "cost_usd": 0.0, "n": 0}
    for d in dirs:
        x = p.get(d, {})
        if x.get("lastStartTime", 0) / 1000 < since - 1: continue
        tot["n"] += 1; tot["cost_usd"] += x.get("lastCost", 0)
        for k, f in (("input", "lastTotalInputTokens"), ("cache_write", "lastTotalCacheCreationInputTokens"), ("cache_read", "lastTotalCacheReadInputTokens"), ("output", "lastTotalOutputTokens")): tot[k] += x.get(f) or 0
    return tot if tot["n"] else None

def run(scen, arm, rep):
    a = ARMS[arm]; tag = f"{scen}-{arm.replace('+', 'p').replace('/', '_')}-{rep}"
    base = f"{ROOT}/_runs/{tag}"; shutil.rmtree(base, ignore_errors=True); os.makedirs(base)
    same = scen.endswith("same"); tabs = scen.startswith("tabs")
    pd = f"{ROOT}/{arm.replace('+', 'p').replace('/', '_')}"
    dirs = [f"{pd}/same"] * 3 if same else [f"{pd}/{c}" for c in "abc"]
    for d in sorted(set(dirs)): mkproj(d, a["opt"]); trust(d)
    cfg = f"{base}/xdg"
    env = dict(BASE_ENV, **a.get("env", {}))
    argv = [("40" if scen == "sep-active" and x == "20" else x) for x in a["argv"]]
    if a["cpp"]:
        env["CLAUDEPP_SUP_DIR"] = base + "/sup"   # its own supervisor: never the one your real windows use
        os.makedirs(cfg + "/claude++")
        c = json.load(open(os.path.expanduser("~/.config/claude++/config.json"))); c["input_mode"] = "default"
        json.dump(c, open(cfg + "/claude++/config.json", "w")); env["XDG_CONFIG_HOME"] = cfg
    r = {"scenario": scen, "arm": arm, "rep": rep, "load": round(os.getloadavg()[0], 1)}
    print("run", tag, "load", r["load"], flush=True)
    since = time.time()
    n = 1 if tabs else 3
    terms = []
    for i in range(n):
        t = Term(argv, cwd=dirs[0] if tabs else dirs[i], env_extra=env); t.alive = True; t.died = None; t.first = None; t.t0 = time.time(); terms.append(t)
    pump(terms, 15)
    r["first_paint_s"] = round(sum(t.first or 0 for t in terms) / n, 3)
    r["S0"] = snap(terms)
    # prime: a keystroke starts claude++'s backend (stock claude already started its own); tabs: open and prime each
    for i in range(3 if tabs else n):
        t = terms[0] if tabs else terms[i]
        if tabs and i:
            os.write(t.fd, b"\x14"); pump(terms, 1)
            if not same:
                key(t, f"/cd {dirs[i]}"); pump(terms, 0.5); os.write(t.fd, b"\r"); pump(terms, 1.5)
        os.write(t.fd, b"x"); pump(terms, 0.3); os.write(t.fd, b"\x7f"); pump(terms, 0.3)
    pump(terms, 12)
    r["S1"] = snap(terms)
    order = [0, 1, 2]
    r["turn1"], r["turn1_wall_s"] = turn(terms, order, sorted(set(dirs)), P1, A1, tabs, since)
    pump(terms, 3); r["S2"] = snap(terms)
    if scen == "sep-active":
        for k in range(ACTIVE_IDLE // 10):   # you work in window 3 only: a key every 10 s
            pump(terms, 10)
            w(terms[2], b"x"); pump(terms, 0.2); w(terms[2], b"\x7f")
            if k == ACTIVE_IDLE // 20: r["S3mid"] = snap(terms)
        r["S3"] = snap(terms)
    else:
        pump(terms, args.idle); r["S3"] = snap(terms)
    t2 = time.time()
    r["turn2"], r["turn2_wall_s"] = turn(terms, order, sorted(set(dirs)), P2, A2, tabs, since)
    pump(terms, 3); r["S4"] = snap(terms)
    r["screen_tail"] = [ANSI.sub(b"", bytes(t.buf[-3000:])).decode(errors="replace")[-400:] for t in terms] if not all(x["answered"] for x in r["turn1"] + r["turn2"]) else None
    r["died"] = [round(t.died - since, 1) if t.died else None for t in terms]
    for i, t in enumerate(terms): open(f"{base}/term{i}.raw", "wb").write(bytes(t.buf))
    allp = r["S4"]["pids"]
    for t in terms: t.close()
    time.sleep(3)
    def alive(p):
        try: os.kill(p, 0); return True
        except OSError: return False
    r["leaked_procs"] = sum(alive(p) for p in allp)
    r["usage"] = usage(sorted(set(dirs)), since)
    r["claude_record"] = record(sorted(set(dirs)), since) if not a["cpp"] and not same else None
    for k in ("S0", "S1", "S2", "S3", "S3mid", "S4"):
        if k in r: r[k].pop("pids", None)
    return r

if __name__ == "__main__":
    want = {tuple(x.split(":")) for x in args.only.split(",") if x}
    cfgs = [(s, a, i) for i in range(args.reps) for s, arms in SCENS.items() for a in arms if not want or (s, a) in want]
    random.seed(11); random.shuffle(cfgs)
    os.makedirs(os.path.dirname(args.out), exist_ok=True)
    res = []
    for s, a, i in cfgs:
        r = run(s, a, i); res.append(r)
        json.dump(res, open(args.out, "w"), indent=1)
        print(json.dumps({k: r[k] for k in ("scenario", "arm", "first_paint_s", "turn1_wall_s", "turn2_wall_s", "leaked_procs")} | {"mb": [r[k]["mb"] for k in ("S0", "S1", "S2", "S3", "S4")]}), flush=True)
    print("saved", args.out)
