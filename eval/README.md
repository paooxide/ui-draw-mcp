# Evaluation harness

`miniwob_run.py` runs [MiniWoB++](https://github.com/Farama-Foundation/miniwob-plusplus) tasks through
Claude Code twice: once with Playwright MCP as the browser tool, once with agentctl (browser category only).
Both arms attach to the same isolated headless Chrome, so the tool surface is the only difference. The
official pages and reward code are used unmodified, except that the episode time limit is raised and the
page is seeded. See the script's docstring for usage; it is resumable and writes one JSON per run.

## Cost: run the cheap tier first

Every model run bills the Claude account that `claude` is logged in to. The full 130-task, 3-seed, 3-arm
Haiku 4.5 run (1,170 runs) cost about $75 at list price. So the harness has tiers; use the lowest one that
answers the question.

| Tier | Use | What | List cost |
|---|---|---|---|
| 0 | every change | `capability_checks.py`: scripted, no model | $0 |
| 1 | before a merge | `--tier 1`: the 48 tasks in `tasks-regress.txt`, 1 seed, `agentctl-core` | about $4 |
| 2 | release gate | `--tier 2`: same tasks, 3 seeds | about $12 |
| 3 | before a published claim | `--tier 3`: all 130 tasks, 3 seeds, `agentctl` and `agentctl-core` | about $55 |

Costs come from the records of the full run above. `--tier` only sets defaults; `--tasks`, `--arms` and
`--seeds` override it. Always check the plan first:

    .venv/bin/python miniwob_run.py --tier 1 --baseline results/full-haiku --dry-run

`--dry-run` prints runs per arm (to run, reused, skipped, already done) and an estimated list cost taken from
`total_cost_usd` of the same task, seed and arm in the `--baseline` directory, then exits without launching anything.

`--baseline DIR` reuses earlier Playwright records instead of re-running that arm: the matching
`playwright__<model>__<task>__<seed>.json` is copied into `--out` with a `baseline_from` field. A task and seed
with no record in DIR is reported and skipped, never run silently. The Playwright MCP version is pinned
(`PLAYWRIGHT_MCP` in `miniwob_run.py`) and written to each playwright record as `playwright_mcp`; re-run that arm only
when the model or that version changes. The baseline in `results/full-haiku` ran on `@latest`, so its version
is not recorded.

Other settings that bound cost:

- `--max-usd` defaults to 0.15 per run (the median run costs about $0.04). `--max-turns` stays at 25: lowering it
  changes results, because many passing runs need more than 15 turns.
- Early stop. The page is polled every 2 s; once `WOB_DONE_GLOBAL` is true the episode is over, so `claude` is
  terminated and the run is scored as before (`stopped_early: "episode_done"`). A claude process stopped
  this way never writes its summary, so its record has the turn count and token usage counted from the stream
  and `total_cost_usd: null` (`claude.partial: true`).
- `--repeat-error-stop N` (default 0, off) ends a run as failed after N identical consecutive tool errors (same tool,
  same error code and message), recorded as `stopped_early: "repeat_error"`. `replay_stops.py` replays recorded
  transcripts through the rule for N = 3, 4, 5 and prints how many runs it would stop, how many of those passed
  anyway, and the turns saved; check it before turning the rule on.

`test_harness.py` covers the tier presets, run planning and the stop rule (`.venv/bin/python -m unittest test_harness`).

## Capability checks (tier 0)

`capability_checks.py` starts `target/release/agentctl serve` against one headless Chrome and one seeded MiniWoB++ page
per check, makes a fixed sequence of tool calls, and asserts on the page (episode reward or DOM state). It prints
PASS or FAIL with the evidence and exits non-zero on any FAIL. No model is involved. It shows a capability
exists; the model tiers measure whether a model finds and uses it. Name checks to run a subset.

| Check | What it proves | Task |
|---|---|---|
| `pointer-svg` | real pointer input on SVG | click-pie |
| `drag-box`, `sliders` | drag | drag-box, use-slider-2 |
| `copy-paste` | key combos, `press` with no ref | copy-paste |
| `scroll-container` | scroll an inner container | scroll-text-2 |
| `plain-word-query`, `plain-word-email` | a plain word as a query | click-button, email-inbox |
| `aliases-and-teaching` | action aliases, errors that teach | click-button |

Build the binary first (`cargo build --release -p agentctl`); the checks use whatever was built last.

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
