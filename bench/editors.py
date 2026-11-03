#!/usr/bin/env python3
"""Real editors inside claude++ (no model calls): for each of vim, nvim and nano, the whole flow through a pty:
  A  input mode "editor": type, write the message, quit -> exactly that message reaches claude
  B  ctrl+g full screen: edit the draft, quit -> the text is back in the box, then Enter sends it
  C  ctrl+\\ cancels: nothing is sent, claude++ lives, the next message goes through
A fake `claude` logs every user message it receives. Usage: python3 bench/editors.py [editor ...]"""
import os, sys, json, tempfile
want = sys.argv[1:]; sys.argv = sys.argv[:1]
_src = open(__file__.replace("editors.py", "tabs.py")).read().split("os.makedirs(W")[0]  # reuse Term/tree helpers
exec(_src)
os.makedirs(W, exist_ok=True)

FAKE = r'''#!/usr/bin/env python3
import sys, json, os
def out(o): sys.stdout.write(json.dumps(o) + "\n"); sys.stdout.flush()
for line in sys.stdin:
    m = json.loads(line)
    if m.get("type") == "control_request":
        sub = m["request"]["subtype"]
        out({"type": "control_response", "response": {"subtype": "success", "request_id": m["request_id"], "response": {"mcpServers": []} if sub == "mcp_status" else {}}})
    elif m.get("type") == "user":
        c = m["message"]["content"]
        open(os.environ["EDITORS_LOG"], "a").write(json.dumps(c if isinstance(c, str) else c) + "\n")
        out({"type": "system", "subtype": "init", "session_id": "ed-1", "model": "fake", "tools": [], "claude_code_version": "2.1.317"})
        out({"type": "result", "subtype": "success", "is_error": False, "result": "ok", "usage": {"input_tokens": 1, "output_tokens": 1}, "total_cost_usd": 0, "duration_api_ms": 1})
'''
base = tempfile.mkdtemp(prefix="cpp-editors-")
open(base + "/claude", "w").write(FAKE); os.chmod(base + "/claude", 0o755)
os.environ["PATH"] = base + ":" + os.environ["PATH"]

# how to write a message and quit, per editor: (command, keys that type `text` and then save+quit, keys that append ` more` to a loaded draft and save+quit)
EDITORS = {
    "vim":  ("vim -u NONE -N", lambda t: f"i{t}\x1b:wq\r", lambda t: f"A{t}\x1b:wq\r"),
    "nvim": ("nvim -u NONE", lambda t: f"i{t}\x1b:wq\r", lambda t: f"A{t}\x1b:wq\r"),
    "nano": ("nano --ignorercfiles", lambda t: f"{t}\x18y\r", lambda t: f"\x05{t}\x18y\r"),  # ^E end of line, ^X, y, Enter keeps the name
}

def start(cmd, mode):
    cfg = tempfile.mkdtemp(prefix="cpp-editors-cfg-"); os.makedirs(cfg + "/claude++")
    c = {"editor": cmd}
    if mode == "editor": c["input_mode"] = "editor"
    json.dump(c, open(cfg + "/claude++/config.json", "w"))
    os.environ["XDG_CONFIG_HOME"] = cfg
    log = base + f"/log-{os.getpid()}-{len(os.listdir(base))}"; os.environ["EDITORS_LOG"] = log
    t = Term([CPP]); t.mark = 0; t.drain(1.5); return t, log

def received(log, n, secs=15):
    end = time.time() + secs
    while time.time() < end:
        try: rows = [json.loads(l) for l in open(log)]
        except FileNotFoundError: rows = []
        if len(rows) >= n: return rows
        time.sleep(0.2)
    return rows if os.path.exists(log) else []

def send_keys(t, keys, gap=0.03):
    for ch in keys:
        os.write(t.fd, ch.encode()); t.drain(gap)

def alive(t):
    try: return os.waitpid(t.pid, os.WNOHANG)[0] == 0
    except ChildProcessError: return False

def scenario_a(name):
    cmd, write, _ = EDITORS[name]
    t, log = start(cmd, "editor")
    # the first keystroke opens the editor and is held until it has painted; nano takes text, vim needs `i` first
    first, rest = (write("hello from " + name)[0], write("hello from " + name)[1:])
    os.write(t.fd, first.encode()); t.drain(2.0)
    send_keys(t, rest)
    got = received(log, 1); t.close()
    return got == ["hello from " + name], f"claude received {got!r}"

def scenario_b(name):
    cmd, _, append = EDITORS[name]
    t, log = start(cmd, "default")
    send_keys(t, "draft"); os.write(t.fd, b"\x07"); t.drain(2.0)  # ctrl+g: the draft is in the editor
    send_keys(t, append(" more"))
    t.drain(1.0); os.write(t.fd, b"\r")                           # back in the box: Enter sends
    got = received(log, 1); t.close()
    return got == ["draft more"], f"claude received {got!r}"

def scenario_c(name):
    cmd, write, _ = EDITORS[name]
    t, log = start(cmd, "editor")
    os.write(t.fd, b"i"); t.drain(2.0); send_keys(t, "never sent"); os.write(t.fd, b"\x1c"); t.drain(1.0)   # ctrl+\
    up = alive(t); nothing = not os.path.exists(log)
    os.write(t.fd, write("second try")[0].encode()); t.drain(2.0); send_keys(t, write("second try")[1:])
    got = received(log, 1) if up else []; t.close()
    return up and nothing and got == ["second try"], f"alive={up}, nothing sent by cancel={nothing}, then received {got!r}"

bad = 0
for name in EDITORS:
    if want and name not in want: continue
    if subprocess.run(["which", name], capture_output=True).returncode: print(f"SKIP  {name}: not installed"); continue
    for label, fn in (("A editor mode", scenario_a), ("B ctrl+g", scenario_b), ("C ctrl+\\ cancel", scenario_c)):
        try: ok, d = fn(name)
        except Exception as e: ok, d = False, f"crashed: {e!r}"
        bad += not ok; print(f"{'PASS' if ok else 'FAIL'}  {name:5} {label}: {d}", flush=True)
sys.exit(1 if bad else 0)
