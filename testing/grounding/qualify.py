#!/usr/bin/env python3
"""Frozen 60/60 grounding evaluation using the actual Rust selector.

No labels or queries are generated here. Supply a reviewed corpus and separate
development/holdout labels. Author-generated examples cannot qualify holdout.
Selector timing is explicitly NOT end-to-end context latency or canary evidence.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import subprocess
import time

POLICY = "grounding-evidence-v1"
CATEGORIES = {"continuation", "paraphrase", "history", "supersession", "scope", "no_answer"}
SPLITS = ("development", "holdout")


def require(condition, message):
    if not condition:
        raise ValueError(message)


def digest(path):
    return hashlib.sha256(Path(path).read_bytes()).hexdigest()


def read(path):
    return json.loads(Path(path).read_text())


def corpus_queries(corpus, split):
    require(isinstance(corpus, dict) and corpus.get("schema_version") == 1, "unsupported corpus")
    require(isinstance(corpus.get("authors"), list) and corpus["authors"] and all(isinstance(a, str) and a for a in corpus["authors"]), "missing corpus authors")
    queries = corpus.get("queries")
    require(isinstance(queries, list) and len(queries) == 120, "exactly 120 frozen queries required")
    seen, texts = set(), set()
    for query in queries:
        require(isinstance(query, dict), "invalid query")
        query_id, text = query.get("id"), query.get("text")
        require(isinstance(query_id, str) and query_id and query_id not in seen, "duplicate/missing query id")
        require(isinstance(text, str) and text.strip() and text.strip().casefold() not in texts, "duplicate/missing query text")
        require(query.get("split") in SPLITS and query.get("category") in CATEGORIES, "invalid split/category")
        projects = query.get("allowed_project_ids")
        require(isinstance(projects, list) and projects and all(p is None or isinstance(p, str) and p for p in projects), "explicit authorized project scopes required")
        seen.add(query_id)
        texts.add(text.strip().casefold())
    for part in SPLITS:
        for category in CATEGORIES:
            require(sum(q["split"] == part and q["category"] == category for q in queries) == 10, "each split requires ten queries in each category")
    return {q["id"]: q for q in queries if q["split"] == split}


def identity(item):
    require(isinstance(item, dict) and isinstance(item.get("id"), str) and item["id"], "missing source identity")
    require("project_id" in item and (item["project_id"] is None or isinstance(item["project_id"], str)), "missing source scope")
    return item["project_id"], item["id"]


def indexed(items, key):
    require(isinstance(items, list) and all(isinstance(i, dict) for i in items), "invalid result array")
    result = {}
    for item in items:
        value = item.get(key)
        require(isinstance(value, str) and value and value not in result, "duplicate/missing result identity")
        result[value] = item
    return result


def measure(corpus, split, labels, replay):
    queries = corpus_queries(corpus, split)
    require(labels.get("split") == split and labels.get("schema_version") == 1, "labels have wrong split/schema")
    require(replay.get("split") == split and replay.get("policy_revision") == POLICY, "replay has wrong split/policy")
    label_map = indexed(labels.get("labels"), "query_id")
    runs = indexed(replay.get("results"), "query_id")
    require(set(queries) == set(label_map) == set(runs), "incomplete or foreign query coverage")
    known, top1, relevant_at_5, answerable, no_answer, false_ground, unavailable, scope_violations = (0,) * 8
    independent = labels.get("provenance") == "independent_review"
    for query_id, query in queries.items():
        label, run = label_map[query_id], runs[query_id]
        reviewer = label.get("reviewer_id")
        independent = independent and isinstance(reviewer, str) and bool(reviewer) and reviewer not in corpus["authors"]
        relevant_list = label.get("relevant")
        require(isinstance(relevant_list, list), "missing relevant-source labels")
        relevant = {identity(item) for item in relevant_list}
        require(len(relevant) == len(relevant_list), "duplicate relevance labels")
        require(all(project in query["allowed_project_ids"] for project, _ in relevant), "label crosses authorized scope")
        expected = identity(label["known_item"]) if label.get("known_item") is not None else None
        require(expected is None or expected in relevant, "known item must be relevant")
        require(run.get("policy_revision") == POLICY, "foreign selector policy")
        require(run.get("retrieval_status") in {"available", "no_evidence", "unavailable"}, "unknown retrieval status")
        if run["retrieval_status"] == "unavailable":
            unavailable += 1
        retrieved = run.get("candidate")
        require(isinstance(retrieved, list) and len(retrieved) <= 5, "candidate must contain at most five hits")
        ids = [identity(item) for item in retrieved]
        require(len(ids) == len(set(ids)), "duplicate candidate identities")
        scope_violations += sum(project not in query["allowed_project_ids"] for project, _ in ids)
        if expected is not None:
            known += 1
            top1 += bool(ids and ids[0] == expected)
        if relevant:
            answerable += 1
            relevant_at_5 += sum(item in relevant for item in ids)
        else:
            no_answer += 1
            false_ground += bool(ids)
    require(known > 0 and answerable > 0 and no_answer >= 10, "known-item and no-answer coverage required")
    return {"query_count": len(queries), "known_item_queries": known, "known_item_top1": top1 / known,
            # Fixed denominator: returning fewer hits cannot inflate precision@5.
            "precision_at_5": relevant_at_5 / (5 * answerable),
            "no_answer_queries": no_answer, "false_grounding_rate": false_ground / no_answer,
            "unavailable_queries": unavailable, "scope_violations": scope_violations,
            "independent_labels": bool(independent)}


def approved_development(report):
    return (report.get("schema_version") == 1 and report.get("split") == "development"
            and report.get("policy_revision") == POLICY and report.get("development_approved") is True
            and report.get("source_dirty") is False and type(report.get("sealed_at_unix")) is int
            and 0 < report["sealed_at_unix"] <= int(time.time()))


def evaluate(corpus_path, split, labels, replay, development=None):
    corpus = read(corpus_path)
    corpus_hash = digest(corpus_path)
    require(labels.get("corpus_sha256") == corpus_hash == replay.get("corpus_sha256"), "corpus/label/replay fingerprint mismatch")
    metrics = measure(corpus, split, labels, replay)
    result = {"schema_version": 1, "split": split, "policy_revision": POLICY, "minimum_lexical_coverage": 0.4,
              "corpus_sha256": corpus_hash, "replay_binary_sha256": replay.get("replay_binary_sha256"),
              "source_commit": replay.get("source_commit"), "source_dirty": replay.get("source_dirty"),
              "sealed_at_unix": int(time.time()), "metrics": metrics,
              "development_approved": False, "retrieval_qualified": False, "release_qualified": False}
    clean = replay.get("source_dirty") is False and isinstance(replay.get("source_commit"), str) and bool(replay["source_commit"])
    clean = clean and isinstance(replay.get("replay_binary_sha256"), str) and len(replay["replay_binary_sha256"]) == 64
    safe = metrics["unavailable_queries"] == 0 and metrics["scope_violations"] == 0 and metrics["false_grounding_rate"] <= 0.05
    if split == "development":
        # This revision has a fixed 0.4 policy. If it fails, change/calibrate on
        # development and collect a new source-bound report before holdout.
        result["development_approved"] = bool(clean and safe)
    else:
        require(isinstance(development, dict) and approved_development(development), "seal development before reading holdout labels")
        require(all(development.get(k) == result.get(k) for k in ("corpus_sha256", "replay_binary_sha256", "source_commit", "policy_revision")), "holdout candidate differs from sealed development")
        require(type(replay.get("collected_at_unix")) is int and development["sealed_at_unix"] <= replay["collected_at_unix"] <= int(time.time()), "holdout collection time is outside the sealed evaluation window")
        result["retrieval_qualified"] = bool(clean and safe and metrics["independent_labels"]
                                              and metrics["known_item_top1"] >= 0.95 and metrics["precision_at_5"] >= 0.8)
    return result


def build_selector():
    root = Path(__file__).resolve().parents[2]
    require(not subprocess.check_output(["git", "status", "--porcelain"], cwd=root).strip(), "clean source required to build selector")
    commit = subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=root, text=True).strip()
    subprocess.run(["cargo", "build", "-p", "mcp-tools", "--example", "grounding_replay", "--offline"], cwd=root, check=True)
    metadata = json.loads(subprocess.check_output(["cargo", "metadata", "--no-deps", "--format-version", "1", "--offline"], cwd=root))
    binary = Path(metadata["target_directory"]) / "debug" / "examples" / ("grounding_replay.exe" if os.name == "nt" else "grounding_replay")
    require(commit == subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=root, text=True).strip()
            and not subprocess.check_output(["git", "status", "--porcelain"], cwd=root).strip(), "source changed during build")
    return {"schema_version":1,"policy_revision":POLICY,"source_commit":commit,"source_dirty":False,
            "binary":str(binary),"replay_binary_sha256":digest(binary),"profile":"debug","built_at_unix":int(time.time())}


def collect(corpus_path, split, recalls_path, binary, build_receipt, development=None):
    corpus = read(corpus_path)
    queries = corpus_queries(corpus, split)
    rows = [json.loads(line) for line in Path(recalls_path).read_text().splitlines() if line.strip()]
    by_id = indexed(rows, "query_id")
    require(set(by_id) == set(queries), "exact split coverage required for recall input")
    for query_id, row in by_id.items():
        require(row.get("query_sha256") == hashlib.sha256(queries[query_id]["text"].encode()).hexdigest(), "recall input belongs to a different query")
    root = Path(__file__).resolve().parents[2]
    commit = subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=root, text=True).strip()
    dirty = bool(subprocess.check_output(["git", "status", "--porcelain"], cwd=root).strip())
    candidate = digest(binary)
    require(isinstance(build_receipt, dict) and build_receipt.get("source_dirty") is False
            and build_receipt.get("source_commit") == commit and build_receipt.get("replay_binary_sha256") == candidate
            and build_receipt.get("policy_revision") == POLICY, "selector build receipt mismatch")
    corpus_hash = digest(corpus_path)
    if split == "holdout":
        require(isinstance(development, dict) and approved_development(development), "development must be sealed first")
        require(not dirty and development["source_commit"] == commit and development["replay_binary_sha256"] == candidate and development["corpus_sha256"] == corpus_hash, "candidate changed after development")
    env = {k: v for k, v in os.environ.items() if not k.startswith("CONTEXTSTREAM_GROUNDING_")}
    process = subprocess.run([str(Path(binary).resolve())], input="\n".join(json.dumps(row) for row in rows) + "\n",
                             text=True, capture_output=True, timeout=120, check=True, env=env)
    results = [json.loads(line) for line in process.stdout.splitlines()]
    require(set(indexed(results, "query_id")) == set(queries), "selector did not return complete coverage")
    require(candidate == digest(binary), "selector changed during collection")
    require(commit == subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=root, text=True).strip(), "source changed during collection")
    dirty = dirty or bool(subprocess.check_output(["git", "status", "--porcelain"], cwd=root).strip())
    return {"schema_version": 1, "split": split, "policy_revision": POLICY, "corpus_sha256": corpus_hash,
            "replay_binary_sha256": candidate, "source_commit": commit, "source_dirty": dirty,
            "recalls_sha256": digest(recalls_path), "collected_at_unix": int(time.time()), "results": results,
            "timing_scope": "selector_only_not_end_to_end", "release_qualified": False}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("action", choices=("build", "collect", "evaluate"))
    parser.add_argument("--split", choices=SPLITS)
    parser.add_argument("--corpus", type=Path)
    parser.add_argument("--build-receipt", type=Path)
    parser.add_argument("--development", type=Path)
    parser.add_argument("--recalls", type=Path)
    parser.add_argument("--binary", type=Path)
    parser.add_argument("--labels", type=Path)
    parser.add_argument("--replay", type=Path)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    if args.action == "build":
        args.output.write_text(json.dumps(build_selector(), indent=2) + "\n")
        return 0
    require(args.split and args.corpus, "split and corpus required")
    development = read(args.development) if args.development else None
    # Before opening holdout labels, check the development seal.
    if args.split == "holdout":
        require(isinstance(development, dict) and approved_development(development), "development seal required")
    if args.action == "collect":
        require(args.recalls and args.binary and args.build_receipt, "collect requires --recalls, --binary and --build-receipt")
        result = collect(args.corpus, args.split, args.recalls, args.binary, read(args.build_receipt), development)
        passed = not result["source_dirty"]
    else:
        require(args.labels and args.replay, "evaluate requires --labels and --replay")
        result = evaluate(args.corpus, args.split, read(args.labels), read(args.replay), development)
        passed = result["development_approved"] if args.split == "development" else result["retrieval_qualified"]
    args.output.write_text(json.dumps(result, indent=2) + "\n")
    print(json.dumps({k: result[k] for k in ("split", "policy_revision", "release_qualified")}))
    return 0 if passed else 1


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except (ValueError, OSError, TypeError, KeyError, subprocess.SubprocessError):
        raise SystemExit("Grounding qualification failed: missing, malformed, unsealed or mismatched evidence.")
