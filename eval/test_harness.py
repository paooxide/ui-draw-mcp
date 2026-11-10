"""Unit tests for the pure logic of miniwob_run.py. Run: .venv/bin/python -m unittest test_harness -v"""
import json
import tempfile
import unittest
from pathlib import Path

import miniwob_run as m


def use(i, name):
    return {"type": "assistant", "message": {"id": f"m{i}", "content": [{"type": "tool_use", "id": f"t{i}", "name": name}]}}


def res(i, text, err=True):
    return {"type": "user", "message": {"content": [{"type": "tool_result", "tool_use_id": f"t{i}", "is_error": err, "content": text}]}}


def agentctl_err(code, msg="x"):
    return json.dumps({"ok": False, "error": {"code": code, "message": msg}})


def transcript(*steps):
    """steps: (tool, error text or None) -> events"""
    ev = []
    for i, (tool, err) in enumerate(steps):
        ev += [use(i, tool), res(i, err or "{}", err=err is not None)]
    return ev


class Tiers(unittest.TestCase):
    def test_tier1_and_2_use_regress_core(self):
        for tier, seeds in ((1, 1), (2, 3)):
            tasks, arms, s = m.apply_tier(tier, None, None, None)
            self.assertEqual((arms, s), ("agentctl-core", seeds))
            self.assertEqual(tasks, m.read_tasks("regress"))
            self.assertEqual(len(tasks), 48)

    def test_tier3_all_tasks_two_arms(self):
        tasks, arms, s = m.apply_tier(3, None, None, None)
        self.assertEqual((len(tasks), arms, s), (130, "agentctl,agentctl-core", 3))
        self.assertTrue(set(m.read_tasks("regress")) <= set(tasks))

    def test_explicit_flags_override(self):
        self.assertEqual(m.apply_tier(1, "a,b", "playwright", 5), (["a", "b"], "playwright", 5))

    def test_no_tier_defaults(self):
        self.assertEqual(m.apply_tier(None, "a", None, None), (["a"], "playwright,agentctl", 1))
        with self.assertRaises(SystemExit):
            m.apply_tier(None, None, None, None)

    def test_task_files_tracked_next_to_harness(self):
        for p in m.TASK_FILES.values():
            self.assertEqual(p.parent, Path(m.__file__).resolve().parent)
            self.assertTrue(p.is_file())


class Matrix(unittest.TestCase):
    def setUp(self):
        self.tmp = Path(tempfile.mkdtemp())
        self.out, self.base = self.tmp / "out", self.tmp / "base"
        self.out.mkdir(), self.base.mkdir()

    def put(self, d, arm, task, seed, cost=None):
        (d / m.record_name(arm, "haiku", task, seed)).write_text(json.dumps({"claude": {"total_cost_usd": cost}}))

    def plan(self, **kw):
        a = dict(tasks=["t1", "t2", "t3"], arms=["playwright", "agentctl"], models=["haiku"], seeds=2,
                 shard="0/1", out=self.out, baseline=self.base)
        a.update(kw)
        return m.plan_matrix(**a)

    def actions(self, plan):
        return [(e["task"], e["seed"], e["arm"], e["action"]) for e in plan]

    def test_baseline_reuse_and_missing_is_skipped_not_run(self):
        self.put(self.base, "playwright", "t1", 0)
        acts = self.actions(self.plan(tasks=["t1"], seeds=2))
        self.assertIn(("t1", 0, "playwright", "reuse"), acts)
        self.assertIn(("t1", 1, "playwright", "skip"), acts)
        self.assertNotIn("run", [a[3] for a in acts if a[2] == "playwright"])

    def test_without_baseline_playwright_runs(self):
        p = self.plan(baseline=None, tasks=["t1"], seeds=1)
        self.assertEqual([e["action"] for e in p], ["run", "run"])

    def test_baseline_only_applies_to_playwright(self):
        self.put(self.base, "agentctl", "t1", 0)
        p = self.plan(tasks=["t1"], seeds=1)
        self.assertEqual([e["action"] for e in p if e["arm"] == "agentctl"], ["run"])

    def test_existing_output_is_done(self):
        self.put(self.out, "agentctl", "t1", 0)
        p = self.plan(tasks=["t1"], seeds=1)
        self.assertEqual({e["arm"]: e["action"] for e in p}, {"playwright": "skip", "agentctl": "done"})

    def test_shards_partition_the_tasks(self):
        seen = [e["task"] for i in range(2) for e in self.plan(shard=f"{i}/2", seeds=1, arms=["agentctl"])]
        self.assertEqual(sorted(seen), ["t1", "t2", "t3"])
        self.assertEqual([e["task"] for e in self.plan(shard="1/2", seeds=1, arms=["agentctl"])], ["t2"])

    def test_estimate_uses_baseline_costs(self):
        self.put(self.base, "agentctl", "t1", 0, 0.5)
        self.put(self.base, "agentctl", "t2", 0, 0.25)
        p = self.plan(tasks=["t1", "t2", "t3"], seeds=1, arms=["agentctl"])
        self.assertEqual(m.estimate_cost(p, self.base), {"agentctl": (0.75, 2, 1)})

    def test_estimate_ignores_reused_and_done(self):
        self.put(self.base, "playwright", "t1", 0, 9.0)
        p = self.plan(tasks=["t1"], seeds=1, arms=["playwright"])
        self.assertEqual(m.estimate_cost(p, self.base), {})


class RepeatError(unittest.TestCase):
    A = ("mcp__agentctl__browser_act", agentctl_err("NOT_FOUND", "element not found"))

    def test_off_when_zero(self):
        self.assertIsNone(m.find_repeat_error(transcript(*[self.A] * 10), 0))

    def test_stops_at_nth_identical_error(self):
        ev = transcript(*[self.A] * 5)
        hit = m.find_repeat_error(ev, 3)
        self.assertEqual(hit["count"], 3)
        self.assertEqual(hit["index"], 5)  # third tool_result event
        self.assertEqual(hit["key"][:2], ("mcp__agentctl__browser_act", "NOT_FOUND"))
        self.assertIsNone(m.find_repeat_error(transcript(*[self.A] * 2), 3))

    def test_success_resets(self):
        ok = ("mcp__agentctl__browser_act", None)
        self.assertIsNone(m.find_repeat_error(transcript(self.A, self.A, ok, self.A, self.A), 3))

    def test_different_code_message_or_tool_does_not_count(self):
        b = ("mcp__agentctl__browser_act", agentctl_err("INVALID_ARGS", "bad"))
        c = ("mcp__agentctl__browser_act", agentctl_err("NOT_FOUND", "other message"))
        d = ("mcp__agentctl__browser_query", self.A[1])
        self.assertIsNone(m.find_repeat_error(transcript(self.A, b, self.A, b, c, d), 3))

    def test_plain_text_errors_compare_by_text(self):
        e = ("mcp__playwright__browser_click", "### Error\nTimeoutError: Timeout 5000ms exceeded.")
        self.assertIsNotNone(m.find_repeat_error(transcript(e, e, e), 3))

    def test_count_turns(self):
        ev = transcript(self.A, self.A, self.A)
        self.assertEqual(m.count_turns(ev), 3)
        self.assertEqual(m.count_turns(ev, 2), 1)


class Partial(unittest.TestCase):
    def test_partial_result_sums_usage_once_per_message(self):
        a = {"type": "assistant", "message": {"id": "x", "usage": {"output_tokens": 5}, "content": []}}
        r = m.partial_result([a, a, {**a, "message": {"id": "y", "usage": {"output_tokens": 7}}}])
        self.assertEqual((r["num_turns"], r["usage"]["output_tokens"], r["total_cost_usd"]), (2, 12, None))


if __name__ == "__main__":
    unittest.main()
