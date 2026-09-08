"""Synthetic unit-test data only; never persisted as holdout evidence."""
import copy
import hashlib
import json
from pathlib import Path
import tempfile
import time
import unittest

from qualify import CATEGORIES, POLICY, approved_development, corpus_queries, digest, evaluate, measure, validate_recall_query


class QualificationTests(unittest.TestCase):
    def setUp(self):
        self.corpus = {"schema_version":1,"authors":["unit-test-author"],"queries":[
            {"id":f"{split}-{category}-{i}","text":f"unit test {split} {category} {i}","split":split,
             "category":category,"allowed_project_ids":["test-project"]}
            for split in ("development", "holdout") for category in sorted(CATEGORIES) for i in range(10)]}

    def evidence(self, split):
        labels, runs = [], []
        for q in corpus_queries(self.corpus, split).values():
            hits = [] if q["category"] == "no_answer" else [{"id":f"{q['id']}-{i}","project_id":"test-project"} for i in range(5)]
            labels.append({"query_id":q["id"],"reviewer_id":"unit-test-reviewer","relevant":hits,"known_item":hits[0] if hits else None})
            runs.append({"query_id":q["id"],"policy_revision":POLICY,"candidate":copy.deepcopy(hits),"retrieval_status":"available" if hits else "no_evidence"})
        return ({"schema_version":1,"split":split,"provenance":"independent_review","labels":labels},
                {"schema_version":1,"split":split,"policy_revision":POLICY,"results":runs,
                 "source_commit":"unit-test-commit","source_dirty":False,"replay_binary_sha256":"a"*64,"collected_at_unix":int(time.time())})

    def test_full_scoped_evidence_scores_exactly(self):
        labels, replay = self.evidence("holdout")
        result = measure(self.corpus, "holdout", labels, replay)
        self.assertEqual(result["known_item_top1"], 1)
        self.assertEqual(result["precision_at_5"], 1)
        self.assertEqual(result["false_grounding_rate"], 0)
        self.assertTrue(result["independent_labels"])

    def test_query_sidecar_cannot_relabel_a_different_recall_payload(self):
        query = {"text": "frozen query"}
        row = {"query_sha256": hashlib.sha256(b"frozen query").hexdigest(),
               "recall": {"query": "frozen query", "results": [], "degraded": False, "errors": []}}
        validate_recall_query(row, query)
        for recall in ({"query": "different query"}, {"query": " frozen query"}, {}, None):
            invalid = dict(row, recall=recall)
            with self.assertRaises(ValueError):
                validate_recall_query(invalid, query)
        with self.assertRaises(ValueError):
            validate_recall_query(dict(row, query_sha256="0" * 64), query)

    def test_partial_recall_cannot_be_replayed_as_clean_evidence(self):
        query = {"text": "frozen query"}
        # Even usable hits do not make missing upstream coverage a clean run.
        for results in ([], [{"id": "available-source"}]):
            clean = {"query": query["text"], "results": results,
                     "degraded": False, "errors": [], "degraded_reason": None}
            row = {"query_sha256": hashlib.sha256(query["text"].encode()).hexdigest(),
                   "recall": clean}
            validate_recall_query(row, query)
            mutations = [
                {"degraded": True}, {"degraded": None}, {"degraded": 0},
                {"degraded": "false"}, {"degraded": []},
                {"errors": ["partial_retrieval_unavailable"]}, {"errors": None},
                {"errors": ""}, {"errors": {}},
                {"degraded_reason": "partial_retrieval_unavailable"},
                {"results": None}, {"results": {}}, {"results": [None]},
            ]
            for mutation in mutations:
                with self.subTest(results=results, mutation=mutation):
                    with self.assertRaises(ValueError):
                        validate_recall_query(dict(row, recall=dict(clean, **mutation)), query)
            for missing in ("degraded", "errors", "results"):
                incomplete = dict(clean)
                del incomplete[missing]
                with self.subTest(missing=missing):
                    with self.assertRaises(ValueError):
                        validate_recall_query(dict(row, recall=incomplete), query)

    def test_author_is_not_an_independent_labeler(self):
        labels, replay = self.evidence("holdout")
        labels["labels"][0]["reviewer_id"] = "unit-test-author"
        self.assertFalse(measure(self.corpus, "holdout", labels, replay)["independent_labels"])

    def test_missing_duplicate_and_unbalanced_corpus_rejected(self):
        for mutation in (lambda c: c["queries"].pop(),
                         lambda c: c["queries"].__setitem__(0, c["queries"][1]),
                         lambda c: c["queries"][0].update(category="scope")):
            corpus = copy.deepcopy(self.corpus)
            mutation(corpus)
            with self.assertRaises(ValueError):
                corpus_queries(corpus, "development")

    def test_missing_duplicate_foreign_and_unknown_replay_rejected(self):
        for mutation in (lambda r: r["results"].pop(),
                         lambda r: r["results"].append(r["results"][0]),
                         lambda r: r["results"][0].update(query_id="foreign"),
                         lambda r: r["results"][0].update(retrieval_status="partial")):
            labels, replay = self.evidence("holdout")
            mutation(replay)
            with self.assertRaises(ValueError):
                measure(self.corpus, "holdout", labels, replay)

    def test_no_answer_and_unavailable_are_not_silently_successful(self):
        labels, replay = self.evidence("holdout")
        row = next(r for r in replay["results"] if "no_answer" in r["query_id"])
        row["candidate"] = [{"id":"unrelated","project_id":"foreign"}]
        replay["results"][0]["retrieval_status"] = "unavailable"
        result = measure(self.corpus, "holdout", labels, replay)
        self.assertEqual(result["false_grounding_rate"], 0.1)
        self.assertEqual(result["scope_violations"], 1)
        self.assertEqual(result["unavailable_queries"], 1)

    def test_fewer_hits_cannot_inflate_precision(self):
        labels, replay = self.evidence("holdout")
        for row in replay["results"]:
            row["candidate"] = row["candidate"][:1]
        result = measure(self.corpus, "holdout", labels, replay)
        self.assertEqual(result["known_item_top1"], 1)
        self.assertEqual(result["precision_at_5"], 0.2)

    def test_sealed_candidate_identity_and_dirty_source_gates(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "unit-test-corpus.json"
            path.write_text(json.dumps(self.corpus))
            labels, replay = self.evidence("development")
            labels["corpus_sha256"] = replay["corpus_sha256"] = digest(path)
            development = evaluate(path, "development", labels, replay)
            self.assertTrue(development["development_approved"])
            labels, replay = self.evidence("holdout")
            labels["corpus_sha256"] = replay["corpus_sha256"] = digest(path)
            result = evaluate(path, "holdout", labels, replay, development)
            self.assertTrue(result["retrieval_qualified"])
            self.assertFalse(result["release_qualified"])
            for key, value in [("corpus_sha256","foreign"), ("replay_binary_sha256","b"*64),
                               ("source_commit","foreign"), ("collected_at_unix",1)]:
                invalid = copy.deepcopy(replay)
                invalid[key] = value
                with self.assertRaises(ValueError):
                    evaluate(path, "holdout", labels, invalid, development)
            replay["source_dirty"] = True
            self.assertFalse(evaluate(path, "holdout", labels, replay, development)["retrieval_qualified"])
            with self.assertRaises(ValueError):
                evaluate(path, "holdout", labels, replay)

    def test_development_requires_independent_quality_before_sealing(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "unit-test-corpus.json"
            path.write_text(json.dumps(self.corpus))
            for failure in ("author_labels", "low_top1", "low_precision"):
                labels, replay = self.evidence("development")
                labels["corpus_sha256"] = replay["corpus_sha256"] = digest(path)
                if failure == "author_labels":
                    labels["labels"][0]["reviewer_id"] = "unit-test-author"
                else:
                    for row in replay["results"]:
                        if failure == "low_top1":
                            row["candidate"].reverse()
                        else:
                            row["candidate"] = row["candidate"][:1]
                with self.subTest(failure=failure):
                    result = evaluate(path, "development", labels, replay)
                    self.assertFalse(result["development_approved"])
                    self.assertFalse(approved_development(result))

    def test_holdout_rechecks_quality_in_existing_development_seals(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "unit-test-corpus.json"
            path.write_text(json.dumps(self.corpus))
            labels, replay = self.evidence("development")
            labels["corpus_sha256"] = replay["corpus_sha256"] = digest(path)
            development = evaluate(path, "development", labels, replay)
            labels, replay = self.evidence("holdout")
            labels["corpus_sha256"] = replay["corpus_sha256"] = digest(path)
            invalid_metrics = [None, [], {}, "approved"]
            mutations = [
                ("independent_labels", False), ("independent_labels", 1),
                ("known_item_top1", 0.949), ("precision_at_5", 0.799),
                ("false_grounding_rate", 0.051), ("unavailable_queries", 1),
                ("scope_violations", 1), ("known_item_top1", True),
                ("precision_at_5", "1.0"), ("precision_at_5", float("nan")),
                ("precision_at_5", float("inf")), ("unavailable_queries", False),
                ("query_count", 59), ("known_item_queries", 0),
                ("known_item_queries", 51), ("no_answer_queries", 9),
                ("no_answer_queries", 60), ("query_count", "60"),
            ]
            for key, value in mutations:
                invalid_metrics.append(dict(development["metrics"], **{key: value}))
            for missing in development["metrics"]:
                metrics = dict(development["metrics"])
                del metrics[missing]
                invalid_metrics.append(metrics)
            for metrics in invalid_metrics:
                with self.subTest(metrics=metrics):
                    stale_seal = dict(development, metrics=metrics)
                    self.assertFalse(approved_development(stale_seal))
                    with self.assertRaises(ValueError):
                        evaluate(path, "holdout", labels, replay, stale_seal)
            missing_metrics = dict(development)
            del missing_metrics["metrics"]
            self.assertFalse(approved_development(missing_metrics))
            boundary = dict(development, metrics=dict(development["metrics"],
                known_item_top1=0.95, precision_at_5=0.8, false_grounding_rate=0.05))
            self.assertTrue(approved_development(boundary))


if __name__ == "__main__":
    unittest.main()
