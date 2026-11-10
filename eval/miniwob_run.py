#!/usr/bin/env python3
"""Dry-run harness: MiniWoB++ tasks driven by Claude Code, with and without agentctl.

One run = one task, one seed, one model, one arm. The official MiniWoB++ pages and
reward code are used unmodified, except that the per-episode time limit is raised
(the default 10 s is shorter than one LLM turn) and the page is seeded.

Both arms attach to the same isolated headless Chrome over CDP, so the only
difference between them is the tool surface the model sees:

  playwright    Playwright MCP (@playwright/mcp), the common baseline
  agentctl      agentctl serve, browser category only, loopback allowed
  agentctl-core same, with --role browser-core (the 12-tool lean list)
  agentctl-open same as agentctl plus browser_eval enabled, to separate a policy block from a missing capability

Success is read from the page after the agent exits: WOB_RAW_REWARD_GLOBAL > 0 and
WOB_DONE_GLOBAL true. Raw reward is used because the official reward is scaled by
elapsed time, which would punish model latency rather than task failure.

Usage: miniwob_run.py --tasks click-button,enter-text --arms playwright,agentctl \
           --models haiku --seeds 1 --out eval/results/dry-run
       miniwob_run.py --tier 1 --baseline eval/results/full-haiku --dry-run   # plan and price a run, launch nothing

Each run costs model money (see README.md, "Cost"): use --dry-run first, and --baseline to reuse the Playwright
records instead of re-running that arm.
"""
import argparse
import json
import queue
import shutil
import socket
import subprocess
import tempfile
import threading
import time
from functools import partial
from http.server import SimpleHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path

import miniwob
from playwright.sync_api import sync_playwright

ROOT = Path(__file__).resolve().parent.parent
HTML_DIR = Path(miniwob.__file__).parent / "html"
AGENTCTL = ROOT / "target" / "release" / "agentctl"
CHROME = "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome"
EPISODE_MS = 10 * 60 * 1000  # generous: the agent, not the page clock, is under test
WALL_TIMEOUT_S = 900
POLL_S = 2  # how often the page is checked for WOB_DONE_GLOBAL while claude runs
# The Playwright MCP version. It was "@latest" for the 2026-10-07 baseline, so that run's exact version is unknown
# (0.0.83 is what the npx cache of the machine that ran it holds, installed that day). Change this only together with
# re-running the playwright arm: records carry it as `playwright_mcp` so a baseline with another version is visible.
PLAYWRIGHT_MCP = "0.0.83"
ARMS = ("playwright", "agentctl", "agentctl-core", "agentctl-open")
HERE = Path(__file__).resolve().parent
TASK_FILES = {"regress": HERE / "tasks-regress.txt", "all": HERE / "tasks-all.txt"}
# Presets for --tier; explicit flags override them. Tier 0 is capability_checks.py (no model).
TIERS = {1: {"tasks": "regress", "seeds": 1, "arms": "agentctl-core"},
         2: {"tasks": "regress", "seeds": 3, "arms": "agentctl-core"},
         3: {"tasks": "all", "seeds": 3, "arms": "agentctl,agentctl-core"}}


def free_port():
    with socket.socket() as s:
        s.bind(("127.0.0.1", 0))
        return s.getsockname()[1]


class Quiet(SimpleHTTPRequestHandler):
    def log_message(self, *a):
        pass


def serve_html():
    port = free_port()
    srv = ThreadingHTTPServer(("127.0.0.1", port), partial(Quiet, directory=str(HTML_DIR)))
    threading.Thread(target=srv.serve_forever, daemon=True).start()
    return srv, port


def launch_chrome(profile):
    port = free_port()
    proc = subprocess.Popen(
        [CHROME, f"--remote-debugging-port={port}", f"--user-data-dir={profile}",
         "--headless=new", "--no-first-run", "--no-default-browser-check",
         "--window-size=1000,700", "about:blank"],
        stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    for _ in range(100):
        try:
            socket.create_connection(("127.0.0.1", port), timeout=0.2).close()
            return proc, port
        except OSError:
            time.sleep(0.1)
    proc.kill()
    raise RuntimeError("chrome did not open its debugging port")


def mcp_config(arm, cdp_port, workdir):
    if arm == "playwright":
        servers = {"playwright": {"command": "npx", "args": [
            "-y", f"@playwright/mcp@{PLAYWRIGHT_MCP}", "--cdp-endpoint", f"http://127.0.0.1:{cdp_port}"]}}
        allow = "mcp__playwright"
    elif arm in ("agentctl", "agentctl-core", "agentctl-open"):
        cfg = workdir / "agentctl.toml"
        enable = '["browser_eval"]' if arm == "agentctl-open" else "[]"  # open = same tools as Playwright's evaluate
        cfg.write_text(
            f'[policy]\ncategories = ["browser"]\nmode = "autonomous"\nenable = {enable}\n'
            f'audit_dir = "{workdir / "audit"}"\nkill_switch_file = "{workdir / "KILL"}"\n'
            '[browser]\nallow_private = true\n')
        servers = {"agentctl": {"command": str(AGENTCTL), "args": ["serve"] + (["--role", "browser-core"] if arm == "agentctl-core" else []),
                                "env": {"AGENTCTL_CONFIG": str(cfg)}}}
        allow = "mcp__agentctl"
    else:
        raise SystemExit(f"unknown arm {arm}")
    path = workdir / f"{arm}.mcp.json"
    path.write_text(json.dumps({"mcpServers": servers}))
    return path, allow


def prompt_for(arm, cdp_port, goal):
    attach = (f"A Chrome browser is already running with remote debugging on port {cdp_port} "
              "and its only tab shows the task. ")
    if arm.startswith("agentctl"):
        attach += f'Attach to it with browser_connect (attach.port={cdp_port}). '
    return (attach + "Do not navigate away from the page and do not reload it. "
            f"Task: {goal}\nWhen the task is complete, reply with the single word DONE.")


# --- pure logic: run planning and stop rules (unit-tested in test_harness.py) ---

def read_tasks(spec):
    """A task list: a preset name (regress, all), a file of names (commas or lines), or a comma list."""
    path = TASK_FILES.get(spec) or (Path(spec) if Path(spec).is_file() else None)
    text = path.read_text() if path else spec
    return [t.strip() for t in text.replace("\n", ",").split(",") if t.strip()]


def apply_tier(tier, tasks, arms, seeds):
    """Fill what the caller left unset (None) from the tier preset, then from the plain defaults."""
    p = TIERS[tier] if tier else {}
    tasks = tasks if tasks is not None else p.get("tasks")
    if tasks is None:
        raise SystemExit("give --tasks or --tier")
    return (read_tasks(tasks), arms if arms is not None else p.get("arms", "playwright,agentctl"),
            seeds if seeds is not None else p.get("seeds", 1))


def record_name(arm, model, task, seed):
    return f"{arm}__{model}__{task}__{seed}.json"


def plan_matrix(tasks, arms, models, seeds, shard, out, baseline=None):
    """The run matrix in execution order. Each entry's `action` is one of
    done (record already in out), reuse (playwright record copied from baseline),
    skip (playwright wanted from a baseline that lacks it: never run silently), run."""
    i, n = map(int, shard.split("/"))
    plan = []
    for task in tasks[i::n]:
        for seed in range(seeds):
            for model in models:
                for arm in arms:
                    name = record_name(arm, model, task, seed)
                    if (out / name).exists():  # resumable
                        action = "done"
                    elif baseline and arm == "playwright":
                        action = "reuse" if (baseline / name).exists() else "skip"
                    else:
                        action = "run"
                    plan.append({"task": task, "seed": seed, "model": model, "arm": arm, "action": action})
    return plan


def estimate_cost(plan, baseline):
    """List cost of the entries that would run, from the baseline's total_cost_usd for the same
    (arm, model, task, seed). Returns {arm: (usd, runs_priced, runs_unpriced)}."""
    est = {}
    for e in plan:
        if e["action"] != "run":
            continue
        usd, priced, unpriced = est.get(e["arm"], (0.0, 0, 0))
        f = baseline / record_name(e["arm"], e["model"], e["task"], e["seed"]) if baseline else None
        cost = ((json.loads(f.read_text()).get("claude") or {}).get("total_cost_usd") if f and f.exists() else None)
        est[e["arm"]] = (usd + cost, priced + 1, unpriced) if cost is not None else (usd, priced, unpriced + 1)
    return est


def tool_result_errors(events):
    """Yield (event_index, error_key) for every tool result in stream-json `events`, with key None
    for a success. The key is (tool, code, message) for agentctl's {"error": {...}} replies and
    (tool, first 200 chars) for any other error text (Playwright MCP replies in prose)."""
    names = {}
    for i, ev in enumerate(events):
        content = (ev.get("message") or {}).get("content")
        if not isinstance(content, list):
            continue
        for b in content:
            if ev.get("type") == "assistant" and b.get("type") == "tool_use":
                names[b.get("id")] = b.get("name")
            elif ev.get("type") == "user" and b.get("type") == "tool_result":
                if not b.get("is_error"):
                    yield i, None
                    continue
                c = b.get("content")
                text = c if isinstance(c, str) else " ".join(x.get("text", "") for x in c or [] if isinstance(x, dict))
                tool = names.get(b.get("tool_use_id"), "?")
                try:
                    err = json.loads(text)["error"]
                    key = (tool, err.get("code"), err.get("message"))
                except (ValueError, KeyError, TypeError, AttributeError):
                    key = (tool, text.strip()[:200])
                yield i, key


def find_repeat_error(events, n):
    """The first point where the same tool returned the same error n times in a row (any success or
    different error in between resets the count). Returns {"index", "key", "count"} or None. n <= 0: off."""
    if n <= 0:
        return None
    last, run = None, 0
    for i, key in tool_result_errors(events):
        run = run + 1 if key is not None and key == last else (1 if key is not None else 0)
        last = key
        if run >= n:
            return {"index": i, "key": key, "count": run}
    return None


def count_turns(events, upto=None):
    """Assistant turns (distinct message ids) in `events[:upto]`."""
    return len({(ev.get("message") or {}).get("id") for ev in events[:upto] if ev.get("type") == "assistant"})


def parse_events(text):
    out = []
    for line in text.splitlines():
        try:
            out.append(json.loads(line))
        except json.JSONDecodeError:
            pass
    return out


def supervise(proc, page, repeat_error_stop):
    """Read claude's stdout as it arrives and watch the page. Stops claude when the episode is done
    (WOB_DONE_GLOBAL) or on a repeated tool error, or after WALL_TIMEOUT_S. Playwright's sync API is not
    thread-safe: reader threads only fill a queue, the page is touched from this thread alone.
    Returns (stdout, stderr, stopped_early, timed_out)."""
    q = queue.Queue()

    def pump(stream, tag):
        for line in stream:
            q.put((tag, line))
        q.put((tag, None))

    for stream, tag in ((proc.stdout, "out"), (proc.stderr, "err")):
        threading.Thread(target=pump, args=(stream, tag), daemon=True).start()
    lines = {"out": [], "err": []}
    events, open_streams, stopped, timed_out = [], 2, None, False
    deadline, next_poll = time.time() + WALL_TIMEOUT_S, time.time() + POLL_S
    while open_streams and not stopped and not timed_out:
        try:
            tag, line = q.get(timeout=0.25)
            if line is None:
                open_streams -= 1
            else:
                lines[tag].append(line)
                if tag == "out" and repeat_error_stop:
                    events.extend(parse_events(line))
                    if find_repeat_error(events, repeat_error_stop):
                        stopped = "repeat_error"
        except queue.Empty:
            pass
        now = time.time()
        if now >= deadline:
            timed_out = True
        elif now >= next_poll and not stopped:
            next_poll = now + POLL_S
            try:
                if page.evaluate("WOB_DONE_GLOBAL"):
                    stopped = "episode_done"
            except Exception:  # page mid-navigation or crashed: the final scoring below reports it
                pass
    if stopped or timed_out:
        proc.terminate()
        try:
            proc.wait(10)
        except subprocess.TimeoutExpired:
            proc.kill()
    proc.wait()
    while True:  # what the reader threads still hold
        try:
            tag, line = q.get(timeout=1)
        except queue.Empty:
            break
        if line is not None:
            lines[tag].append(line)
    return "".join(lines["out"]), "".join(lines["err"]), stopped, timed_out


def partial_result(events):
    """Stand-in for claude's `result` event when claude was stopped before it wrote one: turn count and
    token usage summed from the assistant messages. total_cost_usd stays None (price is not recomputed)."""
    usage = {}
    seen = set()
    for ev in events:
        m = ev.get("message") or {}
        if ev.get("type") != "assistant" or m.get("id") in seen:
            continue
        seen.add(m.get("id"))
        for k in ("input_tokens", "output_tokens", "cache_creation_input_tokens", "cache_read_input_tokens"):
            usage[k] = usage.get(k, 0) + ((m.get("usage") or {}).get(k) or 0)
    return {"is_error": False, "subtype": "stopped_early", "num_turns": len(seen), "total_cost_usd": None,
            "usage": usage, "partial": True}


def run_one(task, seed, model, arm, max_turns, max_usd, trace_root, repeat_error_stop=0):
    srv, http_port = serve_html()
    work = Path(tempfile.mkdtemp(prefix="wob-"))
    chrome, cdp_port = launch_chrome(work / "profile")
    rec = {"task": task, "seed": seed, "model": model, "arm": arm, "max_turns": max_turns}
    if arm == "playwright":
        rec["playwright_mcp"] = PLAYWRIGHT_MCP
    try:
        with sync_playwright() as pw:
            browser = pw.chromium.connect_over_cdp(f"http://127.0.0.1:{cdp_port}")
            page = browser.contexts[0].pages[0]
            page.goto(f"http://127.0.0.1:{http_port}/miniwob/{task}.html")
            page.wait_for_function("typeof core !== 'undefined'")
            page.evaluate(f"core.EPISODE_MAX_TIME = {EPISODE_MS}; Math.seedrandom({json.dumps(f'{task}-{seed}')});"
                          "core.startEpisodeReal();")
            goal = page.evaluate("core.getUtterance()")
            rec["goal"] = goal

            cfg, allow = mcp_config(arm, cdp_port, work)
            cmd = ["claude", "-p", prompt_for(arm, cdp_port, goal), "--model", model,
                   "--output-format", "stream-json", "--verbose", "--strict-mcp-config", "--mcp-config", str(cfg),
                   "--setting-sources", "", "--no-session-persistence", "--tools", "",
                   "--allowedTools", allow, "--max-turns", str(max_turns),
                   "--max-budget-usd", str(max_usd), "--disable-slash-commands"]
            t0 = time.time()
            proc = subprocess.Popen(cmd, cwd=work, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
            stdout, stderr, stopped, timed_out = supervise(proc, page, repeat_error_stop)  # partial transcript kept on timeout
            if stopped:
                rec["stopped_early"] = stopped
            if timed_out:
                rec["timed_out"] = True
            rec["wall_s"] = round(time.time() - t0, 1)
            events = parse_events(stdout)
            out = {"is_error": True, "stdout": stdout[-500:], "stderr": stderr[-500:]}
            for ev in events:
                if ev.get("type") == "result":
                    out = ev
            if stopped and out.get("type") != "result":  # claude was killed before it summarised
                out = partial_result(events)
            tdir = trace_root / f"{arm}-{model}-{task}-{seed}"
            tdir.mkdir(parents=True, exist_ok=True)
            (tdir / "transcript.jsonl").write_text(stdout)
            rec["claude"] = {k: out.get(k) for k in (
                "is_error", "subtype", "num_turns", "total_cost_usd", "usage", "modelUsage",
                "duration_ms", "duration_api_ms", "terminal_reason", "permission_denials", "result")}
            if out.get("is_error") and "stderr" in out:
                rec["claude"].update(out)

            st = page.evaluate("({done: WOB_DONE_GLOBAL, raw: WOB_RAW_REWARD_GLOBAL,"
                               " reward: WOB_REWARD_GLOBAL, reason: WOB_REASON_SAFE})"
                               .replace("WOB_REASON_SAFE", "(typeof WOB_REWARD_REASON === 'undefined' ? null : WOB_REWARD_REASON)"))
            rec["state"] = st
            rec["success"] = bool(st["done"] and st["raw"] > 0)
            browser.close()
    except Exception as e:  # keep the run record, never a silent skip
        rec["error"] = repr(e)
        rec["success"] = False
    finally:
        chrome.kill()
        srv.shutdown()
        audit = work / "audit"
        if audit.exists():
            rec["audit_files"] = [str(f) for f in audit.glob("*")]
            keep = trace_root / f"{arm}-{model}-{task}-{seed}"
            shutil.copytree(audit, keep, dirs_exist_ok=True)
        shutil.rmtree(work, ignore_errors=True)
    return rec


def print_plan(plan, baseline):
    arms = sorted({e["arm"] for e in plan})
    est = estimate_cost(plan, baseline)
    print(f"{'arm':14}{'run':>6}{'reuse':>7}{'skip':>6}{'done':>6}   est. list cost (baseline records)")
    for arm in arms:
        n = {a: sum(1 for e in plan if e["arm"] == arm and e["action"] == a) for a in ("run", "reuse", "skip", "done")}
        usd, priced, unpriced = est.get(arm, (0.0, 0, 0))
        cost = f"${usd:.2f} for {priced} runs, {unpriced} without a baseline cost" if baseline else "no --baseline: no estimate"
        print(f"{arm:14}{n['run']:>6}{n['reuse']:>7}{n['skip']:>6}{n['done']:>6}   {cost}")
    total = sum(v[0] for v in est.values())
    print(f"total to run: {sum(1 for e in plan if e['action'] == 'run')} runs" + (f", about ${total:.2f} at list price" if baseline else ""))


def main():
    ap = argparse.ArgumentParser(epilog="Presets (--tier): 1 = regression tasks, 1 seed, agentctl-core; "
                                 "2 = same, 3 seeds; 3 = all tasks, 3 seeds, agentctl and agentctl-core. "
                                 "Explicit flags override a preset.")
    ap.add_argument("--tier", type=int, choices=sorted(TIERS), help="preset for --tasks, --seeds and --arms")
    ap.add_argument("--tasks", help="comma list, a file of names, or a preset name (regress, all)")
    ap.add_argument("--arms", help=f"comma list of {', '.join(ARMS)} (default playwright,agentctl)")
    ap.add_argument("--models", default="haiku")
    ap.add_argument("--seeds", type=int, help="seeds 0..n-1 (default 1)")
    ap.add_argument("--max-turns", type=int, default=25)
    ap.add_argument("--max-usd", type=float, default=0.15, help="per-run budget cap (the median run costs about $0.04)")
    ap.add_argument("--repeat-error-stop", type=int, default=0, metavar="N",
                    help="end a run as failed after N identical consecutive tool errors (0 = off)")
    ap.add_argument("--baseline", help="dir of earlier records: the playwright arm is copied from it, never re-run")
    ap.add_argument("--dry-run", action="store_true", help="print the run matrix and its estimated cost, launch nothing")
    ap.add_argument("--shard", default="0/1", help="i/n: run every n-th task, so n processes can share one --out")
    ap.add_argument("--out", default=str(ROOT / "eval/results/dry-run"))
    a = ap.parse_args()
    tasks, arms, seeds = apply_tier(a.tier, a.tasks, a.arms, a.seeds)
    arms = arms.split(",")
    for arm in arms:
        if arm not in ARMS:
            raise SystemExit(f"unknown arm {arm}")
    out = Path(a.out)
    baseline = Path(a.baseline) if a.baseline else None
    plan = plan_matrix(tasks, arms, a.models.split(","), seeds, a.shard, out, baseline)
    print_plan(plan, baseline)
    if a.dry_run:
        return
    out.mkdir(parents=True, exist_ok=True)
    for e in plan:
        task, seed, model, arm = e["task"], e["seed"], e["model"], e["arm"]
        f = out / record_name(arm, model, task, seed)
        if e["action"] == "reuse":
            src = baseline / f.name
            f.write_text(json.dumps({**json.loads(src.read_text()), "baseline_from": str(src)}, indent=1))
            print(f"{arm:10} {model:7} {task:22} seed={seed} reused from {src}", flush=True)
        elif e["action"] == "skip":
            print(f"NOTICE {arm} {model} {task} seed={seed}: no record in baseline {baseline}, skipped (not run)", flush=True)
        elif e["action"] == "run":
            for attempt in range(4):
                r = run_one(task, seed, model, arm, a.max_turns, a.max_usd, out / "traces", a.repeat_error_stop)
                c = r.get("claude") or {}
                blob = " ".join(str(c.get(k) or "") for k in ("result", "stdout", "stderr", "subtype", "terminal_reason")).lower()
                # a usage-limit or overload error is infrastructure, not a task failure
                if c.get("is_error") and any(k in blob for k in ("rate limit", "usage limit", "rate_limit", "overloaded", "http 429", "http 529", "status 429", "status 529")):
                    print(f"infra error on {arm} {model} {task} {seed}, retry in 120s", flush=True)
                    time.sleep(120)
                    continue
                break
            f.write_text(json.dumps(r, indent=1))
            print(f"{arm:10} {model:7} {task:22} seed={seed} success={r['success']} "
                  f"turns={c.get('num_turns')} usd={c.get('total_cost_usd')} stopped={r.get('stopped_early')} err={r.get('error')}",
                  flush=True)


if __name__ == "__main__":
    main()
