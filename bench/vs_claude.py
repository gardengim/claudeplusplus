#!/usr/bin/env python3
"""claude vs claude++ in a real pty (stdlib, macOS). One haiku turn per run.
Controls: cwd is a fresh empty dir under /private/tmp (no ancestor AGENTS.md/CLAUDE.md), claude++ gets empty
XDG config/cache dirs (default settings), same model, same backend flags per pairing, runs interleaved + shuffled,
memory/CPU read per PID tree (never system-wide, never other sessions), wait for load < LOAD before each run.
Usage: python3 bench/vs_claude.py [runs] """
import os, pty, time, subprocess, select, struct, fcntl, termios, signal, sys, random, json, re, shutil, statistics as st
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

def run(arm, argv, i):
    cwd = f"/private/tmp/cppbench/{arm.replace('/', '_')}-{i}"
    shutil.rmtree(cwd, ignore_errors=True); os.makedirs(cwd + "/xdg/c"); os.makedirs(cwd + "/xdg/k")
    W = "/private/tmp/cppbench/w"  # one fixed workdir: official claude asks to trust each new folder, so trust it once (see README)
    shutil.rmtree(W, ignore_errors=True); os.makedirs(W)
    env = dict(os.environ, TERM="xterm-256color", XDG_CONFIG_HOME=cwd + "/xdg/c", XDG_CACHE_HOME=cwd + "/xdg/k")
    env.pop("CLAUDECODE", None)
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
    _, r["rss_idle_mb"], c0 = tree(pid); r["procs"] = len(tree(pid)[0])
    print('idle', file=sys.stderr, flush=True); drain(15)
    _, _, c1 = tree(pid); r["idle_cpu_s_per_15s"] = round(c1 - c0, 2)
    n0 = len(buf); t = time.time(); os.write(fd, b"x")
    drain(2, stop=lambda: len(buf) > n0)
    r["key_echo_ms"] = round((time.time() - t) * 1000, 1) if len(buf) > n0 else None
    os.write(fd, b"\x7f"); drain(0.5)
    print('turn', file=sys.stderr, flush=True); n1 = len(buf); os.write(fd, PROMPT.encode()); drain(0.5); t = time.time(); os.write(fd, b"\r")
    ok = drain(60, stop=lambda: ANSWER in ANSI.sub(b"", bytes(buf[n1:])).split(b"\n", 1)[-1] and bytes(ANSI.sub(b"", bytes(buf[n1:]))).count(ANSWER) >= 1)
    r["turn_s"] = round(time.time() - t, 2) if ok else None
    drain(3)
    _, r["rss_after_turn_mb"], _ = tree(pid)
    r["bytes_total"] = len(buf)
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
    return r

random.seed(7)
order = [(a, i) for i in range(RUNS) for a in ARMS]; random.shuffle(order)
res = []
for a, i in order:
    r = run(a, ARMS[a], i); res.append(r); print(json.dumps(r), flush=True)
out = "bench/results/" + time.strftime("%Y-%m-%d") + "-vs-claude.json"
json.dump(res, open(out, "w"), indent=1)
print("\n%-15s %6s %9s %9s %9s %8s %8s %8s %8s" % ("arm", "n", "paint s", "idle MB", "cpu/15s", "key ms", "turn s", "post MB", "procs"))
for a in ARMS:
    rs = [x for x in res if x["arm"] == a]
    m = lambda k: (lambda v: round(st.median(v), 2) if v else None)([x[k] for x in rs if x.get(k) is not None])
    print("%-15s %6d %9s %9s %9s %8s %8s %8s %8s" % (a, len(rs), m("first_paint_s"), m("rss_idle_mb"), m("idle_cpu_s_per_15s"), m("key_echo_ms"), m("turn_s"), m("rss_after_turn_mb"), m("procs")))
