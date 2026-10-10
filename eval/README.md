# Evaluation harness

`miniwob_run.py` runs [MiniWoB++](https://github.com/Farama-Foundation/miniwob-plusplus) tasks through
Claude Code twice: once with Playwright MCP as the browser tool, once with agentctl (browser category only).
Both arms attach to the same isolated headless Chrome, so the tool surface is the only difference. The
official pages and reward code are used unmodified, except that the episode time limit is raised and the
page is seeded. See the script's docstring for usage; it is resumable and writes one JSON per run.

`repro_click.py` reproduces a single agentctl call against a task page, for chasing a failure out of a run.

The runner uses `target/release/agentctl`: rebuild it (`make release`) before a run, or the run measures
whatever was built last.

## Dry run, 2026-10-07 (harness check, not a result)

Haiku 4.5, 8 easy tasks picked by hand, 3 seeds, 24 runs per arm, against agentctl 0.2.0.

| | Playwright MCP | agentctl |
|---|---|---|
| Pass | 23/24 | 19/24 |
| Turns per run | 5.5 | 8.8 |
| List-price cost per run | $0.0245 | $0.0368 |
| Wall time per run | 13.5 s | 17.2 s |

Not publishable: one small model, three seeds, a hand-picked easy subset, and a 4-run pass gap that 24 runs
cannot separate from noise. The token and cost gap was consistent across tasks.

What the agentctl failures led to, all in 0.2.1: `by: "text"` clicked the instruction sentence instead of
the button it named (now ranked: exact, visible, clickable first), `browser_act` results did not say what was
hit (now `target` and `matches`), and a whole-page screenshot reported `0x0`. The other failures were the
model passing `browser_id` for `target_id` and copying XPath refs with escaped quotes.

Run records, transcripts and audit logs go to `results/`, which is not committed.
