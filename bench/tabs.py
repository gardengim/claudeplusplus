#!/usr/bin/env python3
"""Three conversations left open: three `claude` windows vs three claude++ tabs (stdlib, macOS, haiku).
Each conversation gets one turn, then everything sits idle for IDLE seconds and the process trees are measured
with macOS `footprint`. claude++ runs with `--idle-suspend 20`, so its two background tabs stop their backend.
Then tab 1 (suspended) gets a question that needs the earlier turn, to show the context survived, and the
resumed request's prompt-cache use is read from the session transcript.
Usage: python3 bench/tabs.py [IDLE seconds, default 45]"""
import os, pty, time, subprocess, select, struct, fcntl, termios, signal, sys, json, re, shutil, glob
IDLE = int(sys.argv[1]) if len(sys.argv) > 1 else 45
CPP = os.path.abspath("target/release/claudeplusplus")
W = "/private/tmp/cppbench/w"  # the bench folder vs_claude.py uses: claude asks to trust a new folder, and this one is trusted
ANSI = re.compile(rb"\x1b\[[0-9;?<>=]*[ -/]*[@-~]|\x1b\][^\x07\x1b]*(\x07|\x1b\\)|\x1b[()][A-Z0-9]|\x1b[=>]")
PROMPTS = [f"What is {a} times {b}? Reply with the number followed by the letter Z and nothing else." for a, b in [(6, 7), (5, 9), (8, 8)]]
ANSWERS = [b"42Z", b"45Z", b"64Z"]

def tree(root):
    kids = {}
    for l in subprocess.run(["ps", "-A", "-o", "pid=,ppid="], capture_output=True, text=True).stdout.splitlines():
        p, pp = map(int, l.split()); kids.setdefault(pp, []).append(p)
    out, st = [], [root]
    while st:
        x = st.pop(); out.append(x); st += kids.get(x, [])
    return out

def footprint(pids):
    out = subprocess.run(["footprint"] + [a for p in pids for a in ("-p", str(p))], capture_output=True, text=True).stdout
    mul = {"KB": 1 / 1024, "MB": 1, "GB": 1024}
    return round(sum(float(n) * mul[u] for n, u in re.findall(r"\]: .*?Footprint: ([\d.]+) (KB|MB|GB)", out)), 1)

def names(pids):
    """How many processes of each program, e.g. {"claude": 3, "node": 2}."""
    out = subprocess.run(["ps", "-o", "comm=", "-p", ",".join(map(str, pids))], capture_output=True, text=True).stdout.split("\n")
    c = {}
    for n in filter(None, (os.path.basename(x.strip()) for x in out)): c[n] = c.get(n, 0) + 1
    return c

class Term:
    def __init__(self, argv):
        env = {k: v for k, v in os.environ.items() if not k.startswith("CLAUDE")}; env["TERM"] = "xterm-256color"
        self.pid, self.fd = pty.fork()
        if self.pid == 0:
            os.chdir(W); os.execvpe(argv[0], argv, env)
        fcntl.ioctl(self.fd, termios.TIOCSWINSZ, struct.pack("HHHH", 50, 160, 0, 0))
        self.buf = bytearray()
    def drain(self, t, want=None):
        end = time.time() + t
        while time.time() < end:
            r, _, _ = select.select([self.fd], [], [], 0.02)
            if r:
                try: d = os.read(self.fd, 65536)
                except OSError: return False
                self.buf.extend(d)
                if b"\x1b[6n" in d: os.write(self.fd, b"\x1b[1;1R")
                if b"\x1b[c" in d or b"\x1b[0c" in d: os.write(self.fd, b"\x1b[?62;c")
            if want and want in ANSI.sub(b"", bytes(self.buf[self.mark:])): return True
        return False
    def ask(self, text, want):
        """Send a message; True once `want` is on screen. claude++ redraws only changed cells, so for it pass its
        end-of-turn line ("Worked for"), which is drawn once the reply is."""
        self.mark = len(self.buf)
        os.write(self.fd, text.encode()); self.drain(0.5); os.write(self.fd, b"\r")
        # the prompt itself contains "Z" but not the answer, so seeing the answer means the reply arrived
        return self.drain(90, want)
    def close(self):
        os.kill(self.pid, signal.SIGTERM); self.drain(2)
        for _ in range(50):
            try:
                if os.waitpid(self.pid, os.WNOHANG)[0]: return
            except ChildProcessError: return
            time.sleep(0.1)
        os.kill(self.pid, signal.SIGKILL)

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
