"""Shared by the pty benches: a terminal to drive claude++ in, and process-tree helpers (stdlib, macOS)."""
import os, pty, time, subprocess, select, struct, fcntl, termios, signal, sys, json, re, shutil, glob
CPP = os.path.abspath("target/release/claudeplusplus")
W = "/private/tmp/cppbench/w"  # the bench folder vs_claude.py uses: claude asks to trust a new folder, and this one is trusted
ANSI = re.compile(rb"\x1b\[[0-9;?<>=]*[ -/]*[@-~]|\x1b\][^\x07\x1b]*(\x07|\x1b\\)|\x1b[()][A-Z0-9]|\x1b[=>]")

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
