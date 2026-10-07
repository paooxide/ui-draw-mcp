"""Repro: agentctl browser_act click by text on a MiniWoB click-button page (seed 1)."""
import json, subprocess, sys, tempfile, time
from pathlib import Path
sys.path.insert(0, str(Path(__file__).parent))
import miniwob_run as m
from playwright.sync_api import sync_playwright

task, seed = sys.argv[1], int(sys.argv[2])
srv, hp = m.serve_html(); work = Path(tempfile.mkdtemp()); chrome, cp = m.launch_chrome(work/"profile")
cfg, _ = m.mcp_config("agentctl", cp, work)
env = json.loads(cfg.read_text())["mcpServers"]["agentctl"]
import os
p = subprocess.Popen([env["command"], "serve"], stdin=subprocess.PIPE, stdout=subprocess.PIPE, text=True,
                     env={**os.environ, **env["env"]})
rid = 0
def call(method, params):
    global rid; rid += 1
    p.stdin.write(json.dumps({"jsonrpc":"2.0","id":rid,"method":method,"params":params})+"\n"); p.stdin.flush()
    return json.loads(p.stdout.readline())
def tool(n, a):
    r = call("tools/call", {"name": n, "arguments": a}); return json.loads(r["result"]["content"][0]["text"])
call("initialize", {"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"repro","version":"0"}})
with sync_playwright() as pw:
    page = pw.chromium.connect_over_cdp(f"http://127.0.0.1:{cp}").contexts[0].pages[0]
    page.goto(f"http://127.0.0.1:{hp}/miniwob/{task}.html"); page.wait_for_function("typeof core!=='undefined'")
    page.evaluate(f"core.EPISODE_MAX_TIME=600000; Math.seedrandom('{task}-{seed}'); core.startEpisodeReal();")
    print("goal:", page.evaluate("core.getUtterance()"))
    print(tool("browser_connect", {"attach": {"port": cp}})["ok"])
    tid = tool("browser_tabs", {"browser_id": 1, "action": "list"})["data"]["tabs"][0]["target_id"]
    page.evaluate("window.__hits=[];document.addEventListener('mousedown',e=>window.__hits.push(e.target.tagName+'#'+e.target.id+' @'+e.clientX+','+e.clientY),true)")
    q = sys.argv[3]
    print(tool("browser_act", {"target_id": tid, "action": "click", "query": q, "by": "text"}))
    time.sleep(0.3)
    print(page.evaluate("({done:WOB_DONE_GLOBAL,raw:WOB_RAW_REWARD_GLOBAL})"))
    print("hits:", page.evaluate("window.__hits"), "button rect:", page.evaluate("JSON.stringify(document.querySelector('button').getBoundingClientRect())"))
    print(page.evaluate("[...document.querySelectorAll('button,input,div')].map(e=>e.tagName+':'+(e.id||'')+':'+e.textContent.slice(0,20))"))
p.kill(); chrome.kill()
