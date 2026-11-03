#!/usr/bin/env python3
"""Long-session soak, no model calls: a fake `claude` floods claude++ with N turns (streamed text, a tool call and a
2 KB result each). Reports claude++'s memory for two sizes (growth per turn), then key-echo latency and idle CPU
while holding the long transcript. Usage: python3 bench/soak.py [small=300] [large=3000]"""
import os, sys, tempfile, stat
SMALL = int(sys.argv[1]) if len(sys.argv) > 1 else 300
LARGE = int(sys.argv[2]) if len(sys.argv) > 2 else 3000
sys.argv = sys.argv[:1]
_src = open(__file__.replace("soak.py", "tabs.py")).read().split("os.makedirs(W")[0]  # reuse Term/tree/footprint
exec(_src)
os.makedirs(W, exist_ok=True)

FAKE = r'''#!/usr/bin/env python3
import sys, json, os
N = int(os.environ["SOAK_TURNS"])
def out(o): sys.stdout.write(json.dumps(o) + "\n"); sys.stdout.flush()
def ctl(rid, resp): out({"type": "control_response", "response": {"subtype": "success", "request_id": rid, "response": resp}})
usage = {"input_tokens": 3, "cache_read_input_tokens": 1000, "cache_creation_input_tokens": 10, "output_tokens": 50}
for line in sys.stdin:
    m = json.loads(line)
    if m.get("type") == "control_request":
        sub = m["request"]["subtype"]
        ctl(m["request_id"], {"mcpServers": []} if sub == "mcp_status" else {})
    elif m.get("type") == "user":
        out({"type": "system", "subtype": "init", "session_id": "soak-1", "model": "fake", "tools": ["Bash"], "claude_code_version": "2.1.317"})
        for i in range(N):
            mid = f"m{i}"
            out({"type": "stream_event", "event": {"type": "message_start", "message": {"id": mid}}})
            out({"type": "stream_event", "event": {"type": "content_block_start", "index": 0, "content_block": {"type": "text", "text": ""}}})
            for w in range(4):
                out({"type": "stream_event", "event": {"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": f"Turn {i} part {w}: " + "lorem ipsum dolor sit amet " * 6}}})
            out({"type": "stream_event", "event": {"type": "content_block_stop", "index": 0}})
            out({"type": "assistant", "message": {"id": mid, "content": [{"type": "text", "text": f"Turn {i}: " + "lorem ipsum dolor sit amet " * 24},
                 {"type": "tool_use", "id": f"t{i}", "name": "Bash", "input": {"command": f"echo {i}"}}], "usage": usage}})
            out({"type": "user", "message": {"content": [{"type": "tool_result", "tool_use_id": f"t{i}", "content": ("y" * 30_000_000 if os.environ.get("SOAK_BIG") and i == 0 else "\n".join(f"line {j} of output {i}" for j in range(80)))}]}})
        out({"type": "result", "subtype": "success", "is_error": False, "result": "done", "usage": usage, "total_cost_usd": 0.01, "duration_api_ms": 5})
'''
d = tempfile.mkdtemp(prefix="cpp-soak-")
open(d + "/claude", "w").write(FAKE); os.chmod(d + "/claude", 0o755)
os.environ["PATH"] = d + ":" + os.environ["PATH"]
os.environ["XDG_CONFIG_HOME"] = tempfile.mkdtemp(prefix="cpp-soak-cfg-")

def run(turns, hold=0, big=False):
    os.environ["SOAK_TURNS"] = str(turns)
    os.environ.pop("SOAK_BIG", None)
    if big: os.environ["SOAK_BIG"] = "1"
    t = Term([CPP]); t.mark = 0; t.drain(1)
    t.mark = len(t.buf); os.write(t.fd, b"go"); t.drain(0.3); os.write(t.fd, b"\r")
    done = t.drain(240, b"Worked for"); t.drain(2)
    mb = footprint(tree(t.pid))
    r = {"turns": turns, "finished": done, "mb": mb}
    if hold:
        c0 = subprocess.run(["ps", "-o", "time=", "-p", str(t.pid)], capture_output=True, text=True).stdout.strip()
        t.drain(10)
        c1 = subprocess.run(["ps", "-o", "time=", "-p", str(t.pid)], capture_output=True, text=True).stdout.strip()
        lat = []
        for _ in range(10):
            t0 = time.time(); os.write(t.fd, b"x")
            while time.time() - t0 < 2 and not select.select([t.fd], [], [], 0.001)[0]: pass
            lat.append((time.time() - t0) * 1000); t.drain(0.15)
        os.write(t.fd, b"\x15")
        r.update(idle_cpu=f"{c0} -> {c1}", echo_ms_median=round(sorted(lat)[5], 1), echo_ms_max=round(max(lat), 1))
    t.close(); return r

a = run(SMALL); b = run(LARGE, hold=1); big = run(3, hold=1, big=True)  # one 30 MB single-line tool result
per = (b["mb"] - a["mb"]) / (LARGE - SMALL) * 1024
print(json.dumps({"small": a, "large": b, "KB_per_turn": round(per, 1), "one 30 MB tool result": big}, indent=1))
ok = a["finished"] and b["finished"] and b["mb"] < 120 and b["echo_ms_median"] < 50 and big["finished"] and big["mb"] < 400 and big["echo_ms_median"] < 100
print("PASS" if ok else "FAIL")
sys.exit(0 if ok else 1)
