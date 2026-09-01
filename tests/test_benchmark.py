from __future__ import annotations

import json
import shutil
from types import SimpleNamespace

import pytest

from oce_benchmark.benchmark import (
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
    run_lexical_baseline,
    score_case,
    validate_corpus,
)
from oce_benchmark.benchmark_lexical import (
    lexical_query_terms,
)


def _case() -> BenchmarkCase:
    return BenchmarkCase(
        id="case-1",
        repository="oce",
        category="feature",
        query="find feature",
        expected_paths=("src/a.py", "src/b.py"),
    )


def _comparison_payload(
    label: str,
    summary: dict[str, object],
) -> dict[str, object]:
    selected = _case()
    return {
        "label": label,
        "summary": summary,
        "cases": [
            {
                "id": selected.id,
                "repository": selected.repository,
                "category": selected.category,
                "query": selected.query,
                "expected_paths": list(selected.expected_paths),
            }
        ],
    }


def test_checked_in_corpus_is_well_formed_and_has_fifty_cases():
    repositories = load_repositories()
    cases = load_cases()
    variants = load_variants()

    validate_corpus(repositories, cases)

    assert len(repositories) == 2
    assert len(cases) == 50
    assert len({case.id for case in cases}) == 50

    intent_off = variants["full-no-rerank"].environment
    intent_on = variants["full-no-rerank-intent"].environment
    differing_fields = {
        key
        for key in intent_off.keys() | intent_on.keys()
        if intent_off.get(key) != intent_on.get(key)
    }
    assert differing_fields == {"RETRIEVAL_INTENT_CLASSIFICATION_ENABLED"}
    assert intent_off["RETRIEVAL_INTENT_CLASSIFICATION_ENABLED"] == "false"
    assert intent_on["RETRIEVAL_INTENT_CLASSIFICATION_ENABLED"] == "true"


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


def test_lexical_query_terms_keep_symbols_and_remove_question_scaffolding():
    terms = {
        term.casefold()
        for term in lexical_query_terms(
            "Where is WorkspaceContext and model_credentials 定义在哪里？"
        )
    }

    assert {"workspacecontext", "workspace", "context"} <= terms
    assert {"model_credentials", "model", "credentials"} <= terms
    assert "where" not in terms
    assert "defined" not in terms


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
        json.dumps(_comparison_payload("dense", summary)), encoding="utf-8"
    )
    second.write_text(
        json.dumps(_comparison_payload("rerank", summary)), encoding="utf-8"
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

    monkeypatch.setattr("oce_benchmark.benchmark.validate_corpus", lambda *args: None)
    monkeypatch.setattr(
        "oce_benchmark.benchmark.resolve_client_binary",
        lambda configured=None: "oce-client",
    )
    monkeypatch.setattr(
        "oce_benchmark.benchmark._run_client_json",
        lambda *args, **kwargs: (_ for _ in ()).throw(
            RuntimeError("sync unavailable with secret-key")
        ),
    )
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
        client_binary=None,
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


@pytest.mark.skipif(shutil.which("rg") is None, reason="ripgrep is not installed")
def test_run_lexical_baseline_uses_admitted_files_and_needs_no_server(
    tmp_path, monkeypatch
):
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
                "id": "lexical",
                "repository": "repo",
                "category": "symbol_definition",
                "query": "Where is WorkspaceContext defined?",
                "expected_paths": ["src/context.py"],
            }
        )
        + "\n",
        encoding="utf-8",
    )
    workdir = tmp_path / "workspaces"
    root = workdir / "repo"
    (root / "src").mkdir(parents=True)
    (root / "src/context.py").write_text(
        "class WorkspaceContext:\n    pass\n",
        encoding="utf-8",
    )
    (root / "noise.py").write_text(
        "context = 'workspace context context'\n",
        encoding="utf-8",
    )
    (root / ".env").write_text(
        "WorkspaceContext=must-not-be-searched\n",
        encoding="utf-8",
    )
    monkeypatch.setattr("oce_benchmark.benchmark.validate_corpus", lambda *args: None)
    args = SimpleNamespace(
        repositories=repositories,
        cases=cases,
        repository=[],
        category=[],
        case=[],
        workdir=workdir,
        metadata=[],
    )

    payload = run_lexical_baseline(args)

    assert payload["variant"] is None
    assert payload["baseline"]["kind"] == "ripgrep_lexical"
    assert payload["baseline"]["agent_workflow"] is False
    assert payload["sync"]["repo"]["admitted_files"] == 2
    assert payload["summary"]["error_cases"] == 0
    assert payload["summary"]["external_model_tokens"] == {}
    assert payload["cases"][0]["retrieved_paths"][0] == "src/context.py"
