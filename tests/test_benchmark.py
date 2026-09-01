from __future__ import annotations

import json
from types import SimpleNamespace

import pytest

from oce_client.benchmark import (
    BenchmarkCase,
    _load_outcomes,
    _parse_key_values,
    aggregate,
    compare_results,
    failed_case,
    load_cases,
    load_repositories,
    load_variants,
    parse_retrieved_paths,
    run_benchmark,
    score_case,
    validate_corpus,
    verify_variant_profile,
)


def _case() -> BenchmarkCase:
    return BenchmarkCase(
        id="case-1",
        repository="oce",
        category="feature",
        query="find feature",
        expected_paths=("src/a.py", "src/b.py"),
    )


def test_checked_in_corpus_is_well_formed_and_has_fifty_cases():
    repositories = load_repositories()
    cases = load_cases()
    variants = load_variants()

    validate_corpus(repositories, cases)

    assert len(repositories) == 2
    assert len(cases) == 50
    assert len({case.id for case in cases}) == 50
    assert len(variants) == 9


def test_variant_profile_must_match_live_server_controls():
    variant = load_variants()["full-no-rerank"]
    index_stats = {
        "runtime": {
            "embedding_enabled": True,
            "semantic_chunking_enabled": True,
            "exact_enabled": True,
            "path_index_enabled": True,
            "source_priority_enabled": True,
            "coverage_selection_enabled": True,
            "query_decomposition_enabled": True,
            "api_rerank_enabled": False,
            "llm_rerank_enabled": False,
            "query_rewrite_enabled": False,
            "intent_classification_enabled": False,
        },
        "query_cache": {"max_entries": 0},
        "profile": {
            "state": "compatible",
            "fingerprint": "a" * 64,
            "schema_version": 1,
            "embedding_enabled": True,
            "embedding_model": "embedding-v1",
            "embedding_dimensions": 1024,
        },
    }

    profile = verify_variant_profile(variant, index_stats)

    assert profile["runtime"]["exact_enabled"] is True
    assert profile["query_cache"]["max_entries"] == 0
    assert profile["profile"]["embedding_model"] == "embedding-v1"

    index_stats["runtime"]["exact_enabled"] = False
    with pytest.raises(ValueError, match="RETRIEVAL_EXACT_ENABLED"):
        verify_variant_profile(variant, index_stats)

    index_stats["runtime"]["exact_enabled"] = True
    index_stats["profile"]["state"] = "stored_unverified"
    with pytest.raises(ValueError, match="index profile state"):
        verify_variant_profile(variant, index_stats)


def test_parse_and_score_retrieval_paths():
    formatted = (
        "The following code sections were retrieved:\n"
        "Path: src/other.py\nLines: 1-2\n"
        "Path: src/a.py\nLines: 3-4\n"
        "Path: src/a.py\nLines: 8-9\n"
        "Path: src/b.py\nLines: 1-1\n"
    )

    paths = parse_retrieved_paths(formatted)
    result = score_case(_case(), paths, returned_chars=len(formatted), elapsed_ms=12)

    assert paths == ("src/other.py", "src/a.py", "src/b.py")
    assert result.top1 == 0.0
    assert result.recall_at_10 == 1.0
    assert result.reciprocal_rank == 0.5
    assert result.ndcg_at_10 == pytest.approx(0.693426, rel=1e-5)


def test_aggregate_preserves_errors_as_zero_score():
    success = score_case(
        _case(),
        ("src/a.py", "src/b.py"),
        returned_chars=100,
        elapsed_ms=20,
        agent_solved=True,
    )
    failure = failed_case(_case(), RuntimeError("unavailable"), agent_solved=False)

    summary = aggregate([success, failure])

    assert summary["cases"] == 2
    assert summary["successful_cases"] == 1
    assert summary["error_cases"] == 1
    assert summary["top1"] == 0.5
    assert summary["agent_solved_rate"] == 0.5
    assert summary["mean_elapsed_ms"] == 20


def test_compare_renders_decision_metrics(tmp_path):
    first = tmp_path / "first.json"
    second = tmp_path / "second.json"
    summary = {
        "cases": 2,
        "top1": 0.5,
        "recall_at_10": 0.75,
        "mrr": 0.6,
        "ndcg_at_10": 0.7,
        "agent_solved_rate": None,
        "mean_elapsed_ms": 25,
        "mean_returned_chars": 1200,
        "external_model_tokens": None,
        "estimated_external_cost": None,
    }
    first.write_text(
        json.dumps({"label": "dense", "summary": summary}), encoding="utf-8"
    )
    second.write_text(
        json.dumps({"label": "rerank", "summary": summary}), encoding="utf-8"
    )

    table = compare_results([first, second])

    assert "| Variant | Cases | Top-1 | Recall@10 |" in table
    assert "| dense | 2 | 50.0% | 75.0% |" in table
    assert "| rerank | 2 | 50.0% | 75.0% |" in table


def test_task_outcomes_reject_unknown_cases(tmp_path):
    outcomes = tmp_path / "outcomes.json"
    outcomes.write_text(json.dumps({"unknown": True}), encoding="utf-8")

    with pytest.raises(ValueError, match="unknown case ids"):
        _load_outcomes(outcomes, {"case-1"})


@pytest.mark.parametrize("value", ["rerank=-1", "rerank=nan", "rerank=inf"])
def test_prices_must_be_finite_and_non_negative(value):
    with pytest.raises(ValueError, match="finite and non-negative"):
        _parse_key_values([value], numeric=True)


def test_run_preserves_sync_failures_as_case_errors(tmp_path, monkeypatch):
    repositories = tmp_path / "repositories.json"
    repositories.write_text(
        json.dumps(
            {
                "repo": {
                    "url": "https://example.invalid/repo.git",
                    "revision": "a" * 40,
                }
            }
        ),
        encoding="utf-8",
    )
    cases = tmp_path / "cases.jsonl"
    cases.write_text(
        json.dumps(
            {
                "id": "sync-failure",
                "repository": "repo",
                "category": "feature",
                "query": "find the feature",
                "expected_paths": ["src/feature.py"],
            }
        )
        + "\n",
        encoding="utf-8",
    )
    workdir = tmp_path / "workspaces"
    workdir.mkdir()

    class FailingRuntime:
        def __init__(self, settings):
            self.settings = settings

        def context(self):
            return self

        def sync(self):
            raise RuntimeError("sync unavailable with secret-key")

        def close(self):
            pass

    monkeypatch.setattr("oce_client.benchmark.validate_corpus", lambda *args: None)
    monkeypatch.setattr("oce_client.benchmark.ClientRuntime", FailingRuntime)
    monkeypatch.setenv("OCE_API_KEY", "secret-key")
    args = SimpleNamespace(
        repositories=repositories,
        cases=cases,
        repository=[],
        category=[],
        case=[],
        workdir=workdir,
        task_outcomes=None,
        metadata=[],
        price=[],
        api_url="http://127.0.0.1:8986",
        state_dir=None,
        metrics_settle_seconds=0,
        variant=None,
        label="sync-failure",
    )

    payload = run_benchmark(args)

    assert payload["sync"]["repo"]["status"] == "error"
    assert payload["summary"]["error_cases"] == 1
    assert payload["summary"]["top1"] == 0.0
    assert payload["cases"][0]["status"] == "error"
    assert payload["cases"][0]["error"] == "sync unavailable with [REDACTED]"
