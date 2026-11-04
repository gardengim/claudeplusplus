#!/usr/bin/env python3
"""Does the installed `claude` still behave the way claude++ assumes? (stdlib; ~2 haiku requests, cents.)
Run after every Claude Code update:  python3 bench/contract.py
Exit 1 on any FAIL. The last check is the thesis itself: headless mode keeps the request small."""
import json, os, queue, subprocess, sys, tempfile, threading, time, datetime

MAX_REQUEST_TOKENS = 40_000  # interactive `claude` sends ~46k here; headless ~31k. Above this the saving is gone.
FLAGS = ["-p", "--input-format", "stream-json", "--output-format", "stream-json", "--verbose",
         "--include-partial-messages", "--permission-prompt-tool", "stdio", "--model", "haiku"]
CWD = tempfile.mkdtemp(prefix="cpp-contract-")  # $TMPDIR, so it also runs inside a sandbox
results = []


def check(name, ok, detail=""):
    results.append({"check": name, "ok": bool(ok), "detail": str(detail)})
    print(f"{'PASS' if ok else 'FAIL'}  {name}" + (f"  ({detail})" if detail else ""), flush=True)
    return ok


class Proc:
    def __init__(self, *extra):
        self.p = subprocess.Popen(["claude", *FLAGS, *extra], cwd=CWD, stdin=subprocess.PIPE,
                                  stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
        self.q = queue.Queue()
        threading.Thread(target=self._read, daemon=True).start()
        self.err = ""
        threading.Thread(target=lambda: setattr(self, "err", self.p.stderr.read()), daemon=True).start()

    def _read(self):
        for line in self.p.stdout:
            try:
                self.q.put(json.loads(line))
            except ValueError:
                pass
        self.q.put(None)

    def send(self, o):
        self.p.stdin.write(json.dumps(o) + "\n"); self.p.stdin.flush()

    def until(self, pred, secs=90):
        """Messages up to and including the first that satisfies pred; None on EOF/timeout."""
        seen, end = [], time.time() + secs
        while time.time() < end:
            try:
                m = self.q.get(timeout=max(0.1, end - time.time()))
            except queue.Empty:
                break
            if m is None:
                return None
            seen.append(m)
            if pred(m):
                return seen
        return None

    def user(self, text):
        self.send({"type": "user", "session_id": "", "parent_tool_use_id": None,
                   "message": {"role": "user", "content": text}})

    def close(self):
        try: self.p.stdin.close(); self.p.wait(10)
        except Exception: self.p.kill()


def main():
    ver = subprocess.run(["claude", "--version"], capture_output=True, text=True).stdout.strip()
    print(f"claude {ver}  (cwd {CWD})")

    # 1. The control protocol: initialize is answered, keyed by our request_id.
    a = Proc()
    a.send({"type": "control_request", "request_id": "cpp-1", "request": {"subtype": "initialize"}})
    got = a.until(lambda m: m.get("type") == "control_response", 30)
    r = got[-1]["response"] if got else {}
    check("initialize is answered with our request_id", r.get("subtype") == "success" and r.get("request_id") == "cpp-1", r.get("subtype"))
    a.send({"type": "control_request", "request_id": "cpp-2", "request": {"subtype": "get_settings"}})
    got = a.until(lambda m: m.get("type") == "control_response" and m["response"].get("request_id") == "cpp-2", 30)
    check("get_settings is answered", bool(got) and got[-1]["response"].get("subtype") == "success")

    # 2. A turn: init, streamed deltas, result with usage.
    a.user("What is 6 times 7? Reply with the number followed by the letter Z and nothing else.")
    got = a.until(lambda m: m.get("type") == "result", 90) or []
    init = next((m for m in got if m.get("type") == "system" and m.get("subtype") == "init"), {})
    check("system/init carries session_id, model, claude_code_version",
          all(init.get(k) for k in ("session_id", "model", "claude_code_version")), init.get("claude_code_version"))
    check("partial messages stream (message_start)", any(m.get("type") == "stream_event" and m["event"].get("type") == "message_start" for m in got))
    res = got[-1] if got else {}
    check("result is a success containing the answer", res.get("type") == "result" and not res.get("is_error") and "42Z" in str(res.get("result")), str(res.get("result"))[:40])
    u = res.get("usage", {})
    tokens = sum(u.get(k, 0) for k in ("input_tokens", "cache_creation_input_tokens", "cache_read_input_tokens"))
    check(f"THESIS: headless request stays under {MAX_REQUEST_TOKENS} tokens", 0 < tokens < MAX_REQUEST_TOKENS, tokens)
    sid = init.get("session_id")
    a.close()

    # The flag claude++ passes to every backend (cwd, OS and git status move into the first message: better cache reuse).
    helptext = subprocess.run(["claude", "--help"], capture_output=True, text=True).stdout
    check("--exclude-dynamic-system-prompt-sections still exists", "--exclude-dynamic-system-prompt-sections" in helptext)
    check("--autocompact still exists", "--autocompact" in helptext)

    # 3. Resume a real conversation.
    b = Proc("--resume", sid or "x")
    b.user("Reply with only the word OK.")
    got = b.until(lambda m: m.get("type") == "result", 90) or []
    check("--resume <id> continues the conversation", bool(got) and not got[-1].get("is_error"), got[-1].get("result") if got else "no result")
    b.close()

    # 4. Resume a conversation that does not exist: claude++ stops retrying on this exact message and exit.
    c = Proc("--resume", "00000000-0000-4000-8000-000000000000")
    try: code = c.p.wait(30)
    except subprocess.TimeoutExpired: code = None; c.p.kill()
    time.sleep(0.3)
    check("--resume <unknown id> exits non-zero saying 'No conversation found'",
          code not in (0, None) and "No conversation found" in c.err, f"exit {code}: {c.err.strip()[:60]}")

    ok = all(r["ok"] for r in results)
    out = os.path.join(os.path.dirname(os.path.abspath(__file__)), "results", f"{datetime.date.today()}-contract.json")
    os.makedirs(os.path.dirname(out), exist_ok=True)
    json.dump({"claude": ver, "ok": ok, "checks": results}, open(out, "w"), indent=1)
    print(("ALL PASS" if ok else "CONTRACT BROKEN") + f"  -> {out}")
    sys.exit(0 if ok else 1)


if __name__ == "__main__":
    main()
