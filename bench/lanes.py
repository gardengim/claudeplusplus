#!/usr/bin/env python3
"""Memory of N agents: native in-process subagents (Agent tool) vs N separate `claude` processes (haiku, macOS).
Usage: python3 bench/lanes.py solo|native|procs|leantok|heavy [N]
  native  one process, N parallel trivial subagents      procs  N separate processes, one trivial turn each
  leantok `lean` tools alone (prompt-token baseline)     heavy  `lean` + Agent, N subagents that read the repo
Footprint (macOS `footprint`) is summed over each process tree, never system-wide."""
import json, os, re, subprocess, sys, tempfile, threading, time
for _k in [k for k in os.environ if k.startswith("CLAUDE")]: del os.environ[_k]  # run as from a plain terminal, not inside a Claude Code session
WORK = tempfile.mkdtemp(prefix="cpp-lanes-")  # empty cwd: no ancestor CLAUDE.md/AGENTS.md leaks in
SRC = os.path.abspath("src")
def tree_pids(root):
    out = subprocess.run(["ps","-A","-o","pid=,ppid="],capture_output=True,text=True).stdout
    kids={}
    for l in out.splitlines():
        p,pp=map(int,l.split()); kids.setdefault(pp,[]).append(p)
    res,st=[],[root]
    while st:
        p=st.pop(); res.append(p); st+=kids.get(p,[])
    return res
def fp(pids):
    a=["footprint"]+[x for p in pids for x in ("-p",str(p))]
    o=subprocess.run(a,capture_output=True,text=True).stdout
    return sum(float(m[2])*{"KB":1/1024,"MB":1,"GB":1024}[m[3]] for m in re.finditer(r"\[(\d+)\]: 64-bit\s+Footprint:\s+([\d.]+)\s+(KB|MB|GB)",o))
class Sess:
    def __init__(s, flags):
        s.p=subprocess.Popen(["claude","-p","--input-format","stream-json","--output-format","stream-json","--verbose",
          "--permission-prompt-tool","stdio","--model","haiku","--no-session-persistence"]+flags,
          stdin=subprocess.PIPE,stdout=subprocess.PIPE,stderr=subprocess.DEVNULL,text=True,cwd=WORK)
        s.send({"type":"control_request","request_id":"i","request":{"subtype":"initialize"}})
        s.peak=0; s.stop=threading.Event(); s.result=None; s.agents=0
        threading.Thread(target=s.mon,daemon=True).start()
    def send(s,o): s.p.stdin.write(json.dumps(o)+"\n"); s.p.stdin.flush()
    def mon(s):
        while not s.stop.is_set():
            try: s.peak=max(s.peak,fp(tree_pids(s.p.pid)))
            except Exception: pass
            s.stop.wait(0.5)
    def turn(s,prompt):
        s.send({"type":"user","message":{"role":"user","content":prompt}})
        for line in s.p.stdout:
            o=json.loads(line); t=o.get("type")
            if t=="control_request" and o["request"].get("subtype")=="can_use_tool":
                s.send({"type":"control_response","response":{"subtype":"success","request_id":o["request_id"],"response":{"behavior":"allow","updatedInput":o["request"]["input"]}}})
            elif t=="assistant":
                s.agents+=sum(1 for b in o["message"]["content"] if b.get("type")=="tool_use" and b.get("name") in("Agent","Task"))
            elif t=="result": s.result=o; return o
    def fp(s): return fp(tree_pids(s.p.pid))
    def end(s):
        s.stop.set(); s.p.stdin.close()
        try: s.p.wait(5)
        except Exception: s.p.kill()
N=int(sys.argv[2]) if len(sys.argv)>2 else 4
arm=sys.argv[1]
if arm=="solo":
    s=Sess([]); s.turn("Reply with exactly: OK"); print(json.dumps({"arm":arm,"after_mb":round(s.fp()),"peak_mb":round(s.peak)})); s.end()
elif arm=="native":
    s=Sess([]); s.turn("Reply with exactly: OK"); base=s.fp()
    r=s.turn(f"Launch exactly {N} Agent subagents IN PARALLEL (all {N} tool calls in one message). Each subagent's prompt: 'Reply with exactly OK'. When all return, reply DONE.")
    print(json.dumps({"arm":arm,"n":N,"agents_launched":s.agents,"base_mb":round(base),"after_mb":round(s.fp()),"peak_mb":round(s.peak),"cost":r.get("total_cost_usd")})); s.end()
elif arm in("heavy","leantok"):
    LEAN="Bash,Read,Edit,Write,AskUserQuestion,WebFetch,WebSearch"
    fl=["--tools",LEAN+(",Agent" if arm=="heavy" else ""),"--setting-sources","project,local","--strict-mcp-config"]
    s=Sess(fl); r0=s.turn("Reply with exactly: OK"); base=s.fp(); u=r0["usage"]
    tok=u["input_tokens"]+u.get("cache_read_input_tokens",0)+u.get("cache_creation_input_tokens",0)
    out={"arm":arm,"base_mb":round(base),"prompt_tokens":tok}
    if arm=="heavy":
        r=s.turn(f"Launch exactly {N} Agent subagents IN PARALLEL in one message. Each: 'Read every .rs file under {SRC} with Bash cat and report the total line count of the 3 largest.' Then reply DONE.")
        out.update(agents_launched=s.agents,after_mb=round(s.fp()),peak_mb=round(s.peak),cost=r.get("total_cost_usd"))
    print(json.dumps(out)); s.end()
elif arm=="procs":
    ss=[Sess([]) for _ in range(N)]
    ts=[threading.Thread(target=x.turn,args=("Reply with exactly: OK",)) for x in ss]
    [t.start() for t in ts]; [t.join() for t in ts]
    print(json.dumps({"arm":arm,"n":N,"sum_after_mb":round(sum(x.fp() for x in ss)),"sum_peaks_mb":round(sum(x.peak for x in ss))}))
    [x.end() for x in ss]
