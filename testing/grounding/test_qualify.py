"""Synthetic unit-test data only; never persisted as holdout evidence."""
import copy
import json
from pathlib import Path
import tempfile
import time
import unittest

from qualify import CATEGORIES, POLICY, corpus_queries, digest, evaluate, measure


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


if __name__ == "__main__":
    unittest.main()
