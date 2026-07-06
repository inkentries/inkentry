#!/usr/bin/env python3
"""Offline unit tests for build_filtered_tasks.py.

No network / no HuggingFace download: load_verified is monkeypatched to return a
synthetic instance-id set, and --labels / --overlap-with are synthetic tmp files.
Stdlib unittest only (no pytest / datasets dependency).

    python -m unittest bench.agents.test_build_filtered_tasks   # from repo root
    python bench/agents/test_build_filtered_tasks.py            # direct
"""
from __future__ import annotations

import importlib.util
import io
import json
import unittest
from contextlib import redirect_stderr, redirect_stdout
from pathlib import Path
from tempfile import TemporaryDirectory

_HERE = Path(__file__).resolve().parent
_spec = importlib.util.spec_from_file_location(
    "build_filtered_tasks", _HERE / "build_filtered_tasks.py"
)
bft = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(bft)


def _write(dir_: Path, name: str, obj) -> Path:
    p = dir_ / name
    p.write_text(json.dumps(obj))
    return p


class ParseExcludeTest(unittest.TestCase):
    """All three label formats parse to the same exclude set."""

    def setUp(self):
        self._tmp = TemporaryDirectory()
        self.d = Path(self._tmp.name)
        self.expected = {"a__x-1", "b__y-2", "c__z-3"}

    def tearDown(self):
        self._tmp.cleanup()

    def test_flat_list(self):
        p = _write(self.d, "flat.json", sorted(self.expected))
        self.assertEqual(bft.parse_exclude(p), self.expected)

    def test_id_reason_map(self):
        p = _write(self.d, "map.json", {i: "solution_leakage" for i in self.expected})
        self.assertEqual(bft.parse_exclude(p), self.expected)

    def test_exclude_object(self):
        p = _write(
            self.d,
            "obj.json",
            {"exclude": sorted(self.expected), "source": "arXiv:2410.06992"},
        )
        self.assertEqual(bft.parse_exclude(p), self.expected)

    def test_all_three_agree(self):
        a = bft.parse_exclude(_write(self.d, "a.json", sorted(self.expected)))
        b = bft.parse_exclude(
            _write(self.d, "b.json", {i: "r" for i in self.expected})
        )
        c = bft.parse_exclude(
            _write(self.d, "c.json", {"exclude": sorted(self.expected)})
        )
        self.assertEqual(a, b)
        self.assertEqual(b, c)

    def test_unrecognised_shape_raises(self):
        p = _write(self.d, "bad.json", 42)
        with self.assertRaises(ValueError):
            bft.parse_exclude(p)


class _RunMixin(unittest.TestCase):
    """Runs main() with load_verified stubbed and captured std streams."""

    def setUp(self):
        self._tmp = TemporaryDirectory()
        self.d = Path(self._tmp.name)
        self._orig_load = bft.load_verified

    def tearDown(self):
        bft.load_verified = self._orig_load
        self._tmp.cleanup()

    def run_main(self, verified: set[str], exclude, argv_extra=None, overlap=None):
        bft.load_verified = lambda dataset, revision: set(verified)
        labels = _write(self.d, "labels.json", exclude)
        out = self.d / "tasks_filtered.json"
        argv = ["--labels", str(labels), "--out", str(out)]
        if overlap is not None:
            ov = _write(self.d, "overlap.json", overlap)
            argv += ["--overlap-with", str(ov)]
        else:
            # point at a nonexistent file so the real tasks_50.json is not read
            argv += ["--overlap-with", str(self.d / "no_overlap.json")]
        if argv_extra:
            argv += argv_extra
        so, se = io.StringIO(), io.StringIO()
        with redirect_stdout(so), redirect_stderr(se):
            rc = bft.main(argv)
        return rc, so.getvalue(), se.getvalue(), out


class IntersectionTest(_RunMixin):
    def test_survivors_are_verified_minus_exclude(self):
        verified = {f"p__r-{i}" for i in range(200)}
        exclude = [f"p__r-{i}" for i in range(50)]  # first 50 excluded
        rc, _out, _err, outpath = self.run_main(verified, exclude)
        self.assertEqual(rc, 0)
        written = json.loads(outpath.read_text())
        kept = set(written["instance_ids"])
        self.assertEqual(kept, verified - set(exclude))
        # excluded ids are gone
        self.assertTrue(kept.isdisjoint(set(exclude)))
        # kept is sorted
        self.assertEqual(written["instance_ids"], sorted(written["instance_ids"]))

    def test_stray_labels_not_in_verified_dont_remove_anything(self):
        verified = {f"p__r-{i}" for i in range(160)}
        exclude = ["p__r-0", "ghost__x-999"]  # ghost not in verified
        rc, _out, err, outpath = self.run_main(verified, exclude)
        self.assertEqual(rc, 0)
        kept = set(json.loads(outpath.read_text())["instance_ids"])
        self.assertEqual(kept, verified - {"p__r-0"})
        self.assertIn("not in Verified", err)

    def test_dry_run_writes_nothing(self):
        verified = {f"p__r-{i}" for i in range(200)}
        rc, _out, err, outpath = self.run_main(
            verified, ["p__r-0"], argv_extra=["--dry-run"]
        )
        self.assertEqual(rc, 0)
        self.assertFalse(outpath.exists())
        self.assertIn("dry-run", err)


class RangeWarningTest(_RunMixin):
    WARN = "WARNING:"

    def _err_for(self, n_verified, n_exclude):
        verified = {f"p__r-{i}" for i in range(n_verified)}
        exclude = [f"p__r-{i}" for i in range(n_exclude)]
        _rc, _out, err, _outpath = self.run_main(
            verified, exclude, argv_extra=["--dry-run"]
        )
        return err

    def test_warns_below_min(self):
        # 149 survivors < 150
        err = self._err_for(149, 0)
        self.assertIn(self.WARN, err)

    def test_warns_above_max(self):
        # 301 survivors > 300
        err = self._err_for(301, 0)
        self.assertIn(self.WARN, err)

    def test_no_warn_at_lower_bound(self):
        err = self._err_for(150, 0)  # exactly 150
        self.assertNotIn(self.WARN, err)

    def test_no_warn_at_upper_bound(self):
        err = self._err_for(300, 0)  # exactly 300
        self.assertNotIn(self.WARN, err)

    def test_no_warn_in_range(self):
        err = self._err_for(250, 25)  # 225 survivors
        self.assertNotIn(self.WARN, err)


class ProvenanceHeaderTest(_RunMixin):
    def test_header_has_required_keys_and_values(self):
        verified = {f"p__r-{i}" for i in range(200)}
        exclude = [f"p__r-{i}" for i in range(40)]  # 160 survive
        labels = _write(self.d, "labels.json", exclude)
        out = self.d / "tasks_filtered.json"
        bft.load_verified = lambda dataset, revision: set(verified)
        argv = [
            "--labels", str(labels),
            "--out", str(out),
            "--labels-source", "arXiv:2410.06992 repl pkg, rev deadbeef",
            "--dataset-revision", "abc123",
            "--overlap-with", str(self.d / "none.json"),
        ]
        so, se = io.StringIO(), io.StringIO()
        with redirect_stdout(so), redirect_stderr(se):
            rc = bft.main(argv)
        self.assertEqual(rc, 0)
        h = json.loads(out.read_text())["_provenance"]
        for key in (
            "instance_filter", "generated_by", "verified_dataset",
            "verified_dataset_revision", "verified_count",
            "swebench_plus_source", "swebench_plus_ref",
            "excluded_count", "kept_count",
        ):
            self.assertIn(key, h, f"missing provenance key: {key}")
        self.assertEqual(h["instance_filter"], "swebench_plus_filtered")
        self.assertEqual(h["generated_by"], "build_filtered_tasks.py")
        self.assertEqual(h["verified_dataset"], bft.VERIFIED_DATASET)
        self.assertEqual(h["verified_dataset_revision"], "abc123")
        self.assertEqual(h["verified_count"], 200)
        self.assertEqual(h["swebench_plus_source"], "arXiv:2410.06992 repl pkg, rev deadbeef")
        self.assertEqual(h["swebench_plus_ref"], "arXiv:2410.06992")
        # excluded_count counts only labels actually in Verified
        self.assertEqual(h["excluded_count"], 40)
        self.assertEqual(h["kept_count"], 160)
        self.assertEqual(h["kept_count"], len(json.loads(out.read_text())["instance_ids"]))

    def test_excluded_count_ignores_stray_labels(self):
        verified = {f"p__r-{i}" for i in range(160)}
        exclude = ["p__r-0", "p__r-1", "ghost__a-1", "ghost__b-2"]
        _rc, _out, _err, outpath = self.run_main(verified, exclude)
        h = json.loads(outpath.read_text())["_provenance"]
        self.assertEqual(h["excluded_count"], 2)  # ghosts don't count
        self.assertEqual(h["verified_dataset_revision"], "default")  # no --dataset-revision


class OverlapTest(_RunMixin):
    def test_overlap_survivors_and_not_in_verified(self):
        verified = {f"p__r-{i}" for i in range(200)}
        exclude = ["p__r-5", "p__r-6"]
        # tasks_50-like slice: some survive, some excluded, some not in Verified at all
        slice_ids = ["p__r-5", "p__r-10", "p__r-11", "ghost__x-1", "ghost__x-2"]
        _rc, _out, _err, outpath = self.run_main(
            verified, exclude, overlap=slice_ids
        )
        ov = json.loads(outpath.read_text())["_provenance"]["overlap"]
        self.assertEqual(ov["total"], 5)
        # p__r-5 excluded -> not a survivor; p__r-10, p__r-11 survive; ghosts not in verified
        self.assertEqual(sorted(ov["survivors"]), ["p__r-10", "p__r-11"])
        self.assertEqual(sorted(ov["not_in_verified"]), ["ghost__x-1", "ghost__x-2"])
        self.assertEqual(ov["against"], "overlap.json")

    def test_no_overlap_key_when_file_absent(self):
        verified = {f"p__r-{i}" for i in range(160)}
        _rc, _out, _err, outpath = self.run_main(verified, ["p__r-0"])
        h = json.loads(outpath.read_text())["_provenance"]
        self.assertNotIn("overlap", h)


if __name__ == "__main__":
    unittest.main()
