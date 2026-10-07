#!/usr/bin/env python3
"""Dry-run harness: MiniWoB++ tasks driven by Claude Code, with and without agentctl.

One run = one task, one seed, one model, one arm. The official MiniWoB++ pages and
reward code are used unmodified, except that the per-episode time limit is raised
(the default 10 s is shorter than one LLM turn) and the page is seeded.

Both arms attach to the same isolated headless Chrome over CDP, so the only
difference between them is the tool surface the model sees:

  playwright    Playwright MCP (@playwright/mcp), the common baseline
  agentctl      agentctl serve, browser category only, loopback allowed

Success is read from the page after the agent exits: WOB_RAW_REWARD_GLOBAL > 0 and
WOB_DONE_GLOBAL true. Raw reward is used because the official reward is scaled by
elapsed time, which would punish model latency rather than task failure.

Usage: miniwob_run.py --tasks click-button,enter-text --arms playwright,agentctl \
           --models haiku --seeds 1 --out eval/results/dry-run
"""
import argparse
import json
import os
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
            "-y", "@playwright/mcp@latest", "--cdp-endpoint", f"http://127.0.0.1:{cdp_port}"]}}
        allow = "mcp__playwright"
    elif arm == "agentctl":
        cfg = workdir / "agentctl.toml"
        cfg.write_text(
            f'[policy]\ncategories = ["browser"]\nmode = "autonomous"\nenable = []\n'
            f'audit_dir = "{workdir / "audit"}"\nkill_switch_file = "{workdir / "KILL"}"\n'
            '[browser]\nallow_private = true\n')
        servers = {"agentctl": {"command": str(AGENTCTL), "args": ["serve"],
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
    if arm == "agentctl":
        attach += f'Attach to it with browser_connect (attach.port={cdp_port}). '
    return (attach + "Do not navigate away from the page and do not reload it. "
            f"Task: {goal}\nWhen the task is complete, reply with the single word DONE.")


def run_one(task, seed, model, arm, max_turns, max_usd):
    srv, http_port = serve_html()
    work = Path(tempfile.mkdtemp(prefix="wob-"))
    chrome, cdp_port = launch_chrome(work / "profile")
    rec = {"task": task, "seed": seed, "model": model, "arm": arm, "max_turns": max_turns}
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
            p = subprocess.run(cmd, cwd=work, capture_output=True, text=True, timeout=900)
            rec["wall_s"] = round(time.time() - t0, 1)
            out = {"is_error": True, "stdout": p.stdout[-500:], "stderr": p.stderr[-500:]}
            for line in p.stdout.splitlines():
                try:
                    ev = json.loads(line)
                except json.JSONDecodeError:
                    continue
                if ev.get("type") == "result":
                    out = ev
            tdir = ROOT / "eval" / "results" / "traces" / f"{arm}-{model}-{task}-{seed}"
            tdir.mkdir(parents=True, exist_ok=True)
            (tdir / "transcript.jsonl").write_text(p.stdout)
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
            keep = ROOT / "eval" / "results" / "traces" / f"{arm}-{model}-{task}-{seed}"
            shutil.copytree(audit, keep, dirs_exist_ok=True)
        shutil.rmtree(work, ignore_errors=True)
    return rec


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--tasks", required=True)
    ap.add_argument("--arms", default="playwright,agentctl")
    ap.add_argument("--models", default="haiku")
    ap.add_argument("--seeds", type=int, default=1)
    ap.add_argument("--max-turns", type=int, default=25)
    ap.add_argument("--max-usd", type=float, default=0.50)
    ap.add_argument("--out", default=str(ROOT / "eval/results/dry-run"))
    a = ap.parse_args()
    out = Path(a.out)
    out.mkdir(parents=True, exist_ok=True)
    for task in a.tasks.split(","):
        for seed in range(a.seeds):
            for model in a.models.split(","):
                for arm in a.arms.split(","):
                    f = out / f"{arm}__{model}__{task}__{seed}.json"
                    if f.exists():  # resumable
                        continue
                    r = run_one(task, seed, model, arm, a.max_turns, a.max_usd)
                    f.write_text(json.dumps(r, indent=1))
                    c = r.get("claude") or {}
                    print(f"{arm:10} {model:7} {task:22} seed={seed} success={r['success']} "
                          f"turns={c.get('num_turns')} usd={c.get('total_cost_usd')} err={r.get('error')}",
                          flush=True)


if __name__ == "__main__":
    main()
