#!/usr/bin/env python3
"""Unit tests for bench/paired_stats.py — hand-computed fixtures.

Run: python bench/test_paired_stats.py
"""

import unittest

import paired_stats as ps


class TestMcNemarExact(unittest.TestCase):
    def test_known_exact_p(self):
        # b=1, c=8 -> n=9 discordant. Two-sided exact:
        # 2 * (C(9,0)+C(9,1)) / 2^9 = 2 * 10/512 = 0.0390625
        base = {f"t{i}": True for i in range(1)}  # baseline_only = 1
        base["x0"] = True
        for i in range(8):
            base[f"c{i}"] = False  # condition_only = 8
        cond = {"x0": False}
        for i in range(8):
            cond[f"c{i}"] = True
        r = ps.mcnemar_exact(base, cond)
        self.assertEqual(r["baseline_only"], 1)
        self.assertEqual(r["condition_only"], 8)
        self.assertEqual(r["discordant"], 9)
        self.assertAlmostEqual(r["p_value"], 0.0390625, places=10)
        self.assertTrue(r["significant"])

    def test_no_discordant_pairs(self):
        base = {"a": True, "b": False}
        cond = {"a": True, "b": False}
        r = ps.mcnemar_exact(base, cond)
        self.assertEqual(r["discordant"], 0)
        self.assertEqual(r["p_value"], 1.0)
        self.assertFalse(r["significant"])

    def test_negative_result_not_dropped(self):
        # b=3, c=4 -> n=7. 2*sum_{i=0..3}C(7,i)/2^7 = 2*64/128 = 1.0
        base, cond = {}, {}
        for i in range(3):
            base[f"b{i}"], cond[f"b{i}"] = True, False
        for i in range(4):
            base[f"c{i}"], cond[f"c{i}"] = False, True
        r = ps.mcnemar_exact(base, cond)
        self.assertEqual(r["discordant"], 7)
        self.assertAlmostEqual(r["p_value"], 1.0, places=10)
        self.assertFalse(r["significant"])  # reported, not dropped

    def test_no_shared_tasks_errors(self):
        with self.assertRaises(ValueError):
            ps.mcnemar_exact({"a": True}, {"b": True})


class TestBootstrapCI(unittest.TestCase):
    def test_deterministic_n1(self):
        ci = ps.bootstrap_ci([0.42])
        self.assertEqual(ci["n_seeds"], 1)
        self.assertIsNone(ci["ci_low"])
        self.assertEqual(ci["note"], "deterministic, n=1")
        self.assertAlmostEqual(ci["mean"], 0.42)

    def test_ci_brackets_mean_fixed_seed(self):
        vals = [0.40, 0.50, 0.60, 0.55, 0.45]
        ci = ps.bootstrap_ci(vals)
        self.assertEqual(ci["n_seeds"], 5)
        self.assertIsNotNone(ci["ci_low"])
        self.assertLessEqual(ci["ci_low"], ci["mean"])
        self.assertGreaterEqual(ci["ci_high"], ci["mean"])
        self.assertAlmostEqual(ci["mean"], statistics_mean(vals))

    def test_ci_reproducible(self):
        vals = [0.3, 0.7, 0.5, 0.9, 0.1, 0.6]
        a = ps.bootstrap_ci(vals)
        b = ps.bootstrap_ci(vals)
        self.assertEqual((a["ci_low"], a["ci_high"]), (b["ci_low"], b["ci_high"]))


class TestCellLabel(unittest.TestCase):
    def test_refuses_blended_model(self):
        tasks = [
            {"task_id": "a", "model": "m1", "benchmark": "swebench", "condition": "x"},
            {"task_id": "b", "model": "m2", "benchmark": "swebench", "condition": "x"},
        ]
        with self.assertRaises(ValueError):
            ps.cell_label(tasks, "all")

    def test_full_cell_fields(self):
        tasks = [
            {"task_id": "a", "model": "gemma", "benchmark": "swebench",
             "condition": "spelunk", "seed": 42},
            {"task_id": "b", "model": "gemma", "benchmark": "swebench",
             "condition": "spelunk", "seed": 43},
        ]
        label = ps.cell_label(tasks, "django-only")
        self.assertEqual(label["model"], "gemma")
        self.assertEqual(label["harness"], "swebench")
        self.assertEqual(label["condition"], "spelunk")
        self.assertEqual(label["instance_filter"], "django-only")
        self.assertEqual(label["n_tasks"], 2)
        self.assertEqual(label["seeds"], [42, 43])


def statistics_mean(vals):
    return sum(vals) / len(vals)


if __name__ == "__main__":
    unittest.main(verbosity=2)
