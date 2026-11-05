#!/usr/bin/env python3
"""Three claude++ tabs, three MCP servers: every tab starting its own copy (share_mcp off) vs one copy each (on).
Stdlib, macOS, haiku. The servers are synthetic stdio MCP servers (a `ping` tool that reports the server's pid, plus
BALLAST_MB of touched memory, standing in for the 13 to 92 MB a real one costs); each logs its start and its calls.
Each tab asks the model to call all three; the log then says how many server processes were started and whether the
tabs were answered by the same process. Memory is macOS `footprint` over claude++'s whole process tree.
Usage: python3 bench/share.py [BALLAST_MB, default 40]"""
from _term import *
BALLAST = int(sys.argv[1]) if len(sys.argv) > 1 else 40
D = "/private/tmp/cppbench/share"
LOG = D + "/log.txt"
NAMES = ["alpha", "beta", "gamma"]
SERVER = r'''
import sys, json, os
name = sys.argv[1]; log = open(os.environ["LOG"], "a", buffering=1)
ballast = bytearray(int(os.environ["BALLAST_MB"]) << 20); ballast[::4096] = b"x" * len(ballast[::4096])
log.write(f"start {name} {os.getpid()}\n")
for line in sys.stdin:
    m = json.loads(line)
    if "id" not in m: continue
    meth = m["method"]
    if meth == "initialize":
        r = {"protocolVersion": m["params"]["protocolVersion"], "capabilities": {"tools": {}}, "serverInfo": {"name": name, "version": "1"}}
    elif meth == "tools/list":
        r = {"tools": [{"name": "ping", "description": "Returns the process id of this server.", "inputSchema": {"type": "object", "properties": {}}}]}
    elif meth == "tools/call":
        log.write(f"call {name} {os.getpid()}\n"); r = {"content": [{"type": "text", "text": f"{name} pid {os.getpid()}"}]}
    else:
        r = {}
    print(json.dumps({"jsonrpc": "2.0", "id": m["id"], "result": r}), flush=True)
'''
PROMPT = "Call the ping tool of the alpha, beta and gamma servers once each, then reply with the letter Z and nothing else."

def setup(share):
    os.makedirs(D + "/.claude", exist_ok=True)
    servers = {n: {"command": sys.executable, "args": ["-c", SERVER, n], "env": {"LOG": LOG, "BALLAST_MB": str(BALLAST)}} for n in NAMES}
    json.dump({"mcpServers": servers}, open(D + "/.mcp.json", "w"))
    json.dump({"enableAllProjectMcpServers": True}, open(D + "/.claude/settings.local.json", "w"))
    cfg = D + "/xdg/claude++"; os.makedirs(cfg, exist_ok=True)  # a config of its own: the real one is not touched
    json.dump({"share_mcp": "on" if share else "off", "profile": "full"}, open(cfg + "/config.json", "w"))
    open(LOG, "w").close()

def alive(pids):
    out = 0
    for p in pids:
        try: os.kill(p, 0); out += 1
        except OSError: pass
    return out

res = {}
for share in (False, True):
    setup(share)
    t = Term([CPP, "--model", "haiku", "--", "--allowedTools", "mcp__alpha", "mcp__beta", "mcp__gamma"], cwd=D, env_extra={"XDG_CONFIG_HOME": D + "/xdg"})
    t.mark = 0; t.drain(3)
    ok = []
    for i in range(3):
        if i: os.write(t.fd, b"\x14"); t.drain(1)  # ctrl+t: a new tab in the same folder
        ok.append(t.ask(PROMPT, b"Worked for"))
    t.drain(3)
    ps = tree(t.pid)
    log = [l.split() for l in open(LOG).read().splitlines()]
    starts = [l for l in log if l[0] == "start"]
    calls = [l for l in log if l[0] == "call"]
    res["on" if share else "off"] = {
        "tabs answered": sum(ok), "server processes started": len(starts), "tool calls served": len(calls),
        "distinct server pids that answered": len({(l[1], l[2]) for l in calls}),
        "processes in claude++'s tree": names(ps), "mb": footprint(ps)}
    t.close(); time.sleep(2)
    res["on" if share else "off"]["processes left after quit"] = alive(ps)
# Sharing on, claude++ killed outright: the servers' input closes with it, so they must not be left running.
setup(True)
t = Term([CPP, "--model", "haiku", "--", "--allowedTools", "mcp__alpha", "mcp__beta", "mcp__gamma"], cwd=D, env_extra={"XDG_CONFIG_HOME": D + "/xdg"})
t.mark = 0; t.drain(3); t.ask(PROMPT, b"Worked for"); ps = tree(t.pid)
os.kill(t.pid, signal.SIGKILL); time.sleep(4)
res["on, claude++ killed (SIGKILL)"] = {"processes in the tree": len(ps), "left after 4 s": alive(ps[1:])}
print(json.dumps(res, indent=1))
os.makedirs("bench/results", exist_ok=True)
json.dump(res, open("bench/results/" + time.strftime("%Y-%m-%d") + "-share.json", "w"), indent=1)
off, on = res["off"], res["on"]
bad = []
if on["server processes started"] != len(NAMES): bad.append(f"on: {on['server processes started']} servers started, want {len(NAMES)}")
if off["server processes started"] != 3 * len(NAMES): bad.append(f"off: {off['server processes started']} servers started, want {3 * len(NAMES)}")
if on["distinct server pids that answered"] != len(NAMES): bad.append("on: tabs were not answered by the same processes")
if on["tool calls served"] < 3 * len(NAMES): bad.append(f"on: only {on['tool calls served']} calls served")
if on["tabs answered"] != 3 or off["tabs answered"] != 3: bad.append("a tab did not answer")
if on["processes left after quit"] or off["processes left after quit"]: bad.append("a process outlived claude++")
if res["on, claude++ killed (SIGKILL)"]["left after 4 s"]: bad.append("after SIGKILL, processes were left running")
print("SHARING WORKS: %d fewer processes, %.0f MB less for 3 tabs" % (sum(off["processes in claude++'s tree"].values()) - sum(on["processes in claude++'s tree"].values()), off["mb"] - on["mb"]) if not bad else "PROBLEM: " + "; ".join(bad))
sys.exit(1 if bad else 0)
