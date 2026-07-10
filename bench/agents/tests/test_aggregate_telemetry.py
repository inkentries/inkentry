"""Tests for aggregate_telemetry.py — grouping, cost math, legacy-none, projection.

Run:
    uv run --with pytest pytest bench/agents/tests/ -v
"""

from pathlib import Path

import pytest

import aggregate_telemetry as agg

AGENTS_DIR = Path(__file__).resolve().parents[1]
RESULTS_DIR = AGENTS_DIR.parent / "results"
PRICING = AGENTS_DIR / "pricing.json"

# A price config independent of the committed pricing.json, so cost-math tests
# don't drift if list prices are re-verified.
PRICES = {
    "cache_read_multiplier": 0.1,
    "prices": {
        "priced-model": {"input_per_mtok": 3.0, "output_per_mtok": 15.0, "verified_on": "2026-07-08"},
        "placeholder-model": {"input_per_mtok": None, "output_per_mtok": None, "verified_on": None},
    },
}


# --- grouping + legacy-none handling ---------------------------------------


def test_legacy_row_maps_to_harness_none():
    row = {"model": "m", "condition": "baseline"}  # no harness field
    assert agg.cell_key(row) == ("m", "none", "baseline", None)


def test_explicit_harness_and_filter_preserved():
    row = {"model": "m", "harness": "opencode", "condition": "baseline", "instance_filter": "full"}
    assert agg.cell_key(row) == ("m", "opencode", "baseline", "full")


def test_group_splits_legacy_from_harness_rows():
    rows = [
        {"model": "m", "condition": "baseline", "input_tokens": 10},  # legacy -> none
        {"model": "m", "harness": "opencode", "condition": "baseline", "input_tokens": 20},
        {"model": "m", "harness": "opencode", "condition": "baseline", "input_tokens": 30},
    ]
    cells = agg.group_by_cell(rows)
    assert len(cells) == 2
    assert len(cells[("m", "none", "baseline", None)]) == 1
    assert len(cells[("m", "opencode", "baseline", None)]) == 2


def test_null_harness_value_treated_as_none():
    # A row that carries harness: null (the harness=none adapter output).
    row = {"model": "m", "harness": None, "condition": "baseline"}
    assert agg.cell_key(row)[1] == "none"


# --- summary stats ----------------------------------------------------------


def test_summary_mean_and_median():
    rows = [
        {"input_tokens": 10, "output_tokens": 1, "turns": 2, "wall_seconds": 1.0},
        {"input_tokens": 20, "output_tokens": 3, "turns": 4, "wall_seconds": 3.0},
        {"input_tokens": 30, "output_tokens": 5, "turns": 6, "wall_seconds": 5.0},
    ]
    s = agg.summarize_cell(rows)
    assert s["tasks"] == 3
    assert s["input_tokens"]["mean"] == 20.0
    assert s["input_tokens"]["median"] == 20.0
    assert s["output_tokens"]["mean"] == 3.0


def test_summary_handles_missing_metric():
    rows = [{"input_tokens": 10}, {"input_tokens": 20}]  # no turns/wall
    s = agg.summarize_cell(rows)
    assert s["turns"]["mean"] is None
    assert s["wall_seconds"]["median"] is None
    assert s["input_tokens"]["mean"] == 15.0


# --- cost math --------------------------------------------------------------


def test_raw_cost_sums_input_and_output():
    rows = [{"input_tokens": 1_000_000, "output_tokens": 1_000_000}]
    cost = agg.cell_cost(rows, PRICES, "priced-model")
    assert cost["priced"] is True
    # 1M in @ $3 + 1M out @ $15 = $18
    assert cost["raw_usd"] == 18.0


def test_effective_cost_applies_cache_discount():
    # 1M input, all cache-read -> effective input billed at 0.1x = $0.30, no output.
    rows = [{"input_tokens": 1_000_000, "output_tokens": 0, "cache_read_input_tokens": 1_000_000}]
    cost = agg.cell_cost(rows, PRICES, "priced-model")
    assert cost["raw_usd"] == 3.0
    assert cost["effective_usd"] == pytest.approx(0.3)


def test_cache_read_clamped_to_input_tokens():
    # cache_read larger than input_tokens must not produce a negative full-rate term.
    rows = [{"input_tokens": 100, "output_tokens": 0, "cache_read_input_tokens": 999}]
    cost = agg.cell_cost(rows, PRICES, "priced-model")
    # All 100 tokens billed at the cache rate (0.1x), nothing at full rate.
    assert cost["effective_usd"] == pytest.approx(100 * (3.0 / 1_000_000) * 0.1)


def test_no_cache_field_effective_equals_raw():
    rows = [{"input_tokens": 5000, "output_tokens": 200}]
    cost = agg.cell_cost(rows, PRICES, "priced-model")
    assert cost["effective_usd"] == cost["raw_usd"]


def test_placeholder_price_is_not_priced():
    cost = agg.cell_cost([{"input_tokens": 1, "output_tokens": 1}], PRICES, "placeholder-model")
    assert cost["priced"] is False
    assert cost["raw_usd"] is None


def test_unknown_model_is_not_priced():
    cost = agg.cell_cost([{"input_tokens": 1}], PRICES, "nope")
    assert cost["priced"] is False


# --- projection -------------------------------------------------------------


def test_projection_math():
    spec = {
        "name": "demo",
        "model": "priced-model",
        "tasks": 50,
        "conditions": 2,
        "seeds": 3,
        "input_tokens_per_task": 1_000_000,
        "output_tokens_per_task": 0,
    }
    p = agg.project_cost(spec, PRICES)
    assert p["total_runs"] == 300
    # 300 runs * (1M in @ $3) = $900
    assert p["usd"] == 900.0


def test_projection_unpriced_model():
    spec = {
        "model": "placeholder-model",
        "tasks": 10,
        "conditions": 1,
        "seeds": 1,
        "input_tokens_per_task": 1000,
        "output_tokens_per_task": 100,
    }
    p = agg.project_cost(spec, PRICES)
    assert p["priced"] is False
    assert p["usd"] is None


# --- end-to-end over the committed results dir ------------------------------


def test_aggregation_over_committed_results():
    """The committed bench/results contains legacy (harness none) examples plus a
    harness-carrying fixture. Aggregation must produce both, priced with the real
    pricing.json."""
    prices = agg.load_prices(PRICING)
    rows = agg.load_results(RESULTS_DIR)
    report = agg.build_report(rows, prices)

    harnesses = {c["harness"] for c in report["cells"]}
    assert "none" in harnesses, "legacy example rows should aggregate as harness none"
    assert "claude-code" in harnesses, "fixture should contribute a claude-code cell"

    # The claude-code cell is priced (claude-sonnet-5 is in pricing.json) and its
    # effective cost is strictly below raw because a fixture row carries cache reads.
    cc = next(
        c
        for c in report["cells"]
        if c["harness"] == "claude-code" and c["model"] == "claude-sonnet-5"
    )
    assert cc["summary"]["tasks"] == 2
    assert cc["cost"]["priced"] is True
    assert cc["cost"]["effective_usd"] < cc["cost"]["raw_usd"]

    # A legacy example cell uses gemma-4-e2b-it, which has no price -> not priced.
    legacy = next(c for c in report["cells"] if c["harness"] == "none")
    assert legacy["cost"]["priced"] is False

    # The committed projection (Sonnet-5 x 50-slice x 2 x 3) is present and priced.
    assert report["projections"], "pricing.json ships a prospective projection"
    assert any(p["priced"] and p["total_runs"] == 300 for p in report["projections"])


def test_markdown_renders():
    prices = agg.load_prices(PRICING)
    rows = agg.load_results(RESULTS_DIR)
    md = agg.render_markdown(agg.build_report(rows, prices))
    assert "Per-cell telemetry and cost" in md
    assert "Projected cost" in md
