#!/usr/bin/env python3
"""Which knob of the installed `claude` saves tokens, memory or CPU? (stdlib, macOS; haiku unless a stage says
otherwise; a few cents per stage.) One factor changes per run against the `lean` base claude++ ships.
  python3 bench/levers.py envs          every boolean env variable the binary reads, one at a time, trivial turn
  python3 bench/levers.py base [N]      the base N times (noise floor)
  python3 bench/levers.py hidden        a tool turn on a bigger model: which OTHER models did Claude Code call?
  python3 bench/levers.py think         thinking/effort knobs on a bigger model: output tokens and cost
  python3 bench/levers.py big           one 3 MB tool result: peak memory, with and without the output caps
  python3 bench/levers.py one NAME=VAL [NAME=VAL ...]   any env/flag set, same measures
Each run: a fresh folder, a clean environment (no CLAUDE* variables from a parent session), the backend flags
claude++ uses. Measures: tokens of the first request, every model that was called (side calls included, from the
result's modelUsage), cost, footprint (macOS `footprint`, summed over the process tree) after the turn, CPU
seconds over an idle window, startup time. Results: bench/results/<date>-levers-<stage>.json"""
import datetime, json, os, queue, re, shutil, subprocess, sys, tempfile, threading, time

LEAN_TOOLS = re.search(r'LEAN_TOOLS[^"]*"([^"]+)"', open(os.path.join(os.path.dirname(os.path.abspath(__file__)), "../src/profile.rs")).read()).group(1)
FULL = os.environ.get("LEVERS_BASE") == "full"  # no profile: your own settings, plugins, skills and MCP servers
BASE_ARGS = [] if FULL else ["--tools", LEAN_TOOLS, "--setting-sources", "project,local", "--strict-mcp-config"]
BASE_ENV = {} if FULL else {"CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC": "1"}
FLAGS = ["-p", "--input-format", "stream-json", "--output-format", "stream-json", "--verbose", "--include-partial-messages",
         "--permission-prompt-tool", "stdio", "--no-session-persistence"]
TRIVIAL = "What is 6 times 7? Reply with the number followed by the letter Z and nothing else."
OUT = os.path.join(os.path.dirname(os.path.abspath(__file__)), "results")


def tree(root):
    out = subprocess.run(["ps", "-A", "-o", "pid=,ppid=,time="], capture_output=True, text=True).stdout
    kids, cpu = {}, {}
    for l in out.splitlines():
        p = l.split()
        if len(p) < 3: continue
        kids.setdefault(int(p[1]), []).append(int(p[0]))
        s = 0.0
        for x in p[2].replace("-", ":").split(":"): s = s * 60 + float(x)
        cpu[int(p[0])] = s
    seen, todo = [], [root]
    while todo:
        x = todo.pop()
        if x in cpu: seen.append(x); todo += kids.get(x, [])
    return seen, sum(cpu[x] for x in seen)


def footprint(pids):
    args = [a for p in pids for a in ("-p", str(p))]
    out = subprocess.run(["footprint"] + args, capture_output=True, text=True).stdout
    mul = {"KB": 1 / 1024, "MB": 1, "GB": 1024}
    return round(sum(float(n) * mul[u] for n, u in re.findall(r"\]: .*?Footprint: ([\d.]+) (KB|MB|GB)", out)), 1)


def run(name, args=(), env=None, prompts=(TRIVIAL,), model="haiku", idle=10, cwd=None, peak=False, setup=None, sample=False, settle=False, detail=False):
    cwd = cwd or tempfile.mkdtemp(prefix="cpp-lev-")
    if setup: setup(cwd)
    e = {k: v for k, v in os.environ.items() if not k.startswith("CLAUDE") and not k.startswith("ANTHROPIC_")}
    e.update(BASE_ENV); e.update(env or {})
    e = {k: v for k, v in e.items() if v is not None}
    t0 = time.time()
    p = subprocess.Popen(["claude", *FLAGS, "--model", model, *BASE_ARGS, *args], cwd=cwd, env=e, stdin=subprocess.PIPE,
                         stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
    q = queue.Queue()
    def pump():
        for l in p.stdout:
            try: q.put(json.loads(l))
            except ValueError: pass
        q.put(None)
    threading.Thread(target=pump, daemon=True).start()
    err = []
    threading.Thread(target=lambda: err.append(p.stderr.read()), daemon=True).start()
    r = {"name": name, "ok": False, "first_request_tokens": None, "results": []}
    ready = None
    peak_mb = 0
    if settle:  # wait for every MCP server to finish connecting, so the first request lists all their tools
        end, n = time.time() + 30, 0
        while time.time() < end:
            n += 1
            p.stdin.write(json.dumps({"type": "control_request", "request_id": f"lev-{n}", "request": {"subtype": "mcp_status"}}) + "\n"); p.stdin.flush()
            servers, until = None, time.time() + 6
            while time.time() < until:
                try: m = q.get(timeout=0.5)
                except queue.Empty: continue
                if m is None: break
                if m.get("type") == "system" and m.get("subtype") == "init" and ready is None: ready = time.time() - t0
                if m.get("type") == "control_response" and m["response"].get("request_id") == f"lev-{n}":
                    servers = (m["response"].get("response") or {}).get("mcpServers", []); break
            if servers is not None and not any(x.get("status") == "pending" for x in servers):
                r["mcp"] = [(x.get("name"), x.get("status")) for x in servers]; break
            time.sleep(0.5)
    for text in prompts:
        p.stdin.write(json.dumps({"type": "user", "session_id": "", "parent_tool_use_id": None, "message": {"role": "user", "content": text}}) + "\n"); p.stdin.flush()
        end = time.time() + 240
        while time.time() < end:
            try: m = q.get(timeout=0.5 if peak else 1)
            except queue.Empty:
                if peak: peak_mb = max(peak_mb, footprint(tree(p.pid)[0]))
                continue
            if m is None: break
            if m.get("type") == "system" and m.get("subtype") == "init" and ready is None: ready = time.time() - t0
            if m.get("type") == "stream_event" and m["event"].get("type") == "message_start" and r["first_request_tokens"] is None:
                u = m["event"]["message"].get("usage", {})
                r["first_request_tokens"] = sum(u.get(k, 0) or 0 for k in ("input_tokens", "cache_creation_input_tokens", "cache_read_input_tokens"))
            if peak and m.get("type") == "stream_event": peak_mb = max(peak_mb, footprint(tree(p.pid)[0]))
            if m.get("type") == "result":
                r["results"].append({"is_error": m.get("is_error"), "text": str(m.get("result"))[:240], "cost": m.get("total_cost_usd"),
                                     "turns": m.get("num_turns"), "usage": m.get("usage"), "models": {k: {"in": v.get("inputTokens"), "out": v.get("outputTokens"),
                                     "cache_read": v.get("cacheReadInputTokens"), "cache_write": v.get("cacheCreationInputTokens"), "cost": v.get("costUSD")} for k, v in (m.get("modelUsage") or {}).items()}})
                if sample: r.setdefault("fp", []).append(footprint(tree(p.pid)[0]))
                break
    pids, cpu0 = tree(p.pid)
    if detail:
        names = dict(l.split(None, 1) for l in subprocess.run(["ps", "-o", "pid=,comm="] + [a for x in pids for a in ("-p", str(x))], capture_output=True, text=True).stdout.splitlines())
        r["children"] = sorted(((os.path.basename(names.get(str(x), "?").strip()), footprint([x])) for x in pids), key=lambda t: -t[1])
    r["footprint_mb"] = footprint(pids); r["procs"] = len(pids); r["startup_s"] = round(ready or 0, 2)
    time.sleep(idle)
    pids, cpu1 = tree(p.pid)
    r["idle_cpu_s"] = round(cpu1 - cpu0, 2); r["idle_window_s"] = idle
    if peak: r["peak_mb"] = peak_mb
    r["ok"] = bool(r["results"]) and not r["results"][-1]["is_error"]
    try: p.stdin.close(); p.wait(15)
    except Exception: p.kill()
    if err and err[0].strip() and not r["ok"]: r["stderr"] = err[0][-300:]
    shutil.rmtree(cwd, ignore_errors=True)
    return r


def show(r, base=None):
    m = r["results"][-1]["models"] if r["results"] else {}
    models = ",".join(f"{k.split('-')[1] if '-' in k else k}:{(v['in'] or 0) + (v['cache_read'] or 0) + (v['cache_write'] or 0)}/{v['out']}" for k, v in m.items())
    cost = sum(x["cost"] or 0 for x in r["results"])
    d = ""
    if base and r["first_request_tokens"] and base.get("first_request_tokens"):
        d = f"  Δtok {r['first_request_tokens'] - base['first_request_tokens']:+d}  Δmem {r['footprint_mb'] - base['footprint_mb']:+.0f}MB"
    print(f"{'ok ' if r['ok'] else 'ERR'} {r['name']:44} tok {r['first_request_tokens']!s:>6}  mem {r['footprint_mb']:6.1f}MB  idleCPU {r['idle_cpu_s']:5.2f}s/{r['idle_window_s']}s  start {r['startup_s']:4.1f}s  ${cost:.4f}  [{models}]{d}" + (f"\n    {r.get('stderr')}" if r.get("stderr") else ""), flush=True)


def save(stage, rows):
    os.makedirs(OUT, exist_ok=True)
    f = os.path.join(OUT, f"{datetime.date.today()}-levers-{stage}.json")
    json.dump({"claude": subprocess.run(["claude", "--version"], capture_output=True, text=True).stdout.strip(), "rows": rows}, open(f, "w"), indent=1)
    print("->", f)


# Boolean variables read by the binary that could plausibly save something. Names verified with `strings` on 2.1.318.
BOOLS = """CLAUDE_CODE_DISABLE_FILE_CHECKPOINTING CLAUDE_CODE_DISABLE_CRON CLAUDE_CODE_DISABLE_PROACTIVITY
CLAUDE_CODE_DISABLE_AGENT_PROGRESS_SUMMARIES CLAUDE_CODE_DISABLE_ADVISOR_TOOL CLAUDE_CODE_DISABLE_WORKFLOWS
CLAUDE_CODE_DISABLE_FILE_MENTIONS CLAUDE_CODE_DISABLE_BUNDLED_SKILLS CLAUDE_CODE_DISABLE_CLAUDE_API_SKILL
CLAUDE_CODE_DISABLE_CLAUDE_CODE_SKILL CLAUDE_CODE_DISABLE_EXPLORE_PLAN_AGENTS CLAUDE_CODE_DISABLE_TERMINAL_TITLE
CLAUDE_CODE_DISABLE_FEEDBACK_SURVEY CLAUDE_CODE_DISABLE_RESPONSE_COMPARISON CLAUDE_CODE_DISABLE_ULTRATHINK
CLAUDE_CODE_DISABLE_ADAPTIVE_THINKING CLAUDE_CODE_DISABLE_ATTACHMENTS CLAUDE_CODE_DISABLE_AUTO_MEMORY
CLAUDE_CODE_DISABLE_GIT_INSTRUCTIONS CLAUDE_CODE_DISABLE_BACKGROUND_TASKS CLAUDE_CODE_DISABLE_CLAUDE_MDS
CLAUDE_CODE_DISABLE_TRANSCRIPT_READ_AHEAD CLAUDE_CODE_DISABLE_MEMORY_PERIODIC_RESYNC CLAUDE_CODE_DISABLE_MEMORY_STREAM_LIST
CLAUDE_CODE_DISABLE_ORG_MEMORY CLAUDE_CODE_DISABLE_STARTUP_WORK_GATE CLAUDE_CODE_DISABLE_HOME_SETTINGS_SEED
CLAUDE_CODE_DISABLE_OFFICIAL_MARKETPLACE_AUTOINSTALL CLAUDE_CODE_DISABLE_VITALS_EMITTER CLAUDE_CODE_DISABLE_FAST_MODE
CLAUDE_CODE_DISABLE_1M_CONTEXT CLAUDE_CODE_DISABLE_AGENT_VIEW CLAUDE_CODE_DISABLE_ARTIFACT CLAUDE_CODE_DISABLE_LAUNCH_COMPOSER
CLAUDE_CODE_DISABLE_MCP_TASKS CLAUDE_CODE_DISABLE_AWAITING_USER_IDLE CLAUDE_CODE_DISABLE_EXPERIMENTAL_BETAS
CLAUDE_CODE_DISABLE_HELD_MESSAGE_RECOGNITION CLAUDE_CODE_DISABLE_STRUCTURED_OUTPUTS CLAUDE_CODE_DISABLE_BG_STABLE_PATH
CLAUDE_CODE_DISABLE_WORKING_SYNC CLAUDE_CODE_DISABLE_PRECOMPACT_SKIP CLAUDE_CODE_DISABLE_POLICY_SKILLS
DISABLE_GROWTHBOOK DISABLE_TELEMETRY DISABLE_ERROR_REPORTING DISABLE_AUTOUPDATER DISABLE_COST_WARNINGS
DISABLE_INSTALLATION_CHECKS DISABLE_INTERLEAVED_THINKING CLAUDE_CODE_PLUGIN_DIR_WATCH=0 CLAUDE_CODE_CONFIG_WATCH_EVENTS=0""".split()


def kv(s):
    return (s.split("=", 1) + ["1"])[:2] if "=" in s else (s, "1")


def main():
    stage = sys.argv[1] if len(sys.argv) > 1 else "envs"
    rows = []
    if stage == "base":
        n = int(sys.argv[2]) if len(sys.argv) > 2 else 3
        for i in range(n):
            rows.append(run(f"base #{i + 1}")); show(rows[-1])
    elif stage == "envs":
        b = run("base"); show(b); rows.append(b)
        for s in BOOLS:
            k, v = kv(s)
            rows.append(run(f"{k}={v}", env={k: v})); show(rows[-1], b)
        b2 = run("base (again)"); show(b2); rows.append(b2)
    elif stage == "full":
        # LEVERS_BASE=full python3 bench/levers.py full : the unprofiled backend, as `full` runs it
        b = run("full (your config)"); show(b); rows.append(b)
        for name, args, env in [
                ("--disable-slash-commands", ["--disable-slash-commands"], {}),
                ("--exclude-dynamic-system-prompt-sections", ["--exclude-dynamic-system-prompt-sections"], {}),
                ("--setting-sources project,local", ["--setting-sources", "project,local"], {}),
                ("--strict-mcp-config", ["--strict-mcp-config"], {}),
                ("--strict-mcp-config + project,local", ["--strict-mcp-config", "--setting-sources", "project,local"], {}),
                ("NONESSENTIAL_TRAFFIC", [], {"CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC": "1"}),
                ("DISABLE_BUNDLED_SKILLS", [], {"CLAUDE_CODE_DISABLE_BUNDLED_SKILLS": "1"}),
                ("DISABLE_CLAUDE_MDS", [], {"CLAUDE_CODE_DISABLE_CLAUDE_MDS": "1"}),
                ("DISABLE_AUTO_MEMORY", [], {"CLAUDE_CODE_DISABLE_AUTO_MEMORY": "1"}),
                ("DISABLE_GIT_INSTRUCTIONS", [], {"CLAUDE_CODE_DISABLE_GIT_INSTRUCTIONS": "1"}),
                ("DISABLE_EXPLORE_PLAN_AGENTS", [], {"CLAUDE_CODE_DISABLE_EXPLORE_PLAN_AGENTS": "1"}),
                ("DISABLE_CRON", [], {"CLAUDE_CODE_DISABLE_CRON": "1"}),
                ("DISABLE_WORKFLOWS", [], {"CLAUDE_CODE_DISABLE_WORKFLOWS": "1"}),
                ("DISABLE_ADVISOR_TOOL", [], {"CLAUDE_CODE_DISABLE_ADVISOR_TOOL": "1"}),
                ("DISABLE_ATTACHMENTS", [], {"CLAUDE_CODE_DISABLE_ATTACHMENTS": "1"}),
                ("MAX_MCP_DESCRIPTION_LENGTH=200", [], {"CLAUDE_CODE_MAX_MCP_DESCRIPTION_LENGTH": "200"}),
                ("DISABLE_BACKGROUND_TASKS", [], {"CLAUDE_CODE_DISABLE_BACKGROUND_TASKS": "1"})]:
            rows.append(run(name, args=args, env=env)); show(rows[-1], b)
        b2 = run("full (again)"); show(b2); rows.append(b2)
    elif stage == "xcache":
        # Do two NEW sessions in different folders (two tabs, or two days) share the cached prompt? A git repo, so the
        # dynamic part (cwd, git status) is real. B's cache_read vs cache_write is the answer.
        def repo(cwd):
            subprocess.run("git init -q && echo hi > a.txt && git add . && git -c user.email=a@b -c user.name=x commit -qm init && echo dirty >> a.txt", shell=True, cwd=cwd)
        for rnd in (1, 2):
            for name, args in [("default", []), ("--exclude-dynamic-system-prompt-sections", ["--exclude-dynamic-system-prompt-sections"])]:
                for who in "AB":
                    r = run(f"{name} #{rnd} {who}", args=args, setup=repo, idle=0); rows.append(r)
                    u = r["results"][-1]["usage"] or {}
                    print(f"   {r['name']:52} cache_read {u.get('cache_read_input_tokens')}  cache_write {u.get('cache_creation_input_tokens')}  fresh {u.get('input_tokens')}  ${r['results'][-1]['cost']:.4f}", flush=True)
    elif stage == "grow":
        # Memory of the backend over a long session: 25 turns, each prints ~9 KB with Bash. Footprint after every turn.
        turn = "Use the Bash tool to run `seq 1 2000` and then reply with only the word ok."
        envs = [("base", {}), ("BUN_JSC_gcMaxHeapSize=64000000", {"BUN_JSC_gcMaxHeapSize": "64000000"})]
        for name, env in envs:
            r = run(name, env=env, prompts=(turn,) * 25, args=["--allowedTools", "Bash"], idle=5, sample=True); rows.append(r); show(r)
            print("   footprint per turn:", r.get("fp"), flush=True)
    elif stage == "mcp":
        # What does each MCP server cost? (stdio ones defined in ~/.claude.json or the project's .mcp.json; plugin and
        # claude.ai servers cannot be picked one by one, so they are measured as part of "everything".)
        c = json.load(open(os.path.expanduser("~/.claude.json")))
        defs = dict(c.get("mcpServers", {}))
        for v in c.get("projects", {}).values(): defs.update({k: d for k, d in v.get("mcpServers", {}).items() if k not in defs})
        b = run("everything (full)", settle=True, detail=True, idle=3); show(b); rows.append(b)
        print("   servers:", b.get("mcp")); print("   biggest processes:", b.get("children", [])[:8], flush=True)
        n = run("--strict-mcp-config, none", args=["--strict-mcp-config"], settle=True, detail=True, idle=3); show(n, b); rows.append(n)
        for name, d in defs.items():
            f = tempfile.mktemp(suffix=".json"); json.dump({"mcpServers": {name: d}}, open(f, "w"))
            r = run(f"only {name}", args=["--strict-mcp-config", "--mcp-config", f], settle=True, detail=True, idle=3); show(r, n); rows.append(r)
            print("   ", r.get("mcp"), r.get("children", [])[:4], flush=True)
    elif stage == "caps":
        # A big tool result is re-sent on every later request of the session. How much do the output caps save, and does
        # the model still answer right? Bash prints 108 KB; Read opens a 400 KB file whose last line is known.
        def files(cwd):
            open(cwd + "/big.txt", "w").write("".join(f"line {i} " + "x" * 30 + "\n" for i in range(1, 9001)))
        bash = "Use the Bash tool to run `seq 1 20000`. Then reply with only: the last number you can see in the output, and the word TRUNCATED if the output looks cut."
        read = "Use the Read tool on big.txt. Then reply with only: the number of the last line you can see, and the word TRUNCATED if it looks cut."
        arms = [("default", {}), ("BASH_MAX_OUTPUT_LENGTH=8000", {"BASH_MAX_OUTPUT_LENGTH": "8000"}), ("BASH_MAX_OUTPUT_LENGTH=2000", {"BASH_MAX_OUTPUT_LENGTH": "2000"}),
                ("FILE_READ_MAX_OUTPUT_TOKENS=4000", {"CLAUDE_CODE_FILE_READ_MAX_OUTPUT_TOKENS": "4000"}), ("FILE_READ_MAX_OUTPUT_TOKENS=1000", {"CLAUDE_CODE_FILE_READ_MAX_OUTPUT_TOKENS": "1000"})]
        for what, text in (("bash", bash), ("read", read)):
            for name, env in arms:
                r = run(f"{what}: {name}", env=env, prompts=(text,), args=["--allowedTools", "Bash,Read"], setup=files, idle=0); rows.append(r)
                u = (r["results"][-1]["usage"] or {}) if r["results"] else {}
                tot = sum(u.get(k, 0) or 0 for k in ("input_tokens", "cache_creation_input_tokens", "cache_read_input_tokens"))
                print(f"   {r['name']:44} request tokens in total {tot:7}  out {u.get('output_tokens')}  ${r['results'][-1]['cost'] if r['results'] else 0:.4f}  answer: {r['results'][-1]['text'][:90] if r['results'] else r.get('stderr')!r}", flush=True)
    elif stage == "hidden":
        # Does Claude Code call OTHER models behind our back (titles, command classification, summaries)? Main model is
        # sonnet, so any haiku entry in modelUsage is a side call. A tool turn is where those calls happen.
        tool = "Use the Bash tool to run `echo hello` and then reply with the single word done."
        for name, env in [("sonnet tool turn", {}), ("+DISABLE_NON_ESSENTIAL_MODEL_CALLS", {"DISABLE_NON_ESSENTIAL_MODEL_CALLS": "1"}),
                          ("+DISABLE_AGENT_PROGRESS_SUMMARIES", {"CLAUDE_CODE_DISABLE_AGENT_PROGRESS_SUMMARIES": "1"})]:
            rows.append(run(name, env=env, prompts=(tool,), model="sonnet", args=["--allowedTools", "Bash"])); show(rows[-1], rows[0])
    elif stage == "think":
        task = ("How many different ways can you make change for 100 cents using pennies, nickels, dimes, quarters and "
                "half-dollars? Work it out carefully, then reply with the number and one sentence.")
        for name, args, env in [("sonnet default", [], {}), ("--effort low", ["--effort", "low"], {}), ("--effort medium", ["--effort", "medium"], {}),
                                ("DISABLE_ADAPTIVE_THINKING", [], {"CLAUDE_CODE_DISABLE_ADAPTIVE_THINKING": "1"}),
                                ("MAX_THINKING_TOKENS=1024", [], {"MAX_THINKING_TOKENS": "1024"}),
                                ("CLAUDE_CODE_DISABLE_THINKING", [], {"CLAUDE_CODE_DISABLE_THINKING": "1"})]:
            rows.append(run(name, args=args, env=env, prompts=(task,), model="sonnet", idle=1)); show(rows[-1], rows[0])
    elif stage == "one":
        for s in sys.argv[2:]:
            k, v = kv(s)
            rows.append(run(f"{k}={v}", env={k: v})); show(rows[-1])
    else:
        sys.exit(f"stage {stage}: see the file header")
    save(stage, rows)


if __name__ == "__main__":
    main()
