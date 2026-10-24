#!/usr/bin/env python3
"""Controlled benchmark of headless Claude Code launch configurations (stdlib only, macOS).

Controls
  - One factor changes per config vs `base`; `lean*` combine factors. Model is fixed.
  - Fresh fixture copy per run (no session persistence), cwd outside $HOME so no ancestor AGENTS.md leaks in.
  - Memory is read per PID tree (`footprint`, plus sampled peak RSS); nothing system-wide, so other
    sessions on the machine cannot contaminate it. Other processes are never touched.
  - Runs are shuffled (fixed seed) so machine drift hits every config equally; one discarded warm-up per
    config puts its prompt prefix in the cache so cost reflects steady state.
  - Token counts (input + cache read + cache write) are deterministic per config, so they are reported as the
    primary token metric; cost is secondary because it depends on cache state.
  - Load is checked before each run; the run waits if the machine is busy and records the load it saw.
"""
import argparse, json, os, random, re, shutil, statistics as st, subprocess, sys, tempfile, threading, time

MODEL = "haiku"
LEAN_TOOLS = "Bash,Read,Edit,Write,AskUserQuestion,WebFetch,WebSearch"  # keep in sync with src/main.rs
MIN_PROMPT = ("You are a coding agent in a terminal. Inspect and edit files with the tools, "
              "run tests to verify your work, and be concise.")
SOURCES = ["--setting-sources", "project,local"]
TOOLS = ["--tools", LEAN_TOOLS, "--strict-mcp-config"]
SYS = ["--system-prompt", MIN_PROMPT]
CONFIGS = {
    "base": [],
    "sources": SOURCES,
    "tools": TOOLS,
    "sysprompt": SYS,
    "lean": SOURCES + TOOLS,
    "lean+sys": SOURCES + TOOLS + SYS,
}

FIXTURES = {
    "bugfix": {
        "files": {
            "stats.py": "def median(xs):\n    xs = sorted(xs)\n    n = len(xs)\n    return xs[n // 2]\n",
            "test_stats.py": "import unittest\nfrom stats import median\n\nclass T(unittest.TestCase):\n"
                             "    def test_odd(self): self.assertEqual(median([3, 1, 2]), 2)\n"
                             "    def test_even(self): self.assertEqual(median([4, 1, 3, 2]), 2.5)\n",
        },
        "prompt": "The tests in this directory are failing. Fix the bug in the source (not the tests) "
                  "so that `python3 -m unittest -q` passes.",
    },
    "implement": {
        "files": {
            "text.py": 'def wrap_words(text, width):\n    """Greedy word wrap: return a list of lines, each at most `width` chars,\n'
                       '    splitting only on spaces. A word longer than width gets its own line."""\n'
                       "    raise NotImplementedError\n",
            "test_text.py": "import unittest\nfrom text import wrap_words\n\nclass T(unittest.TestCase):\n"
                            "    def test_basic(self): self.assertEqual(wrap_words('aa bb cc', 5), ['aa bb', 'cc'])\n"
                            "    def test_long(self): self.assertEqual(wrap_words('abcdefgh x', 3), ['abcdefgh', 'x'])\n"
                            "    def test_empty(self): self.assertEqual(wrap_words('', 4), [])\n",
        },
        "prompt": "Implement wrap_words in text.py as described in its docstring so the tests pass. "
                  "Do not edit the tests. Verify with `python3 -m unittest -q`.",
    },
    "rename": {
        "files": {
            "shop.py": "def calc_total(items):\n    return sum(p * q for p, q in items)\n",
            "report.py": "from shop import calc_total\n\ndef line(items):\n    return f'total={calc_total(items)}'\n",
            "test_shop.py": "import unittest\nfrom shop import order_total\nfrom report import line\n\nclass T(unittest.TestCase):\n"
                            "    def test_total(self): self.assertEqual(order_total([(2, 3), (1, 4)]), 10)\n"
                            "    def test_line(self): self.assertEqual(line([(1, 1)]), 'total=1')\n",
        },
        "prompt": "Rename the function calc_total to order_total everywhere in this project "
                  "(definition and callers), then run `python3 -m unittest -q`.",
        "extra_check": lambda d: not any("calc_total" in open(os.path.join(d, f)).read()
                                         for f in os.listdir(d) if f.endswith(".py")),
    },
}
PROBE_PROMPT = "Reply with exactly: OK"
TESTS = ["bugfix", "implement", "rename"]


def tree_pids(root):
    out = subprocess.run(["ps", "-A", "-o", "pid=,ppid="], capture_output=True, text=True).stdout
    kids = {}
    for l in out.splitlines():
        p, pp = map(int, l.split())
        kids.setdefault(pp, []).append(p)
    res, stack = [], [root]
    while stack:
        p = stack.pop()
        res.append(p)
        stack += kids.get(p, [])
    return res


def footprint_mb(pids):
    """phys_footprint per pid in MB via /usr/bin/footprint."""
    args = ["footprint"] + [a for p in pids for a in ("-p", str(p))]
    out = subprocess.run(args, capture_output=True, text=True).stdout
    res = {}
    for m in re.finditer(r"\[(\d+)\]: 64-bit\s+Footprint:\s+([\d.]+)\s+(KB|MB|GB)", out):
        res[int(m[1])] = float(m[2]) * {"KB": 1 / 1024, "MB": 1, "GB": 1024}[m[3]]
    return res


def rss_tree_mb(root):
    pids = ",".join(map(str, tree_pids(root)))
    out = subprocess.run(["ps", "-o", "rss=", "-p", pids], capture_output=True, text=True).stdout
    return sum(int(x) for x in out.split()) / 1024


def wait_for_quiet(max_load, log):
    for _ in range(30):
        l1 = os.getloadavg()[0]
        if l1 <= max_load:
            return l1
        log(f"  load {l1:.1f} > {max_load:.1f}, waiting")
        time.sleep(2)
    return os.getloadavg()[0]


def run_session(flags, prompt, cwd, claude="claude"):
    argv = [claude, "-p", "--input-format", "stream-json", "--output-format", "stream-json", "--verbose",
            "--permission-prompt-tool", "stdio", "--model", MODEL, "--no-session-persistence"] + flags
    t0 = time.time()
    p = subprocess.Popen(argv, stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL,
                         text=True, cwd=cwd)

    def send(o):
        p.stdin.write(json.dumps(o) + "\n")
        p.stdin.flush()

    send({"type": "control_request", "request_id": "i", "request": {"subtype": "initialize"}})
    send({"type": "user", "message": {"role": "user", "content": prompt}})
    peak = [0.0]
    stop = threading.Event()

    def mon():
        while not stop.is_set():
            try:
                peak[0] = max(peak[0], rss_tree_mb(p.pid))
            except Exception:
                pass
            stop.wait(0.25)

    threading.Thread(target=mon, daemon=True).start()
    r = {"startup_ms": None}
    for line in p.stdout:
        o = json.loads(line)
        t = o.get("type")
        if t == "system" and o.get("subtype") == "init" and r["startup_ms"] is None:
            r["startup_ms"] = round((time.time() - t0) * 1000)
        elif t == "control_request" and o["request"].get("subtype") == "can_use_tool":
            send({"type": "control_response", "response": {"subtype": "success", "request_id": o["request_id"],
                  "response": {"behavior": "allow", "updatedInput": o["request"]["input"]}}})
        elif t == "result":
            u = o["usage"]
            r.update(input=u["input_tokens"], cache_read=u["cache_read_input_tokens"],
                     cache_write=u["cache_creation_input_tokens"], output=u["output_tokens"],
                     cost=o.get("total_cost_usd", 0.0), turns=o.get("num_turns"),
                     duration_ms=o.get("duration_ms"), is_error=bool(o.get("is_error")))
            r["prompt_total"] = r["input"] + r["cache_read"] + r["cache_write"]
            pids = tree_pids(p.pid)
            fp = footprint_mb(pids)
            r["fp_claude_mb"] = round(fp.get(p.pid, 0), 1)
            r["fp_tree_mb"] = round(sum(fp.values()), 1)
            r["procs"] = len(pids)
            break
    stop.set()
    r["peak_rss_tree_mb"] = round(peak[0], 1)
    p.stdin.close()
    try:
        p.wait(timeout=10)
    except subprocess.TimeoutExpired:
        p.kill()
        p.wait()
    return r


def fixture(name, root):
    d = tempfile.mkdtemp(prefix=f"{name}-", dir=root)
    for f, c in FIXTURES[name]["files"].items():
        open(os.path.join(d, f), "w").write(c)
    return d


def passes(name, d):
    ok = subprocess.run(["python3", "-m", "unittest", "-q"], cwd=d, capture_output=True).returncode == 0
    extra = FIXTURES[name].get("extra_check")
    return ok and (extra(d) if extra else True)


def med(xs):
    xs = [x for x in xs if x is not None]
    return st.median(xs) if xs else None


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--configs", default=",".join(CONFIGS))
    ap.add_argument("--probe-reps", type=int, default=3)
    ap.add_argument("--task-reps", type=int, default=2)
    ap.add_argument("--tasks", default=",".join(TESTS))
    ap.add_argument("--max-load", type=float, default=4.0)
    ap.add_argument("--seed", type=int, default=7)
    ap.add_argument("--out", default="bench-results.json")
    ap.add_argument("--workdir", required=True, help="scratch dir OUTSIDE $HOME-ancestors-with-AGENTS.md")
    a = ap.parse_args()
    cfgs = a.configs.split(",")
    tasks = [t for t in a.tasks.split(",") if t]
    os.makedirs(a.workdir, exist_ok=True)
    log = lambda s: print(s, file=sys.stderr, flush=True)

    jobs = [(c, "probe", None, i) for c in cfgs for i in range(a.probe_reps)]
    jobs += [(c, "task", t, i) for c in cfgs for t in tasks for i in range(a.task_reps)]
    random.Random(a.seed).shuffle(jobs)

    log(f"preflight: ncpu={os.cpu_count()} load={os.getloadavg()[0]:.2f} jobs={len(jobs)} (+{len(cfgs)} warm-ups)")
    rows = []
    for c in cfgs:  # warm-up: discarded, primes the prompt cache for this config's prefix
        d = fixture("bugfix", a.workdir)
        log(f"warm-up {c}")
        run_session(CONFIGS[c], PROBE_PROMPT, d)
        shutil.rmtree(d, ignore_errors=True)
    for n, (c, kind, t, i) in enumerate(jobs, 1):
        load = wait_for_quiet(a.max_load, log)
        d = fixture(t or "bugfix", a.workdir)
        prompt = FIXTURES[t]["prompt"] if kind == "task" else PROBE_PROMPT
        log(f"[{n}/{len(jobs)}] {c} {kind} {t or ''} #{i}")
        r = run_session(CONFIGS[c], prompt, d)
        r.update(config=c, kind=kind, task=t, rep=i, load1=round(load, 2))
        if kind == "task":
            r["success"] = passes(t, d)
        shutil.rmtree(d, ignore_errors=True)
        rows.append(r)
        json.dump(rows, open(a.out, "w"), indent=1)

    base_floor = med([r["prompt_total"] for r in rows if r["config"] == "base" and r["kind"] == "probe"])
    print(f"\nmodel={MODEL}  probe reps={a.probe_reps}  task reps={a.task_reps}  (medians; tokens deterministic)\n")
    print(f"{'config':<10} {'startup ms':>10} {'floor tok':>10} {'vs base':>8} {'fp claude':>10} {'fp tree':>8} {'peak rss':>9}")
    for c in cfgs:
        pr = [r for r in rows if r["config"] == c and r["kind"] == "probe"]
        floor = med([r["prompt_total"] for r in pr])
        d = f"{100 * (floor - base_floor) / base_floor:+.0f}%" if floor and base_floor else "-"
        print(f"{c:<10} {med([r['startup_ms'] for r in pr]):>10} {floor:>10} {d:>8} "
              f"{med([r['fp_claude_mb'] for r in pr]):>9}M {med([r['fp_tree_mb'] for r in pr]):>7}M "
              f"{med([r['peak_rss_tree_mb'] for r in pr]):>8}M")
    print(f"\n{'config':<10} {'task':<10} {'pass':>5} {'turns':>6} {'prompt tok':>11} {'output':>7} {'cost $':>8} {'secs':>6}")
    for c in cfgs:
        for t in tasks:
            tr = [r for r in rows if r["config"] == c and r["task"] == t]
            if not tr:
                continue
            print(f"{c:<10} {t:<10} {sum(r['success'] for r in tr):>2}/{len(tr)} {med([r['turns'] for r in tr]):>6} "
                  f"{med([r['prompt_total'] for r in tr]):>11.0f} {med([r['output'] for r in tr]):>7.0f} "
                  f"{med([r['cost'] for r in tr]):>8.4f} {med([r['duration_ms'] for r in tr]) / 1000:>6.1f}")
    print("\nloads seen:", f"min {min(r['load1'] for r in rows)} max {max(r['load1'] for r in rows)}")


if __name__ == "__main__":
    main()
