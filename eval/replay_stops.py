#!/usr/bin/env python3
"""Replay recorded transcripts through the repeat-error stop rule, offline and free.

For N = 3, 4, 5: how many runs the rule would have stopped, how many of those PASSED in the recorded
result (each is a flip the rule would cause), and the assistant turns it would have saved.
Transcripts are streamed one at a time. Usage: replay_stops.py [--traces DIR] [--records DIR]
"""
import argparse
import json
import re
from collections import defaultdict
from pathlib import Path

from miniwob_run import ROOT, count_turns, find_repeat_error

NS = (3, 4, 5)
NAME = re.compile(r"^(agentctl-core|agentctl-open|agentctl|playwright)-([^-]+)-(.+)-(\d+)$")


def events_of(path):
    with open(path) as fh:
        for line in fh:
            try:
                yield json.loads(line)
            except json.JSONDecodeError:
                pass


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--traces", default=str(ROOT / "eval/results/traces"))
    ap.add_argument("--records", default=str(ROOT / "eval/results/full-haiku"))
    a = ap.parse_args()
    stats = {n: defaultdict(lambda: defaultdict(float)) for n in NS}  # n -> arm -> counters
    flips = {n: [] for n in NS}
    runs = defaultdict(int)
    for d in sorted(Path(a.traces).iterdir()):
        m, tr = NAME.match(d.name), d / "transcript.jsonl"
        rec_path = Path(a.records) / f"{m.group(1)}__{m.group(2)}__{m.group(3)}__{m.group(4)}.json" if m else None
        if not (m and tr.exists() and rec_path.exists()):
            continue
        arm = m.group(1)
        rec = json.loads(rec_path.read_text())
        events = list(events_of(tr))
        total = count_turns(events)
        cost = (rec.get("claude") or {}).get("total_cost_usd") or 0.0
        runs[arm] += 1
        for n in NS:
            hit = find_repeat_error(events, n)
            if not hit:
                continue
            saved = total - count_turns(events, hit["index"] + 1)
            s = stats[n][arm]
            s["stopped"] += 1
            s["turns_saved"] += saved
            s["usd_saved"] += cost * saved / total if total else 0  # pro rata by turns: an estimate
            if rec.get("success"):
                s["flips"] += 1
                flips[n].append((d.name, hit["key"][0:2], saved))
    print(f"runs replayed: {dict(runs)} (total {sum(runs.values())})\n")
    print(f"{'N':>2} {'arm':14}{'stopped':>8}{'were PASS':>10}{'turns saved':>12}{'~usd saved':>11}")
    for n in NS:
        for arm in sorted(runs) + ["all"]:
            s = stats[n][arm] if arm != "all" else {k: sum(stats[n][x][k] for x in runs) for k in ("stopped", "flips", "turns_saved", "usd_saved")}
            print(f"{n:>2} {arm:14}{int(s.get('stopped', 0)):>8}{int(s.get('flips', 0)):>10}"
                  f"{int(s.get('turns_saved', 0)):>12}{s.get('usd_saved', 0):>11.2f}")
    for n in NS:
        print(f"\nN={n} flips (recorded PASS that the rule would have cut):")
        for name, key, saved in flips[n]:
            print(f"  {name}  {key}  turns after the cut: {saved}")


if __name__ == "__main__":
    main()
