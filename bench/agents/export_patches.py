#!/usr/bin/env python3
"""Export agent patches to SWE-bench prediction format.

Reads a batch result JSON and writes a flat predictions list the
SWE-bench harness can consume, plus a metadata sidecar.

Usage:
    python bench/agents/export_patches.py \\
        --results bench/results/swebench-baseline-batch.json \\
        --patches-dir bench/patches/baseline \\
        --out bench/predictions/baseline.json
"""

import argparse
import json
from pathlib import Path


def main():
    parser = argparse.ArgumentParser(
        description="Export agent patches to SWE-bench prediction format."
    )
    parser.add_argument("--results", required=True, help="Batch result JSON.")
    parser.add_argument(
        "--patches-dir",
        required=True,
        help="Directory containing per-task .patch files.",
    )
    parser.add_argument("--out", required=True, help="Output predictions JSON.")
    args = parser.parse_args()

    with open(args.results) as f:
        results = json.load(f)

    patches_dir = Path(args.patches_dir)
    predictions = {}

    skipped = 0
    errored = 0
    no_patch = 0

    for r in results:
        task_id = r.get("task_id", "")
        if r.get("skipped"):
            skipped += 1
            continue
        if r.get("error"):
            errored += 1
            continue

        patch_file = r.get("patch_file")
        if not patch_file:
            no_patch += 1
            continue

        patch_path = Path(patch_file)
        if not patch_path.exists():
            patch_path = patches_dir / f"{task_id}.patch"

        if patch_path.exists():
            predictions[task_id] = {
                "instance_id": task_id,
                "model_name_or_path": r.get("model", "deepseek-v4-flash"),
                "model_patch": patch_path.read_text(),
            }
        else:
            no_patch += 1

    predictions_list = list(predictions.values())

    # Write flat list — SWE-bench harness expects this format
    Path(args.out).parent.mkdir(parents=True, exist_ok=True)
    with open(args.out, "w") as f:
        json.dump(predictions_list, f, indent=2)

    # Metadata sidecar
    meta_path = Path(args.out).with_suffix(".meta.json")
    meta_path.write_text(
        json.dumps(
            {
                "model": results[0].get("model", "") if results else "",
                "condition": results[0].get("condition", "") if results else "",
                "spelunk_version": results[0].get("spelunk_version", ""),
                "seed": results[0].get("seed", ""),
                "tasks_total": len(results),
                "tasks_with_patch": len(predictions_list),
                "tasks_skipped": skipped,
                "tasks_errored": errored,
                "tasks_no_patch": no_patch,
            },
            indent=2,
        )
    )

    print(f"Exported {len(predictions_list)} predictions to {args.out}")
    print(f"  Skipped: {skipped}  Errored: {errored}  No patch: {no_patch}")


if __name__ == "__main__":
    main()
