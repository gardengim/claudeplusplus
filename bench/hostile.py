#!/usr/bin/env python3
"""claude++ against a hostile world, with the real `claude` (haiku, a few cents, ~3 min): signals, a backend killed
mid-turn, interrupt, a permission prompt. Each scenario must leave claude++ alive and usable (or exited cleanly),
and no `claude` process behind. Usage: python3 bench/hostile.py [scenario ...]"""
import os, sys, tempfile
from _term import *
want = sys.argv[1:]; sys.argv = sys.argv[:1]
USER_CFG = os.environ.get("HOSTILE_CONFIG") == "user"  # default: claude++ default settings; "user": your ~/.config/claude++ (e.g. vim mode)
os.makedirs(W, exist_ok=True)
if not USER_CFG: os.environ["XDG_CONFIG_HOME"] = tempfile.mkdtemp(prefix="cpp-hostile-cfg-")
Q = "What is 6 times 7? Reply with the number followed by the letter Z and nothing else."
LONG = "Write a 400 word story about a lighthouse keeper. Do not use any tools."

def screen(t): return ANSI.sub(b"", bytes(t.buf)).decode("utf8", "replace")
def kids(pid): return [int(x) for x in subprocess.run(["pgrep", "-P", str(pid)], capture_output=True, text=True).stdout.split()]
def alive(pid):
    try: os.kill(pid, 0); return True
    except OSError: return False
def gone(pids, secs=6):
    end = time.time() + secs
    while time.time() < end:
        if not any(alive(p) for p in pids): return True
        time.sleep(0.2)
    return False
def start():
    t = Term([CPP, "--model", "haiku"]); t.mark = 0; t.drain(2); return t
def reaped(t):
    try: return os.waitpid(t.pid, os.WNOHANG)[0] != 0
    except ChildProcessError: return True

def sc_signal(sig, name):
    def run():
        t = start()
        if not t.ask(Q, b"Worked for"): return False, "no answer first"
        backend = tree(t.pid)[1:]
        os.kill(t.pid, sig); t.drain(3)
        ex = False
        for _ in range(30):
            if reaped(t): ex = True; break
            time.sleep(0.2)
        left = [p for p in backend if alive(p)] if gone(backend, 8) is False else []
        restored = b"\x1b[?1049l" in t.buf or sig == signal.SIGKILL  # SIGKILL cannot restore anything
        return ex and not left and restored, f"exited={ex} orphans={left} terminal_restored={restored}"
    return run

def sc_backend_killed():
    t = start()
    t.mark = 0; os.write(t.fd, LONG.encode()); t.drain(0.5); os.write(t.fd, b"\r"); t.drain(4)
    b = [p for p in tree(t.pid)[1:] if "claude" in subprocess.run(["ps", "-o", "comm=", "-p", str(p)], capture_output=True, text=True).stdout]
    for p in b[:1]: os.kill(p, signal.SIGKILL)
    t.drain(3)
    up = not reaped(t)
    t.mark = len(t.buf)
    ok2 = t.ask(Q, b"Worked for") if up else False
    t.close()
    return up and ok2, f"claude++ survived={up}, next message answered={ok2}"

def sc_interrupt():
    t = start()
    t.mark = 0; os.write(t.fd, b"Write an essay of at least 3000 words on the history of lighthouses. Plain text only, answer directly."); t.drain(0.5); os.write(t.fd, b"\r"); t.drain(5)
    os.write(t.fd, b"\x1b"); t0 = time.time()
    t.mark = len(t.buf)
    ok2 = t.ask(Q, b"Worked for")
    dt = time.time() - t0
    t.close()
    # an uninterrupted essay takes ~50 s before the queued follow-up even starts (measured); an interrupt answers in seconds
    marked = "interrupted by user" in screen(t)
    return ok2 and marked and dt < 20, f"interrupt marker shown={marked}, follow-up answered={ok2} in {dt:.1f}s after Esc" + ("" if ok2 else " | screen tail: " + screen(t)[-500:].replace("\n", " / "))

def sc_permission():
    """Needs the model to attempt the tool call; it sometimes declines to, so try up to 3 fresh sessions."""
    for attempt in range(3):
        t = Term([CPP, "--model", "haiku", "--permission-mode", "default"]); t.mark = 0; t.drain(2)
        t.mark = len(t.buf); os.write(t.fd, b"Run this exact shell command with the Bash tool: touch /private/tmp/cppbench/perm-probe.txt"); t.drain(0.5); os.write(t.fd, b"\r")
        prompt = t.drain(60, b"don't ask again") or t.drain(1, b"Yes")  # outside the working folder, so it asks
        if prompt: os.write(t.fd, b"\x1b"); t.drain(3)  # decline with esc
        up = not reaped(t)
        t.mark = len(t.buf); ok2 = t.ask(Q, b"Worked for") if up else False
        tail = screen(t)[-500:].replace("\n", " / ")
        t.close()
        if prompt:
            denied = not os.path.exists("/private/tmp/cppbench/perm-probe.txt")
            return up and ok2 and denied, f"prompt shown, survived decline={up}, next answered={ok2}, declined command did not run={denied} (attempt {attempt + 1})"
    return False, "the model never attempted the tool call in 3 sessions | last screen: " + tail

def sc_editor_orphans(sig):
    """An editor open in the box must not outlive claude++, however it dies."""
    def run():
        import json
        cfg = tempfile.mkdtemp(prefix="cpp-hostile-ed-"); os.makedirs(cfg + "/claude++")
        json.dump({"editor": "sleep 300 #", "input_mode": "editor"}, open(cfg + "/claude++/config.json", "w"))
        old = os.environ["XDG_CONFIG_HOME"]; os.environ["XDG_CONFIG_HOME"] = cfg
        try: t = Term([CPP, "--model", "haiku"])
        finally: os.environ["XDG_CONFIG_HOME"] = old
        t.mark = 0; t.drain(1.5); os.write(t.fd, b"x"); t.drain(2.0)
        editor = [p for p in tree(t.pid)[1:] if "sleep" in subprocess.run(["ps", "-o", "command=", "-p", str(p)], capture_output=True, text=True).stdout]
        if not editor: return False, "the editor never started"
        os.kill(t.pid, sig); t.drain(2)
        return gone(editor, 8), f"editor processes {editor} gone after the signal: {gone(editor, 0.1)}"
    return run

SC = {"editor-term": sc_editor_orphans(signal.SIGTERM), "editor-hup": sc_editor_orphans(signal.SIGHUP), "editor-kill": sc_editor_orphans(signal.SIGKILL), "sigterm": sc_signal(signal.SIGTERM, "SIGTERM"), "sighup": sc_signal(signal.SIGHUP, "SIGHUP"), "sigkill": sc_signal(signal.SIGKILL, "SIGKILL"),
      "killed": sc_backend_killed, "interrupt": sc_interrupt, "permission": sc_permission}
bad = 0
for name, fn in SC.items():
    if want and name not in want: continue
    try: ok, d = fn()
    except Exception as e: ok, d = False, f"crashed: {e!r}"
    bad += not ok; print(f"{'PASS' if ok else 'FAIL'}  {name}: {d}", flush=True)
left = subprocess.run(["pgrep", "-f", "claude.*stream-json.*haiku"], capture_output=True, text=True).stdout.split()
print(f"stray claude processes at the end: {left or 'none'}")
sys.exit(1 if bad or left else 0)
