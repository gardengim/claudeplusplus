#!/usr/bin/env python3
"""The thesis as a budget: a resting claude++ must stay tiny, whatever features are added. No model calls (free, ~15 s).
Fails (exit 1) when a change makes it heavier. Run after `cargo build --release`:  python3 bench/budget.py"""
import sys
from _term import *
sys.argv = sys.argv[:1]
os.makedirs(W, exist_ok=True)
BUDGET = {"binary_mb": 3.0, "resting_mb": 8.0, "first_paint_ms": 150, "idle_cpu_s_per_10s": 0.1}

size = os.path.getsize(CPP) / 1048576
# Its own supervisor directory: the one your real windows use must not decide (or be replaced by) this measurement.
t = Term([CPP, "--model", "haiku"], env_extra={"CLAUDEPP_SUP_DIR": W + "/sup"})
t0 = time.time(); paint = None
while paint is None and time.time() - t0 < 5:
    if select.select([t.fd], [], [], 0.002)[0]:
        try: d = os.read(t.fd, 65536)
        except OSError: break
        t.buf.extend(d); paint = (time.time() - t0) * 1000
        if b"\x1b[6n" in d: os.write(t.fd, b"\x1b[1;1R")
t.mark = 0; t.drain(3)
def cpu(pid):
    s = subprocess.run(["ps", "-o", "time=", "-p", str(pid)], capture_output=True, text=True).stdout.strip().split(":")
    return sum(float(x) * 60 ** i for i, x in enumerate(reversed(s)))
c0 = cpu(t.pid); t.drain(10); c1 = cpu(t.pid)
ps = tree(t.pid)
got = {"binary_mb": round(size, 2), "resting_mb": footprint(ps), "first_paint_ms": round(paint or 9999),
       "idle_cpu_s_per_10s": round(c1 - c0, 2), "processes": names(ps)}
t.close()
bad = [k for k in BUDGET if got[k] > BUDGET[k]]
print(json.dumps({"got": got, "budget": BUDGET}, indent=1))
print("OVER BUDGET: " + ", ".join(bad) if bad else "WITHIN BUDGET")
sys.exit(1 if bad else 0)
