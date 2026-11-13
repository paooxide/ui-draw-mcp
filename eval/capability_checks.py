#!/usr/bin/env python3
"""Tier 0: scripted capability checks. No model, no network beyond loopback, $0.

Each check drives the local `agentctl serve` over stdio JSON-RPC with a fixed call sequence against a seeded
MiniWoB++ page in one headless Chrome, then asserts on the page (reward, DOM state). It proves a capability
exists and works; the model tiers measure whether a model finds and uses it. Each check names the capability
it proves and the MiniWoB++ task it uses.

Usage: capability_checks.py [check ...]     (no names = all; exit code 1 if any FAIL)
Needs target/release/agentctl (cargo build --release -p agentctl).
"""
import json
import os
import re
import select
import shutil
import subprocess
import sys
import tempfile
import time
from pathlib import Path

import miniwob_run as m
from playwright.sync_api import sync_playwright

CHECKS = {}


def check(name, req, task):
    def reg(fn):
        CHECKS[name] = (req, task, fn)
        return fn
    return reg


class Fail(AssertionError):
    pass


def expect(cond, evidence):
    if not cond:
        raise Fail(evidence)


class Env:
    """One Chrome, one agentctl, one static server; `open()` loads a seeded task page."""

    def __enter__(self):
        if not m.AGENTCTL.exists():
            raise SystemExit(f"{m.AGENTCTL} missing: cargo build --release -p agentctl")
        self.srv, self.http_port = m.serve_html()
        self.work = Path(tempfile.mkdtemp(prefix="wob-chk-"))
        self.chrome, self.cdp = m.launch_chrome(self.work / "profile")
        cfg, _ = m.mcp_config("agentctl", self.cdp, self.work)
        srv = json.loads(cfg.read_text())["mcpServers"]["agentctl"]
        self.proc = subprocess.Popen([srv["command"], "serve"], stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                                     stderr=subprocess.DEVNULL, text=True, env={**os.environ, **srv["env"]})
        self.pw = sync_playwright().start()
        self.page = self.pw.chromium.connect_over_cdp(f"http://127.0.0.1:{self.cdp}").contexts[0].pages[0]
        self.rid = 0
        self.rpc("initialize", {"protocolVersion": "2024-11-05", "capabilities": {},
                                "clientInfo": {"name": "capability_checks", "version": "0"}})
        return self

    def __exit__(self, *exc):
        for stop in (self.pw.stop, self.proc.kill, self.chrome.kill, self.srv.shutdown):
            try:
                stop()
            except Exception:
                pass
        self.chrome.wait()
        shutil.rmtree(self.work, ignore_errors=True)

    def rpc(self, method, params):
        self.rid += 1
        self.proc.stdin.write(json.dumps({"jsonrpc": "2.0", "id": self.rid, "method": method, "params": params}) + "\n")
        self.proc.stdin.flush()
        if not select.select([self.proc.stdout], [], [], 60)[0]:
            raise Fail(f"agentctl did not answer {method} within 60 s")
        return json.loads(self.proc.stdout.readline())

    def tool(self, name, args):
        """Call a tool; returns the parsed {ok, data | error} reply (errors are returned, not raised)."""
        r = self.rpc("tools/call", {"name": name, "arguments": args})
        if "result" not in r:
            raise Fail(f"{name}: JSON-RPC error {r.get('error')}")
        return json.loads(r["result"]["content"][0]["text"])

    def act(self, **args):
        return self.tool("browser_act", {"target_id": self.tid, **args})

    def open(self, task, seed=0):
        """Load `task`, seeded as miniwob_run does, attach agentctl; returns the task's goal text."""
        p = self.page
        p.goto(f"http://127.0.0.1:{self.http_port}/miniwob/{task}.html")
        p.wait_for_function("typeof core !== 'undefined'")
        p.evaluate(f"core.EPISODE_MAX_TIME = {m.EPISODE_MS}; Math.seedrandom({json.dumps(f'{task}-{seed}')});"
                   "core.startEpisodeReal();")
        r = self.tool("browser_connect", {"attach": {"port": self.cdp}})
        expect(r.get("ok"), f"browser_connect failed: {r}")
        self.tid = r["data"]["target_id"]
        return p.evaluate("core.getUtterance()")

    def reward(self):
        time.sleep(0.4)  # let the page's own handlers finish
        return self.page.evaluate("({done: WOB_DONE_GLOBAL, raw: WOB_RAW_REWARD_GLOBAL})")


def expect_reward(env, what):
    st = env.reward()
    expect(st["done"] and st["raw"] > 0, f"{what}: episode state {st}")
    return f"{what}: episode done, raw reward {st['raw']}"


@check("pointer-svg", "real pointer input", "click-pie")
def pointer_svg(env):
    """Real pointer events reach SVG targets: expand the pie menu, click the slice named in the goal."""
    goal = env.open("click-pie")
    want = re.search(r'"(.+?)"', goal).group(1)
    r = env.act(action="click", query="#wheelnav-divWheel-spreader")
    expect(r.get("ok"), f"spreader click: {r}")
    time.sleep(1.5)  # the menu animates open; slices ignore pointer events before that
    idx = env.page.evaluate("(w) => [...document.querySelectorAll('[id^=wheelnav-divWheel-title-]')]"
                            ".find(t => t.textContent.trim() === w).id.split('-').pop()", want)
    env.page.evaluate("window.__ev = []; for (const t of ['mousedown', 'mouseup', 'click']) document.addEventListener("
                      "t, e => __ev.push(t + ':' + (e.target.id || e.target.tagName) + ':' + e.isTrusted), true)")
    r = env.act(action="click", query=f"#wheelnav-divWheel-slice-{idx}")
    expect(r.get("ok") and r["data"].get("input") == "cdp", f"slice click not reported as CDP input: {r}")
    ev = env.page.evaluate("__ev")
    # wheelnav acts on mouseup and removes the wheel, so a trailing click event is not required
    expect(all(f"{t}:wheelnav-divWheel-slice-{idx}:true" in ev for t in ("mousedown", "mouseup")),
           f"no trusted mousedown/mouseup on the slice, page saw {ev}")
    return expect_reward(env, f"clicked slice {idx} for '{want}' ({r['data']['input']} input, page saw {ev})")


@check("drag-box", "drag", "drag-box")
def drag_box(env):
    """drag moves a jQuery-UI draggable by a pixel offset."""
    env.open("drag-box")
    (sx, sy), (lx, ly) = (env.page.evaluate(f"(() => {{const r = document.querySelector('{q}').getBoundingClientRect();"
                                            " return [r.x, r.y]; })()") for q in ("#draggableSmall", "#draggableLarge"))
    r = env.act(action="drag", query="#draggableSmall", dx=lx + 15 - sx, dy=ly + 15 - sy)  # 15 px inside the large box
    expect(r.get("ok"), f"drag: {r}")
    env.act(action="click", query="#subbtn")
    return expect_reward(env, f"dragged by ({lx + 15 - sx:.0f},{ly + 15 - sy:.0f}), {r['data'].get('moves')} moves")


@check("sliders", "drag", "use-slider-2")
def sliders(env):
    """Drag three jQuery-UI slider handles to the values in the goal."""
    goal = env.open("use-slider-2")
    want = json.loads(re.search(r"\[.*?\]", goal).group(0))
    for i, v in enumerate(want, 1):
        rect = env.page.evaluate(f"(() => {{const r = document.querySelector('#slider-{i}').getBoundingClientRect();"
                                 " return [r.x, r.width]; })()")
        hx = env.page.evaluate(f"(() => {{const r = document.querySelector('#slider-{i} .ui-slider-handle')"
                               ".getBoundingClientRect(); return r.x + r.width / 2; })()")
        r = env.act(action="drag", query=f"#slider-{i} .ui-slider-handle", dx=rect[0] + rect[1] * v / 20 - hx, dy=0)
        expect(r.get("ok"), f"drag slider {i}: {r}")
    env.act(action="click", query="#subbtn")
    return expect_reward(env, f"sliders set to {want}")


@check("copy-paste", "key combos", "copy-paste")
def copy_paste(env):
    """ctrl+a / ctrl+c / ctrl+v with no ref or query: press goes to whatever has focus."""
    env.open("copy-paste")
    env.act(action="click", query="#to-copy")
    for combo in ("ctrl+a", "ctrl+c"):
        r = env.act(action="press", value=combo)
        expect(r.get("ok"), f"press {combo} with no target: {r}")
    env.act(action="click", query="#answer-input")
    r = env.act(action="press", value="ctrl+v")
    expect(r.get("ok"), f"press ctrl+v with no target: {r}")
    got = env.page.evaluate("document.querySelector('#answer-input').value")
    env.act(action="click", query="#subbtn")
    return expect_reward(env, f"pasted {len(got)} chars via combos with no ref")


@check("scroll-container", "container scroll", "scroll-text-2")
def scroll_container(env):
    """scroll to an edge of an inner scroll container (a textarea), not the page."""
    goal = env.open("scroll-text-2")
    edge = "bottom" if "bottom" in goal else "top"
    before = env.page.evaluate("document.getElementById('text-area').scrollTop")
    r = env.act(action="scroll", query="#text-area", value=edge)
    expect(r.get("ok"), f"scroll: {r}")
    after = env.page.evaluate("document.getElementById('text-area').scrollTop")
    env.act(action="click", query="#subbtn")
    return expect_reward(env, f"scrolled the textarea to the {edge}, scrollTop {before} -> {after}")


@check("plain-word-query", "plain-word query", "click-button")
def plain_word_query(env):
    """A bare word with no `by` falls back from CSS to visible text."""
    goal = env.open("click-button")
    word = re.search(r'"(.+?)"', goal).group(1)
    r = env.act(action="click", query=word)
    expect(r.get("ok") and r["data"].get("matched_by") == "text", f"click query={word!r}: {r}")
    return expect_reward(env, f"query {word!r} matched_by {r['data']['matched_by']}")


@check("plain-word-email", "plain-word query", "email-inbox")
def plain_word_email(env):
    """The proof task: the sender's name is also in the instruction text; a plain-word click must open the inbox row."""
    goal = env.open("email-inbox")
    sender, rcpt = re.search(r"by (\w+) and forward that email to (\w+)", goal).groups()
    env.page.evaluate("window.__hit = null; document.addEventListener('click', e => { window.__hit = "
                      "e.target.closest('.email-thread') ? 'inbox row' : 'outside the inbox: ' + "
                      "(e.target.closest('#query') ? 'the instruction text' : e.target.tagName); }, true)")
    r = env.act(action="click", query=sender)
    hit = env.page.evaluate("__hit")
    expect(hit == "inbox row", f"click query={sender!r} (no by) landed on {hit}; matches={r.get('data', {}).get('matches')}")
    env.act(action="click", query="#email .email-forward")
    env.act(action="click", query=".forward-sender")
    env.act(action="type", value=rcpt)
    env.act(action="click", query="#send-forward")
    return expect_reward(env, f"forwarded to {rcpt}")


@check("login-user-popup", "overlay in the way", "login-user-popup")
def overlay_report(env):
    """A popup raised by focusing a field says so; fill_form does not claim a submit the popup blocked."""
    for seed in range(12):  # the task raises its popup on the username, the password or not at all, by seed
        goal = env.open("login-user-popup", seed)
        user, pw = re.findall(r'"(.+?)"', goal)[:2]
        fields = [{"selector": "#username", "value": user}, {"selector": "#password", "value": pw}]
        r = env.tool("browser_fill_form", {"target_id": env.tid, "fields": fields, "submit": {"selector": "#subbtn"}})
        if env.page.evaluate("!!document.querySelector('#popup')"):
            break
    else:
        raise Fail("no seed in 0..11 raised the popup")
    d = r.get("data") or {}
    expect(r.get("ok") and d.get("submitted") is False and d.get("submit_blocked"), f"fill_form claimed a submit the popup blocked: {r}")
    ov = (d.get("effects") or {}).get("overlay") or {}
    expect("Cancel" in ov.get("text", "") and ov.get("ref"), f"no overlay naming the popup in fill_form: {r}")
    r = env.act(action="click", query="Cancel")
    expect(r.get("ok"), f"cancel click: {r}")
    r = env.tool("browser_fill_form", {"target_id": env.tid, "fields": fields, "submit": {"selector": "#subbtn"}})
    expect(r.get("ok") and r["data"].get("submitted") is True and "submit_blocked" not in r["data"], f"refill after cancelling: {r}")
    return expect_reward(env, f"seed {seed}: submit blocked ({d['submit_blocked']}), overlay {ov['text'][:40]!r}, then cancelled and submitted")


@check("aliases-and-teaching", "aliases, teaching errors", "click-button")
def aliases_and_teaching(env):
    """Aliases are accepted; a wrong action name lists the valid ones; a near-miss query lists similar elements."""
    goal = env.open("click-button")
    word = re.search(r'"(.+?)"', goal).group(1)
    r = env.act(action="dblclick", query=word)
    expect(r.get("ok") and r["data"].get("action") == "double_click", f"alias dblclick: {r}")
    r = env.act(action="bogus_action", query=word)
    msg = (r.get("error") or {}).get("message", "")
    expect(not r.get("ok") and "double_click" in msg and "drag" in msg, f"unknown action did not list the valid set: {r}")
    env.open("click-button")
    r = env.act(action="click", query=f"button:contains('{word}')")
    expect(r.get("ok") and r["data"].get("matches") == 1, f":contains() selector not understood: {r}")
    env.open("click-button")
    r = env.act(action="click", query=f"{word} buttn")
    msg2 = (r.get("error") or {}).get("message", "")
    expect((r.get("error") or {}).get("code") == "NOT_FOUND" and f'button "{word}"' in msg2, f"miss did not name the near element: {r}")
    return f"dblclick -> double_click; unknown action lists the valid set; :contains() works; near-miss says '...{msg2[-60:]}'"


def run(names):
    results = []
    with Env() as env:
        for name in names:
            req, task, fn = CHECKS[name]
            try:
                evidence, ok = fn(env), True
            except Exception as e:  # a failed assertion or an unexpected error both fail the check
                evidence, ok = f"{type(e).__name__}: {e}", False
            print(f"{'PASS' if ok else 'FAIL'}  {name:22} {req:24} [{task}] {evidence}", flush=True)
            results.append(ok)
    return results


if __name__ == "__main__":
    names = sys.argv[1:] or list(CHECKS)
    bad = [n for n in names if n not in CHECKS]
    if bad:
        raise SystemExit(f"unknown check {bad}; have {list(CHECKS)}")
    res = run(names)
    print(f"{sum(res)}/{len(res)} passed")
    sys.exit(0 if all(res) else 1)
