#!/usr/bin/env python3
"""claude vs claude++ in a real pty (stdlib, macOS). One haiku turn per run.
Controls: cwd is a fresh empty dir under /private/tmp (no ancestor AGENTS.md/CLAUDE.md), claude++ gets empty
XDG config/cache dirs (default settings), same model, same backend flags per pairing, runs interleaved + shuffled,
memory/CPU read per PID tree (never system-wide, never other sessions), wait for load < LOAD before each run.
Usage: python3 bench/vs_claude.py [runs] """
import glob, os, pty, time, subprocess, select, struct, fcntl, termios, signal, sys, random, json, re, shutil, statistics as st
from _term import footprint
RUNS = int(sys.argv[1]) if len(sys.argv) > 1 else 3
LOAD = 14.0  # machine is shared and busy (system daemons); load is recorded per run instead of waited out
CPP = os.path.abspath("target/release/claudeplusplus")
LEAN = ["--tools", "Bash,Read,Edit,Write,AskUserQuestion,WebFetch,WebSearch", "--setting-sources", "project,local", "--strict-mcp-config"]
ARMS = {
    "claude/full":   ["claude", "--model", "haiku"],
    "claude++/full": [CPP, "--profile", "full", "--model", "haiku"],
    "claude/lean":   ["claude", "--model", "haiku"] + LEAN,
    "claude++/lean": [CPP, "--profile", "lean", "--model", "haiku"],
}
PROMPT = "What is 6 times 7? Reply with the number followed by the letter Z and nothing else."
ANSWER = b"42Z"
ANSI = re.compile(rb"\x1b\[[0-9;?<>=]*[ -/]*[@-~]|\x1b\][^\x07\x1b]*(\x07|\x1b\\)|\x1b[()][A-Z0-9]|\x1b[=>]")

def tree(root):
    out = subprocess.run(["ps", "-A", "-o", "pid=,ppid=,rss=,time="], capture_output=True, text=True).stdout
    rows = {}; kids = {}
    for l in out.splitlines():
        p = l.split()
        if len(p) < 4: continue
        pid, pp, rss = int(p[0]), int(p[1]), int(p[2]); rows[pid] = (rss, p[3]); kids.setdefault(pp, []).append(pid)
    seen, st_ = [], [root]
    while st_:
        x = st_.pop()
        if x in rows: seen.append(x); st_ += kids.get(x, [])
    def secs(t):
        parts = t.replace("-", ":").split(":"); parts = [float(x) for x in parts]
        s = 0.0
        for x in parts: s = s * 60 + x
        return s
    return seen, sum(rows[x][0] for x in seen) / 1024, sum(secs(rows[x][1]) for x in seen)


def transcript_usage(since):
    """Tokens of every model request in the session transcripts written since `since` (the same source for both
    frontends): one entry per API message id, so streamed copies of a message count once."""
    msgs = {}
    for f in glob.glob(os.path.expanduser("~/.claude/projects/-private-tmp-cppbench-w/*.jsonl")):
        if os.path.getmtime(f) < since: continue
        for l in open(f):
            v = json.loads(l)
            m = v.get("message") or {}
            if v.get("type") == "assistant" and m.get("usage"): msgs[m.get("id")] = (m.get("model"), m["usage"])
    if not msgs: return None
    g = lambda k: sum(u.get(k) or 0 for _, u in msgs.values())
    return {"requests": len(msgs), "input": g("input_tokens"), "cache_write": g("cache_creation_input_tokens"), "cache_read": g("cache_read_input_tokens"),
            "output": g("output_tokens"), "models": sorted({m for m, _ in msgs.values()})}

def usage(since):
    """What Claude Code itself recorded for the session in the bench folder (written when it exits): tokens by kind,
    every model it called (side calls included) and its own cost figure. None if no session ended after `since`."""
    p = json.load(open(os.path.expanduser("~/.claude.json"))).get("projects", {}).get("/private/tmp/cppbench/w", {})
    if p.get("lastStartTime", 0) / 1000 < since - 1: return None
    return {"cost_usd": round(p.get("lastCost", 0), 5), "input": p.get("lastTotalInputTokens"), "cache_write": p.get("lastTotalCacheCreationInputTokens"),
            "cache_read": p.get("lastTotalCacheReadInputTokens"), "output": p.get("lastTotalOutputTokens"), "models": sorted(p.get("lastModelUsage", {}))}

def run(arm, argv, i):
    cwd = f"/private/tmp/cppbench/{arm.replace('/', '_')}-{i}"
    shutil.rmtree(cwd, ignore_errors=True); os.makedirs(cwd + "/xdg/c"); os.makedirs(cwd + "/xdg/k")
    W = "/private/tmp/cppbench/w"  # one fixed workdir: official claude asks to trust each new folder, so trust it once (see README)
    shutil.rmtree(W, ignore_errors=True); os.makedirs(W)
    # A plain terminal's environment: none of the variables a parent Claude Code session sets (session id,
    # child-session flag, effort...), which change how either frontend's claude behaves.
    env = {k: v for k, v in os.environ.items() if not k.startswith("CLAUDE")}
    env.update(TERM="xterm-256color", XDG_CONFIG_HOME=cwd + "/xdg/c", XDG_CACHE_HOME=cwd + "/xdg/k")
    while float(os.getloadavg()[0]) > LOAD: time.sleep(5)
    load = os.getloadavg()[0]
    pid, fd = pty.fork()
    if pid == 0:
        os.chdir(W); os.execvpe(argv[0], argv, env)
    fcntl.ioctl(fd, termios.TIOCSWINSZ, struct.pack("HHHH", 50, 160, 0, 0))
    buf = bytearray(); t0 = time.time(); first = [None]
    def drain(t, stop=None):
        end = time.time() + t
        while time.time() < end:
            r, _, _ = select.select([fd], [], [], 0.01)
            if r:
                try: d = os.read(fd, 65536)
                except OSError: return False
                if d:
                    if first[0] is None: first[0] = time.time() - t0
                    buf.extend(d)
                    # answer terminal queries so TUIs don't stall (cursor pos, device attrs)
                    if b"\x1b[6n" in d: os.write(fd, b"\x1b[1;1R")
                    if b"\x1b[c" in d or b"\x1b[0c" in d: os.write(fd, b"\x1b[?62;c")
            if stop and stop(): return True
        return False
    r = {"arm": arm, "load": round(load, 2)}
    print('start', arm, i, file=sys.stderr, flush=True)
    print('settle', file=sys.stderr, flush=True); drain(12)  # settle (backend warm-up included for both)
    r["first_paint_s"] = round(first[0], 3) if first[0] else None
    ps, r["rss_idle_mb"], c0 = tree(pid); r["procs"] = len(ps); r["mb_idle"] = footprint(ps)
    print('idle', file=sys.stderr, flush=True); drain(15)
    _, _, c1 = tree(pid); r["idle_cpu_s_per_15s"] = round(c1 - c0, 2)
    n0 = len(buf); t = time.time(); os.write(fd, b"x")
    drain(2, stop=lambda: len(buf) > n0)
    r["key_echo_ms"] = round((time.time() - t) * 1000, 1) if len(buf) > n0 else None
    os.write(fd, b"\x7f")
    drain(12)  # claude++ starts its backend on that keystroke: let it (and its MCP servers) finish starting, as stock claude did while settling
    print('turn', file=sys.stderr, flush=True); n1 = len(buf); os.write(fd, PROMPT.encode()); drain(0.5); t = time.time(); os.write(fd, b"\r")
    # claude++ redraws only changed cells, so "42" and "Z" can arrive as separate writes: its end-of-turn line
    # ("Worked for 1s"), drawn once the reply is on screen, marks the same moment.
    done = (lambda t: b"Worked for" in t) if "++" in arm else (lambda t: ANSWER in t.split(b"\n", 1)[-1])
    ok = drain(60, stop=lambda: done(ANSI.sub(b"", bytes(buf[n1:]))))
    r["turn_s"] = round(time.time() - t, 2) if ok else None
    drain(3)
    ps, r["rss_after_turn_mb"], _ = tree(pid); r["mb_after_turn"] = footprint(ps)
    r["bytes_total"] = len(buf)
    open(cwd + "/screen.raw", "wb").write(buf)  # what the terminal received, for checking a missed answer
    os.kill(pid, signal.SIGTERM); drain(1.5)  # keep reading so the exiting child can flush its terminal restore
    try: os.close(fd)
    except OSError: pass
    for _ in range(30):
        try:
            if os.waitpid(pid, os.WNOHANG)[0]: break
        except ChildProcessError: break
        time.sleep(0.1)
    else:
        os.kill(pid, signal.SIGKILL); os.waitpid(pid, 0)
    time.sleep(1)
    r["usage"] = transcript_usage(t0)
    # Interactive claude also records its own totals as it exits, side calls included (`claude -p` does not).
    r["claude_record"] = usage(t0)
    return r

random.seed(7)
order = [(a, i) for i in range(RUNS) for a in ARMS]; random.shuffle(order)
res = []
for a, i in order:
    r = run(a, ARMS[a], i); res.append(r); print(json.dumps(r), flush=True)
out = "bench/results/" + time.strftime("%Y-%m-%d") + "-vs-claude.json"
json.dump(res, open(out, "w"), indent=1)
print("\n%-14s %3s %8s %8s %8s %7s %7s %8s %9s %9s %8s %5s" % ("arm", "n", "paint s", "idle MB", "cpu/15s", "key ms", "turn s", "turn MB", "context", "output", "rec. $", "procs"))
for a in ARMS:
    rs = [x for x in res if x["arm"] == a]
    m = lambda f: (lambda v: round(st.median(v), 5) if v else None)([y for y in map(f, rs) if y is not None])
    u = lambda k: lambda x: x["usage"] and x["usage"][k]
    ctx = lambda x: x["usage"] and x["usage"]["input"] + x["usage"]["cache_write"] + x["usage"]["cache_read"]
    print("%-14s %3d %8s %8s %8s %7s %7s %8s %9s %9s %8s %5s" % (a, len(rs), m(lambda x: x["first_paint_s"]), m(lambda x: x["mb_idle"]), m(lambda x: x["idle_cpu_s_per_15s"]),
          m(lambda x: x["key_echo_ms"]), m(lambda x: x["turn_s"]), m(lambda x: x["mb_after_turn"]), m(ctx), m(u("output")), m(lambda x: x["claude_record"] and x["claude_record"]["cost_usd"]), m(lambda x: x["procs"])))
print("memory: macOS footprint summed over the process tree; context = input + cache write + cache read tokens over the session; tokens from the session transcripts;\nrec. $ = interactive claude's own cost record (all its model calls; claude -p keeps none)")
