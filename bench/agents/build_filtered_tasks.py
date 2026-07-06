#!/usr/bin/env python3
"""Build a leakage-filtered SWE-bench instance list (SWE-Bench+ intersection).

Intersects SWE-bench Verified with a SWE-Bench+ leakage/suspicious label set
(arXiv 2410.06992) and writes the surviving instance_ids, plus a provenance
header, to bench/agents/tasks_filtered.json.

Reporting rule (see bench/agents/README.md): filtered and unfiltered Track-B
numbers are ALWAYS reported separately, and every published figure names its
`instance_filter` — one of {"swebench_plus_filtered", "full"}.

    instance_filter = "full"                 -> SWE-bench Verified, unfiltered
    instance_filter = "swebench_plus_filtered" -> the list this script produces

------------------------------------------------------------------------------
The SWE-Bench+ label set
------------------------------------------------------------------------------
SWE-Bench+ (arXiv 2410.06992) is not a filtered subset published as a single
machine-readable file; the paper releases a NEW post-cutoff dataset and reports
that a large fraction of SWE-bench passing patches benefit from solution
leakage (~32.67% overall) or weak tests. To reproduce a *contamination-
controlled Verified subset* we need the per-instance leakage/suspicious labels
for Verified. Supply them via --labels as a JSON file of instance_ids to
EXCLUDE, or a JSON object mapping instance_id -> label.

Accepted --labels formats (auto-detected):
  1. ["repo__name-1234", ...]                        # flat exclude list
  2. {"repo__name-1234": "solution_leakage", ...}    # id -> reason
  3. {"exclude": [...], "source": "...", ...}        # object with "exclude" key

Obtain the labels from the SWE-Bench+ authors' released artifact (paper page /
replication package) and pin the revision you used; record it via --labels-source
so it lands in the output provenance header.

------------------------------------------------------------------------------
Usage
------------------------------------------------------------------------------
    python bench/agents/build_filtered_tasks.py \
        --labels swebench_plus_verified_exclude.json \
        --labels-source "arXiv:2410.06992 replication pkg, rev <sha/date>" \
        --out bench/agents/tasks_filtered.json

    # Verified loads from the HuggingFace datasets cache / hub.
    # Override the dataset revision for reproducibility:
    python bench/agents/build_filtered_tasks.py \
        --labels labels.json --dataset-revision <hf_revision>

    # Dry run: report counts + tasks_50 overlap without writing.
    python bench/agents/build_filtered_tasks.py --labels labels.json --dry-run
"""
from __future__ import annotations

import argparse
import datetime as _dt
import json
import sys
from pathlib import Path

VERIFIED_DATASET = "princeton-nlp/SWE-bench_Verified"
FILTER_NAME = "swebench_plus_filtered"
MIN_EXPECTED = 150
MAX_EXPECTED = 300


def load_verified(dataset: str, revision: str | None) -> set[str]:
    from datasets import load_dataset  # imported lazily so --help needs no deps

    kwargs = {"split": "test"}
    if revision:
        kwargs["revision"] = revision
    ds = load_dataset(dataset, **kwargs)
    return {row["instance_id"] for row in ds}


def parse_exclude(labels_path: Path) -> set[str]:
    data = json.loads(labels_path.read_text())
    if isinstance(data, list):
        return set(data)
    if isinstance(data, dict):
        if "exclude" in data:
            return set(data["exclude"])
        return set(data.keys())  # id -> reason mapping
    raise ValueError(f"unrecognised --labels shape: {type(data).__name__}")


def main(argv: list[str] | None = None) -> int:
    ap = argparse.ArgumentParser(
        description="Intersect SWE-bench Verified with a SWE-Bench+ leakage label set.",
        formatter_class=argparse.RawDescriptionHelpFormatter,
        epilog=__doc__,
    )
    ap.add_argument("--labels", required=True, type=Path,
                    help="JSON of SWE-Bench+ instance_ids to EXCLUDE (see module docstring).")
    ap.add_argument("--labels-source", default="unspecified",
                    help="Human-readable provenance for --labels (paper rev/date); recorded in output.")
    ap.add_argument("--out", type=Path, default=Path(__file__).with_name("tasks_filtered.json"))
    ap.add_argument("--dataset", default=VERIFIED_DATASET)
    ap.add_argument("--dataset-revision", default=None,
                    help="Pin the HuggingFace dataset revision for reproducibility.")
    ap.add_argument("--overlap-with", type=Path, default=Path(__file__).with_name("tasks_50.json"),
                    help="Existing task list to report survival overlap against.")
    ap.add_argument("--dry-run", action="store_true", help="Report counts, do not write --out.")
    args = ap.parse_args(argv)

    if not args.labels.exists():
        ap.error(f"--labels not found: {args.labels}")

    verified = load_verified(args.dataset, args.dataset_revision)
    exclude = parse_exclude(args.labels)

    # Labels not present in Verified are almost always a version/split mismatch.
    stray = sorted(exclude - verified)
    kept = sorted(verified - exclude)

    header = {
        "instance_filter": FILTER_NAME,
        "generated_by": Path(__file__).name,
        "generated_utc": _dt.datetime.now(_dt.timezone.utc).isoformat(timespec="seconds"),
        "verified_dataset": args.dataset,
        "verified_dataset_revision": args.dataset_revision or "default",
        "verified_count": len(verified),
        "swebench_plus_source": args.labels_source,
        "swebench_plus_ref": "arXiv:2410.06992",
        "excluded_count": len(exclude & verified),
        "kept_count": len(kept),
    }

    print(f"Verified instances:     {len(verified)}", file=sys.stderr)
    print(f"Exclude labels:         {len(exclude)}", file=sys.stderr)
    print(f"  (not in Verified):    {len(stray)}", file=sys.stderr)
    print(f"Kept (filtered subset): {len(kept)}", file=sys.stderr)

    if not (MIN_EXPECTED <= len(kept) <= MAX_EXPECTED):
        print(f"WARNING: kept count {len(kept)} outside expected "
              f"[{MIN_EXPECTED}, {MAX_EXPECTED}] range — verify the label set.",
              file=sys.stderr)

    if args.overlap_with.exists():
        slice_ids = json.loads(args.overlap_with.read_text())
        survivors = [t for t in slice_ids if t in set(kept)]
        not_verified = [t for t in slice_ids if t not in verified]
        print(f"\n{args.overlap_with.name}: {len(slice_ids)} tasks", file=sys.stderr)
        print(f"  survive filter:       {len(survivors)}", file=sys.stderr)
        print(f"  not in Verified:      {len(not_verified)}", file=sys.stderr)
        header["overlap"] = {
            "against": args.overlap_with.name,
            "total": len(slice_ids),
            "survivors": survivors,
            "not_in_verified": not_verified,
        }

    if args.dry_run:
        print("\n[dry-run] not writing output.", file=sys.stderr)
        return 0

    args.out.write_text(json.dumps({"_provenance": header, "instance_ids": kept}, indent=2) + "\n")
    print(f"\nWrote {len(kept)} instances -> {args.out}", file=sys.stderr)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
