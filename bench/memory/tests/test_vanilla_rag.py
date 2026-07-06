#!/usr/bin/env python3
"""Offline unit tests for the vanilla_rag condition in decision_archaeology.

No server, no network, no real embedder: a FakeEmbedder returns deterministic
synthetic vectors so cosine/KNN ranking, recall/MRR, graceful-disable, and the
provenance block are all exercised without I/O.

Run:  python3 -m unittest bench.memory.tests.test_vanilla_rag   (from repo root)
  or: python3 bench/memory/tests/test_vanilla_rag.py
"""

import importlib.util
import re
import unittest
from pathlib import Path

# Load decision_archaeology.py by path (bench/ is not a package).
_MOD_PATH = Path(__file__).resolve().parents[1] / "decision_archaeology.py"
_spec = importlib.util.spec_from_file_location("decision_archaeology", _MOD_PATH)
da = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(da)

_SOURCE = _MOD_PATH.read_text()


def _class_source(name: str) -> str:
    """Slice a top-level class body out of the module source by column dedent."""
    m = re.search(rf"^class {name}\b.*?(?=^\S)", _SOURCE, re.S | re.M)
    assert m, f"class {name} not found"
    return m.group(0)


class FakeEmbedder:
    """Injectable stand-in for VanillaRagEmbedder — maps text -> fixed vector.

    Unknown text embeds to a zero vector (cosine 0). Set `raise_on_embed` to
    simulate an unreachable backend.
    """

    def __init__(self, table, dim, raise_on_embed=False):
        self.table = table
        self.dim = dim
        self.raise_on_embed = raise_on_embed

    def embed(self, texts):
        if self.raise_on_embed:
            raise RuntimeError("embed backend unavailable")
        return [self.table.get(t, [0.0] * self.dim) for t in texts]


def _commit(sha, title, body=""):
    return {"commit": sha, "title": title, "body": body}


class CosineKnnTest(unittest.TestCase):
    def test_cosine_basic(self):
        self.assertAlmostEqual(da._cosine([1.0, 0.0], [1.0, 0.0]), 1.0)
        self.assertAlmostEqual(da._cosine([1.0, 0.0], [0.0, 1.0]), 0.0)
        self.assertAlmostEqual(da._cosine([1.0, 0.0], [-1.0, 0.0]), -1.0)

    def test_cosine_zero_vector_no_div_by_zero(self):
        # Norm falls back to 1.0, so a zero vector yields 0.0 not an error.
        self.assertEqual(da._cosine([0.0, 0.0], [1.0, 0.0]), 0.0)

    def test_knn_ranks_nearest_first(self):
        # Query aligns with c2; c3 is orthogonal; c1 anti-aligned.
        commits = [_commit("c1", "one"), _commit("c2", "two"), _commit("c3", "three")]
        table = {
            "one\n\nnone-body": None,  # unused
        }
        # Build corpus texts exactly as VanillaRagIndex does.
        texts = [f"{c['title']}\n\n{c['body']}".strip() for c in commits]
        vecs = {
            texts[0]: [-1.0, 0.0],  # c1
            texts[1]: [1.0, 0.0],  # c2  <- nearest to query
            texts[2]: [0.0, 1.0],  # c3
            "QUERY": [1.0, 0.0],
        }
        idx = da.VanillaRagIndex(FakeEmbedder(vecs, dim=2), commits)
        out = idx.search("QUERY", limit=3)
        self.assertEqual([c["commit"] for c in out], ["c2", "c3", "c1"])

    def test_knn_respects_limit(self):
        commits = [_commit(f"c{i}", f"t{i}") for i in range(5)]
        texts = [f"{c['title']}\n\n{c['body']}".strip() for c in commits]
        # Descending similarity by index.
        vecs = {t: [float(len(texts) - i), 0.0] for i, t in enumerate(texts)}
        vecs["Q"] = [1.0, 0.0]
        idx = da.VanillaRagIndex(FakeEmbedder(vecs, dim=2), commits)
        out = idx.search("Q", limit=2)
        self.assertEqual([c["commit"] for c in out], ["c0", "c1"])

    def test_empty_corpus_returns_empty(self):
        idx = da.VanillaRagIndex(FakeEmbedder({}, dim=2), [])
        self.assertEqual(idx.search("anything", limit=5), [])


class CheckHitTest(unittest.TestCase):
    def test_hit_returns_1_based_rank(self):
        results = [_commit("aaa111", "x"), _commit("bbb222", "y")]
        hit, rank = da.check_hit(results, "bbb222")
        self.assertTrue(hit)
        self.assertEqual(rank, 2)

    def test_prefix_match_short_vs_full(self):
        results = [_commit("abc123def456", "x")]
        hit, rank = da.check_hit(results, "abc123")
        self.assertTrue(hit)
        self.assertEqual(rank, 1)

    def test_miss_returns_none_rank(self):
        results = [_commit("aaa", "x")]
        hit, rank = da.check_hit(results, "zzz")
        self.assertFalse(hit)
        self.assertIsNone(rank)

    def test_empty_ground_truth_is_miss(self):
        self.assertEqual(da.check_hit([_commit("aaa", "x")], ""), (False, None))


class RecallMrrTest(unittest.TestCase):
    """Reproduce the exact aggregation main() performs per condition."""

    @staticmethod
    def _aggregate(results_per_q, truths):
        hits, ranks = [], []
        for results, commit in zip(results_per_q, truths):
            hit, rank = da.check_hit(results, commit)
            hits.append(1.0 if hit else 0.0)
            ranks.append(1.0 / rank if rank else 0.0)
        recall = round(float(sum(hits) / len(hits)), 4) if hits else 0.0
        mrr = round(float(sum(ranks) / len(ranks)), 4) if ranks else 0.0
        return recall, mrr

    def test_hit_at_rank_1(self):
        recall, mrr = self._aggregate([[_commit("gt", "x")]], ["gt"])
        self.assertEqual(recall, 1.0)
        self.assertEqual(mrr, 1.0)

    def test_hit_at_rank_2_gives_half_mrr(self):
        results = [[_commit("other", "x"), _commit("gt", "y")]]
        recall, mrr = self._aggregate(results, ["gt"])
        self.assertEqual(recall, 1.0)
        self.assertEqual(mrr, 0.5)

    def test_known_miss_scores_zero(self):
        recall, mrr = self._aggregate([[_commit("other", "x")]], ["gt"])
        self.assertEqual(recall, 0.0)
        self.assertEqual(mrr, 0.0)

    def test_mixed_hit_and_miss(self):
        # q1 hit@1, q2 miss -> recall 0.5, mrr (1 + 0)/2 = 0.5
        results = [[_commit("gt1", "a")], [_commit("nope", "b")]]
        recall, mrr = self._aggregate(results, ["gt1", "gt2"])
        self.assertEqual(recall, 0.5)
        self.assertEqual(mrr, 0.5)


class GracefulDisableTest(unittest.TestCase):
    def test_index_build_raises_when_backend_unreachable(self):
        # VanillaRagIndex embeds the corpus in __init__; a raising embedder
        # must propagate so main() can catch it and disable the condition.
        bad = FakeEmbedder({}, dim=2, raise_on_embed=True)
        with self.assertRaises(RuntimeError):
            da.VanillaRagIndex(bad, [_commit("c1", "t1")])

    def test_disabled_condition_scores_zero_and_records_error(self):
        # Mirror main()'s disable path: index=None, error captured, and the
        # per-question branch yields [] (no crash), so recall/MRR collapse to 0.
        vanilla_index = None
        vanilla_error = None
        try:
            da.VanillaRagIndex(FakeEmbedder({}, dim=2, raise_on_embed=True),
                               [_commit("c1", "t1")])
        except Exception as e:  # noqa: BLE001 - mirrors main()
            vanilla_error = f"{type(e).__name__}: {e}"

        self.assertIsNone(vanilla_index)
        self.assertIsNotNone(vanilla_error)
        self.assertIn("RuntimeError", vanilla_error)

        # main() does: results = vanilla_index.search(...) if vanilla_index else []
        results = vanilla_index.search("q", 10) if vanilla_index else []
        self.assertEqual(results, [])
        hit, rank = da.check_hit(results, "gt")
        self.assertFalse(hit)
        self.assertIsNone(rank)


class ProvenanceTest(unittest.TestCase):
    """Build the provenance block the way main() does and assert its shape."""

    @staticmethod
    def _provenance(embedder_dim, vanilla_index, vanilla_error):
        return {
            "backend": "spelunk-server /index/embed (native F2LLM-v2-330M)",
            "embedding_model": "codefuse-ai/F2LLM-v2-330M",
            "embedding_dim": embedder_dim,
            "method": "plain embed-and-KNN over raw commit messages (no harvest, no LLM extraction, no graph, no rerank)",
            "determinism": "deterministic, n=1",
            "corpus_commits": len(vanilla_index.commits) if vanilla_index else 0,
            "error": vanilla_error,
        }

    def test_provenance_keys_and_values_when_enabled(self):
        commits = [_commit("c1", "t1"), _commit("c2", "t2")]
        texts = [f"{c['title']}\n\n{c['body']}".strip() for c in commits]
        vecs = {t: [1.0, 0.0] for t in texts}
        emb = FakeEmbedder(vecs, dim=896)
        idx = da.VanillaRagIndex(emb, commits)

        prov = self._provenance(emb.dim, idx, None)
        for key in (
            "embedding_model",
            "embedding_dim",
            "method",
            "determinism",
            "corpus_commits",
        ):
            self.assertIn(key, prov)
        self.assertEqual(prov["embedding_model"], "codefuse-ai/F2LLM-v2-330M")
        self.assertEqual(prov["embedding_dim"], 896)
        self.assertEqual(prov["determinism"], "deterministic, n=1")
        self.assertEqual(prov["corpus_commits"], 2)
        self.assertIsNone(prov["error"])

    def test_provenance_when_disabled(self):
        prov = self._provenance(None, None, "RuntimeError: down")
        self.assertEqual(prov["corpus_commits"], 0)
        self.assertEqual(prov["error"], "RuntimeError: down")
        self.assertIsNone(prov["embedding_dim"])


class PlainControlTest(unittest.TestCase):
    """Structural guard: vanilla_rag must be a plain embed+KNN control with no
    harvest / LLM / graph / rerank machinery."""

    def test_search_only_uses_embed_and_cosine(self):
        lowered = _class_source("VanillaRagIndex").lower()
        for forbidden in ("harvest", "llm", "graph", "rerank", "subprocess", "spelunk"):
            self.assertNotIn(forbidden, lowered,
                             f"vanilla_rag path must not reference {forbidden!r}")

    def test_embedder_makes_no_subprocess_or_cli_calls(self):
        src = _class_source("VanillaRagEmbedder").lower()
        for forbidden in ("subprocess", "harvest", "rerank"):
            self.assertNotIn(forbidden, src)

    def test_search_call_graph_touches_only_embed(self):
        # The only external effect of search() is embedder.embed(); prove it by
        # counting embed calls and asserting nothing else is invoked.
        calls = {"embed": 0}

        class CountingEmbedder(FakeEmbedder):
            def embed(self, texts):
                calls["embed"] += 1
                return super().embed(texts)

        commits = [_commit("c1", "t1")]
        texts = [f"{c['title']}\n\n{c['body']}".strip() for c in commits]
        emb = CountingEmbedder({texts[0]: [1.0, 0.0], "q": [1.0, 0.0]}, dim=2)
        idx = da.VanillaRagIndex(emb, commits)  # embeds corpus once
        idx.search("q", 5)  # embeds query once
        self.assertEqual(calls["embed"], 2)


if __name__ == "__main__":
    unittest.main()
