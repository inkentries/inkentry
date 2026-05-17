#!/usr/bin/env python3
"""Export agent patches to SWE-bench prediction format.

Reads a batch result JSON (from batch_run.py or swebench_run.sh) and
extracts per-task patches, model info, and reproducibility fields into
the format expected by the SWE-bench Docker harness.

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

    for r in results:
        task_id = r.get("task_id", "")
        if r.get("skipped") or r.get("error"):
            continue

        patch_file = r.get("patch_file")
        if not patch_file:
            continue

        patch_path = Path(patch_file)
        if not patch_path.exists():
            # Try relative to patches-dir
            patch_path = patches_dir / f"{task_id}.patch"

        if patch_path.exists():
            predictions[task_id] = {
                "instance_id": task_id,
                "model_name_or_path": r.get("model", "deepseek-v4-flash"),
                "model_patch": patch_path.read_text(),
            }

    output = {
        "predictions": list(predictions.values()),
        "metadata": {
            "model": results[0].get("model", "") if results else "",
            "condition": results[0].get("condition", "") if results else "",
            "spelunk_version": results[0].get("spelunk_version", ""),
            "seed": results[0].get("seed", ""),
        },
    }

    Path(args.out).parent.mkdir(parents=True, exist_ok=True)
    with open(args.out, "w") as f:
        json.dump(output, f, indent=2)

    print(f"Exported {len(predictions)} predictions to {args.out}")


if __name__ == "__main__":
    main()
