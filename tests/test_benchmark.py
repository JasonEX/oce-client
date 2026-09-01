from __future__ import annotations

import json
import shutil
from types import SimpleNamespace

import pytest

from oce_client.benchmark import (
    BENCHMARK_SCHEMA_VERSION,
    BenchmarkCase,
    RepositorySpec,
    _load_outcomes,
    _parse_key_values,
    aggregate,
    compare_results,
    corpus_fingerprint,
    failed_case,
    load_cases,
    load_repositories,
    load_variants,
    parse_retrieved_paths,
    run_benchmark,
    run_lexical_baseline,
    score_case,
    validate_corpus,
    verify_variant_profile,
)
from oce_client.benchmark_lexical import (
    lexical_baseline_profile,
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
    *,
    case: BenchmarkCase | None = None,
    embedding_fingerprint: str = "b" * 64,
    named_variant: bool = True,
    lexical_baseline: bool = False,
) -> dict[str, object]:
    selected = case or _case()
    repositories = {
        "oce": RepositorySpec(
            name="oce",
            url="https://example.test/oce.git",
            revision="a" * 40,
        )
    }
    return {
        "schema_version": BENCHMARK_SCHEMA_VERSION,
        "label": label,
        "corpus_fingerprint": corpus_fingerprint(repositories, [selected]),
        "repositories": {
            "oce": {
                "name": "oce",
                "url": repositories["oce"].url,
                "revision": repositories["oce"].revision,
            }
        },
        "variant": {"name": label} if named_variant and not lexical_baseline else None,
        "baseline": (
            lexical_baseline_profile("ripgrep 14.1.0") if lexical_baseline else None
        ),
        "server_profile": (
            None
            if lexical_baseline
            else {
                "profile": {
                    "state": "compatible",
                    "embedding_fingerprint": embedding_fingerprint,
                }
            }
        ),
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
    assert len(variants) == 9
    assert len(corpus_fingerprint(repositories, cases)) == 64


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
            "embedding_fingerprint": "b" * 64,
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
    index_stats["profile"]["state"] = "incompatible"
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


def test_compare_rejects_corpus_or_embedding_confounds(tmp_path):
    summary = {
        "cases": 1,
        "top1": 0.0,
        "recall_at_10": 0.0,
        "mrr": 0.0,
        "ndcg_at_10": 0.0,
        "agent_solved_rate": None,
        "mean_elapsed_ms": 1,
        "mean_returned_chars": 1,
        "external_model_tokens": None,
        "estimated_external_cost": None,
    }
    baseline = tmp_path / "baseline.json"
    changed = tmp_path / "changed.json"
    baseline.write_text(
        json.dumps(_comparison_payload("baseline", summary)),
        encoding="utf-8",
    )
    changed_case = BenchmarkCase(
        id="case-2",
        repository="oce",
        category="feature",
        query="different query",
        expected_paths=("src/other.py",),
    )
    changed.write_text(
        json.dumps(_comparison_payload("changed", summary, case=changed_case)),
        encoding="utf-8",
    )

    with pytest.raises(ValueError, match="different corpora"):
        compare_results([baseline, changed])

    changed.write_text(
        json.dumps(
            _comparison_payload(
                "changed",
                summary,
                embedding_fingerprint="c" * 64,
            )
        ),
        encoding="utf-8",
    )
    with pytest.raises(ValueError, match="different embedding profiles"):
        compare_results([baseline, changed])
    assert "| changed |" in compare_results(
        [baseline, changed],
        allow_embedding_change=True,
    )


def test_compare_accepts_ad_hoc_results_for_internal_experiments(tmp_path):
    summary = {
        "cases": 1,
        "top1": 0.0,
        "recall_at_10": 0.0,
        "mrr": 0.0,
        "ndcg_at_10": 0.0,
        "agent_solved_rate": None,
        "mean_elapsed_ms": 1,
        "mean_returned_chars": 1,
        "external_model_tokens": None,
        "estimated_external_cost": None,
    }
    named = tmp_path / "named.json"
    ad_hoc = tmp_path / "ad-hoc.json"
    named.write_text(
        json.dumps(_comparison_payload("named", summary)),
        encoding="utf-8",
    )
    ad_hoc.write_text(
        json.dumps(_comparison_payload("ad-hoc", summary, named_variant=False)),
        encoding="utf-8",
    )

    assert "| ad-hoc |" in compare_results([named, ad_hoc])


def test_compare_accepts_lexical_baseline_alongside_named_variant(tmp_path):
    summary = {
        "cases": 1,
        "top1": 1.0,
        "recall_at_10": 1.0,
        "mrr": 1.0,
        "ndcg_at_10": 1.0,
        "agent_solved_rate": None,
        "mean_elapsed_ms": 1,
        "mean_returned_chars": 100,
        "external_model_tokens": {},
        "estimated_external_cost": 0.0,
    }
    lexical = tmp_path / "lexical.json"
    variant = tmp_path / "variant.json"
    lexical.write_text(
        json.dumps(
            _comparison_payload(
                "ripgrep-lexical-v1",
                summary,
                named_variant=False,
                lexical_baseline=True,
            )
        ),
        encoding="utf-8",
    )
    variant.write_text(
        json.dumps(_comparison_payload("dense", summary)),
        encoding="utf-8",
    )

    table = compare_results([lexical, variant])

    assert "| ripgrep-lexical-v1 |" in table
    assert "| dense |" in table


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
    monkeypatch.setattr("oce_client.benchmark.validate_corpus", lambda *args: None)
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
    assert payload["server_profile"] is None
    assert payload["baseline"]["kind"] == "ripgrep_lexical"
    assert payload["baseline"]["agent_workflow"] is False
    assert payload["sync"]["repo"]["admitted_files"] == 2
    assert payload["summary"]["error_cases"] == 0
    assert payload["summary"]["external_model_tokens"] == {}
    assert payload["cases"][0]["retrieved_paths"][0] == "src/context.py"
