from __future__ import annotations

import argparse
import hashlib
import json
import math
import os
import subprocess
import tempfile
import time
from dataclasses import asdict, dataclass
from datetime import datetime, timezone
from pathlib import Path
from statistics import fmean
from typing import Iterable, Sequence

import httpx

from oce_client.runtime import ClientRuntime, ClientSettings


ROOT = Path(__file__).resolve().parents[2] / "benchmarks"
DEFAULT_REPOSITORIES = ROOT / "repositories.json"
DEFAULT_CASES = ROOT / "cases.jsonl"
DEFAULT_VARIANTS = ROOT / "variants.json"
BENCHMARK_SCHEMA_VERSION = 3

VARIANT_PROFILE_FIELDS = {
    "EMBED_ENABLED": ("runtime", "embedding_enabled"),
    "CHUNKING_SEMANTIC_ENABLED": ("runtime", "semantic_chunking_enabled"),
    "RETRIEVAL_EXACT_ENABLED": ("runtime", "exact_enabled"),
    "RETRIEVAL_PATH_INDEX_ENABLED": ("runtime", "path_index_enabled"),
    "RETRIEVAL_SOURCE_PRIORITY_ENABLED": (
        "runtime",
        "source_priority_enabled",
    ),
    "RETRIEVAL_COVERAGE_SELECTION_ENABLED": (
        "runtime",
        "coverage_selection_enabled",
    ),
    "RETRIEVAL_QUERY_DECOMPOSITION_ENABLED": (
        "runtime",
        "query_decomposition_enabled",
    ),
    "RERANK_ENABLED": ("runtime", "api_rerank_enabled"),
    "LLM_RERANK_ENABLED": ("runtime", "llm_rerank_enabled"),
    "RETRIEVAL_QUERY_REWRITE_ENABLED": ("runtime", "query_rewrite_enabled"),
    "RETRIEVAL_INTENT_CLASSIFICATION_ENABLED": (
        "runtime",
        "intent_classification_enabled",
    ),
    "EMBED_QUERY_CACHE_MAX_ENTRIES": ("query_cache", "max_entries"),
}
_BOOLEAN_VARIANT_FIELDS = frozenset(VARIANT_PROFILE_FIELDS) - {
    "EMBED_QUERY_CACHE_MAX_ENTRIES"
}


@dataclass(frozen=True)
class RepositorySpec:
    name: str
    url: str
    revision: str


@dataclass(frozen=True)
class BenchmarkCase:
    id: str
    repository: str
    category: str
    query: str
    expected_paths: tuple[str, ...]


@dataclass(frozen=True)
class BenchmarkVariant:
    name: str
    description: str
    environment: dict[str, str]


@dataclass(frozen=True)
class CaseResult:
    id: str
    repository: str
    category: str
    query: str
    expected_paths: tuple[str, ...]
    status: str
    retrieved_paths: tuple[str, ...] = ()
    top1: float = 0.0
    recall_at_10: float = 0.0
    reciprocal_rank: float = 0.0
    ndcg_at_10: float = 0.0
    returned_chars: int = 0
    elapsed_ms: int = 0
    agent_solved: bool | None = None
    error_type: str | None = None
    error: str | None = None


def _read_json(path: Path) -> object:
    return json.loads(path.read_text(encoding="utf-8"))


def _json_fingerprint(value: object) -> str:
    encoded = json.dumps(
        value,
        ensure_ascii=False,
        separators=(",", ":"),
        sort_keys=True,
    ).encode("utf-8")
    return hashlib.sha256(encoded).hexdigest()


def _file_fingerprint(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as source:
        for block in iter(lambda: source.read(1024 * 1024), b""):
            digest.update(block)
    return digest.hexdigest()


def corpus_fingerprint(
    repositories: dict[str, RepositorySpec],
    cases: Sequence[BenchmarkCase],
) -> str:
    selected_repositories = sorted({case.repository for case in cases})
    material = {
        "repositories": {
            name: {
                "url": repositories[name].url,
                "revision": repositories[name].revision,
            }
            for name in selected_repositories
        },
        "cases": [
            {
                "id": case.id,
                "repository": case.repository,
                "category": case.category,
                "query": case.query,
                "expected_paths": list(case.expected_paths),
            }
            for case in sorted(cases, key=lambda item: item.id)
        ],
    }
    return _json_fingerprint(material)


def load_repositories(path: Path = DEFAULT_REPOSITORIES) -> dict[str, RepositorySpec]:
    payload = _read_json(path)
    if not isinstance(payload, dict):
        raise ValueError("repositories manifest must be a JSON object")
    repositories: dict[str, RepositorySpec] = {}
    for name, value in payload.items():
        if not isinstance(name, str) or not isinstance(value, dict):
            raise ValueError("repository entries must be named JSON objects")
        spec = RepositorySpec(
            name=name,
            url=str(value.get("url", "")),
            revision=str(value.get("revision", "")),
        )
        if not spec.url or len(spec.revision) != 40:
            raise ValueError(
                f"repository {name!r} needs a URL and full 40-char revision"
            )
        repositories[name] = spec
    return repositories


def load_cases(path: Path = DEFAULT_CASES) -> list[BenchmarkCase]:
    cases: list[BenchmarkCase] = []
    seen: set[str] = set()
    for line_number, raw in enumerate(path.read_text(encoding="utf-8").splitlines(), 1):
        if not raw.strip():
            continue
        try:
            value = json.loads(raw)
        except json.JSONDecodeError as exc:
            raise ValueError(f"{path}:{line_number}: invalid JSON") from exc
        if not isinstance(value, dict):
            raise ValueError(f"{path}:{line_number}: case must be an object")
        case = BenchmarkCase(
            id=str(value.get("id", "")),
            repository=str(value.get("repository", "")),
            category=str(value.get("category", "")),
            query=str(value.get("query", "")),
            expected_paths=tuple(str(item) for item in value.get("expected_paths", ())),
        )
        if not case.id or case.id in seen:
            raise ValueError(
                f"{path}:{line_number}: missing or duplicate case id {case.id!r}"
            )
        if not case.repository or not case.category or not case.query.strip():
            raise ValueError(
                f"{path}:{line_number}: repository/category/query is required"
            )
        if not case.expected_paths or any(
            not item or Path(item).is_absolute() or ".." in Path(item).parts
            for item in case.expected_paths
        ):
            raise ValueError(
                f"{path}:{line_number}: expected_paths must be safe relative paths"
            )
        seen.add(case.id)
        cases.append(case)
    if not cases:
        raise ValueError("benchmark corpus is empty")
    return cases


def load_variants(path: Path = DEFAULT_VARIANTS) -> dict[str, BenchmarkVariant]:
    payload = _read_json(path)
    if not isinstance(payload, dict) or not payload:
        raise ValueError("variants manifest must be a non-empty JSON object")
    variants: dict[str, BenchmarkVariant] = {}
    required = set(VARIANT_PROFILE_FIELDS)
    for name, value in payload.items():
        if not isinstance(name, str) or not name or not isinstance(value, dict):
            raise ValueError("variant entries must be named JSON objects")
        description = value.get("description")
        environment = value.get("environment")
        if not isinstance(description, str) or not description.strip():
            raise ValueError(f"variant {name!r} needs a description")
        if not isinstance(environment, dict) or any(
            not isinstance(key, str) or not isinstance(item, str)
            for key, item in environment.items()
        ):
            raise ValueError(
                f"variant {name!r} environment must map strings to strings"
            )
        missing = sorted(required - environment.keys())
        unexpected = sorted(environment.keys() - required)
        if missing or unexpected:
            raise ValueError(
                f"variant {name!r} has missing fields {missing} "
                f"and unexpected fields {unexpected}"
            )
        invalid_booleans = sorted(
            key
            for key in _BOOLEAN_VARIANT_FIELDS
            if environment[key] not in {"true", "false"}
        )
        if invalid_booleans:
            raise ValueError(
                f"variant {name!r} boolean fields must use true/false: "
                f"{invalid_booleans}"
            )
        try:
            cache_entries = int(environment["EMBED_QUERY_CACHE_MAX_ENTRIES"])
        except ValueError as exc:
            raise ValueError(
                f"variant {name!r} cache capacity must be a non-negative integer"
            ) from exc
        if (
            cache_entries < 0
            or str(cache_entries) != environment["EMBED_QUERY_CACHE_MAX_ENTRIES"]
        ):
            raise ValueError(
                f"variant {name!r} cache capacity must be a non-negative integer"
            )
        variants[name] = BenchmarkVariant(
            name=name,
            description=description.strip(),
            environment=dict(environment),
        )
    return variants


def parse_retrieved_paths(formatted_retrieval: str) -> tuple[str, ...]:
    paths: list[str] = []
    for line in formatted_retrieval.splitlines():
        if not line.startswith("Path: "):
            continue
        path = line.removeprefix("Path: ").strip()
        if path and path not in paths:
            paths.append(path)
    return tuple(paths)


def score_case(
    case: BenchmarkCase,
    retrieved_paths: Sequence[str],
    *,
    returned_chars: int,
    elapsed_ms: int,
    agent_solved: bool | None = None,
) -> CaseResult:
    ranked = list(retrieved_paths[:10])
    relevant = set(case.expected_paths)
    matched = [path in relevant for path in ranked]
    first_rank = next((index for index, hit in enumerate(matched, 1) if hit), None)
    dcg = sum(1.0 / math.log2(index + 1) for index, hit in enumerate(matched, 1) if hit)
    ideal_hits = min(len(relevant), 10)
    ideal_dcg = sum(1.0 / math.log2(index + 1) for index in range(1, ideal_hits + 1))
    return CaseResult(
        id=case.id,
        repository=case.repository,
        category=case.category,
        query=case.query,
        expected_paths=case.expected_paths,
        status="ok",
        retrieved_paths=tuple(retrieved_paths),
        top1=float(bool(matched and matched[0])),
        recall_at_10=sum(matched) / len(relevant),
        reciprocal_rank=0.0 if first_rank is None else 1.0 / first_rank,
        ndcg_at_10=0.0 if ideal_dcg == 0 else dcg / ideal_dcg,
        returned_chars=returned_chars,
        elapsed_ms=elapsed_ms,
        agent_solved=agent_solved,
    )


def failed_case(
    case: BenchmarkCase,
    exc: Exception,
    *,
    agent_solved: bool | None = None,
) -> CaseResult:
    return CaseResult(
        id=case.id,
        repository=case.repository,
        category=case.category,
        query=case.query,
        expected_paths=case.expected_paths,
        status="error",
        agent_solved=agent_solved,
        error_type=type(exc).__name__,
        error=_safe_error_message(exc),
    )


def _safe_error_message(exc: Exception) -> str:
    message = str(exc)
    for name in ("OCE_API_KEY", "OCE_ADMIN_API_KEY"):
        if secret := os.environ.get(name):
            message = message.replace(secret, "[REDACTED]")
    return message


def aggregate(results: Sequence[CaseResult]) -> dict[str, object]:
    if not results:
        raise ValueError("cannot aggregate an empty result set")
    solved = [item.agent_solved for item in results if item.agent_solved is not None]
    successful = [item for item in results if item.status == "ok"]
    return {
        "cases": len(results),
        "successful_cases": len(successful),
        "error_cases": len(results) - len(successful),
        "top1": fmean(item.top1 for item in results),
        "recall_at_10": fmean(item.recall_at_10 for item in results),
        "mrr": fmean(item.reciprocal_rank for item in results),
        "ndcg_at_10": fmean(item.ndcg_at_10 for item in results),
        "mean_returned_chars": fmean(item.returned_chars for item in results),
        "mean_elapsed_ms": (
            fmean(item.elapsed_ms for item in successful) if successful else None
        ),
        "agent_solved_rate": (
            sum(bool(value) for value in solved) / len(solved) if solved else None
        ),
        "agent_solved_cases": len(solved),
    }


def _git(root: Path, *args: str) -> str:
    completed = subprocess.run(
        ["git", "-C", str(root), *args],
        check=True,
        capture_output=True,
        text=True,
    )
    return completed.stdout.strip()


def validate_workspace(root: Path, spec: RepositorySpec) -> None:
    if not root.is_dir():
        raise ValueError(f"workspace is not a directory: {root}")
    revision = _git(root, "rev-parse", "HEAD")
    if revision != spec.revision:
        raise ValueError(
            f"{spec.name} is at {revision}; benchmark requires {spec.revision}"
        )
    dirty = _git(root, "status", "--porcelain")
    if dirty:
        raise ValueError(f"benchmark workspace is dirty: {root}")


def validate_corpus(
    repositories: dict[str, RepositorySpec],
    cases: Sequence[BenchmarkCase],
    workdir: Path | None = None,
) -> None:
    unknown = sorted({case.repository for case in cases} - repositories.keys())
    if unknown:
        raise ValueError(f"cases reference unknown repositories: {unknown}")
    if workdir is None:
        return
    for name, spec in repositories.items():
        relevant = [case for case in cases if case.repository == name]
        if not relevant:
            continue
        root = (workdir / name).resolve()
        validate_workspace(root, spec)
        missing = sorted(
            {
                path
                for case in relevant
                for path in case.expected_paths
                if not (root / path).is_file()
            }
        )
        if missing:
            raise ValueError(
                f"{name} corpus paths do not exist at pinned revision: {missing}"
            )


def prepare_workspaces(
    repositories: dict[str, RepositorySpec],
    workdir: Path,
) -> None:
    workdir.mkdir(parents=True, exist_ok=True)
    for spec in repositories.values():
        target = (workdir / spec.name).resolve()
        if target.exists():
            validate_workspace(target, spec)
            continue
        subprocess.run(
            [
                "git",
                "clone",
                "--filter=blob:none",
                "--no-checkout",
                spec.url,
                str(target),
            ],
            check=True,
        )
        subprocess.run(
            ["git", "-C", str(target), "checkout", "--detach", spec.revision],
            check=True,
        )


def _load_outcomes(
    path: Path | None,
    known_case_ids: set[str],
) -> dict[str, bool]:
    if path is None:
        return {}
    value = _read_json(path)
    if not isinstance(value, dict) or any(
        not isinstance(item, bool) for item in value.values()
    ):
        raise ValueError(
            "task outcomes must be a JSON object mapping case ids to booleans"
        )
    outcomes = {str(key): item for key, item in value.items()}
    unknown = sorted(outcomes.keys() - known_case_ids)
    if unknown:
        raise ValueError(f"task outcomes reference unknown case ids: {unknown}")
    return outcomes


def _parse_key_values(
    values: Iterable[str], *, numeric: bool = False
) -> dict[str, object]:
    output: dict[str, object] = {}
    for value in values:
        key, separator, raw = value.partition("=")
        key = key.strip()
        if not separator or not key:
            raise ValueError(f"expected KEY=VALUE, got {value!r}")
        if key in output:
            raise ValueError(f"duplicate key {key!r}")
        if not numeric:
            output[key] = raw
            continue
        number = float(raw)
        if not math.isfinite(number) or number < 0:
            raise ValueError(
                f"numeric value for {key!r} must be finite and non-negative"
            )
        output[key] = number
    return output


def _admin_stats(api_url: str, admin_key: str) -> dict[str, object]:
    response = httpx.get(
        f"{api_url.rstrip('/')}/admin/stats",
        params={"window_hours": 24},
        headers={"Authorization": f"Bearer {admin_key}"},
        timeout=30.0,
    )
    response.raise_for_status()
    value = response.json()
    if not isinstance(value, dict):
        raise ValueError("admin stats response must be an object")
    return value


def _admin_index_stats(api_url: str, admin_key: str) -> dict[str, object]:
    response = httpx.get(
        f"{api_url.rstrip('/')}/admin/index-stats",
        headers={"Authorization": f"Bearer {admin_key}"},
        timeout=30.0,
    )
    response.raise_for_status()
    value = response.json()
    if not isinstance(value, dict):
        raise ValueError("admin index stats response must be an object")
    return value


def verify_variant_profile(
    variant: BenchmarkVariant,
    index_stats: dict[str, object],
) -> dict[str, object]:
    """Verify that a named result is backed by the declared server controls."""
    mismatches: list[str] = []
    actual_profile: dict[str, object] = {"runtime": {}, "query_cache": {}}
    for environment_name, (section_name, field_name) in VARIANT_PROFILE_FIELDS.items():
        section = index_stats.get(section_name)
        actual = section.get(field_name) if isinstance(section, dict) else None
        raw_expected = variant.environment[environment_name]
        expected: object = (
            raw_expected == "true"
            if environment_name in _BOOLEAN_VARIANT_FIELDS
            else int(raw_expected)
        )
        profile_section = actual_profile[section_name]
        assert isinstance(profile_section, dict)
        profile_section[field_name] = actual
        if type(actual) is not type(expected) or actual != expected:
            mismatches.append(
                f"{environment_name} expected {expected!r}, got {actual!r}"
            )
    index_profile = index_stats.get("profile")
    if not isinstance(index_profile, dict):
        mismatches.append("index profile is missing")
    else:
        actual_profile["profile"] = {
            key: index_profile.get(key)
            for key in (
                "state",
                "fingerprint",
                "schema_version",
                "embedding_enabled",
                "embedding_fingerprint",
                "embedding_model",
                "embedding_dimensions",
            )
        }
        if index_profile.get("state") != "compatible":
            mismatches.append(
                "index profile state expected 'compatible', "
                f"got {index_profile.get('state')!r}"
            )
        fingerprint = index_profile.get("fingerprint")
        if not isinstance(fingerprint, str) or len(fingerprint) != 64:
            mismatches.append("index profile fingerprint is missing or invalid")
        if index_profile.get("embedding_enabled") is not True:
            mismatches.append("persisted index profile does not enable embedding")
        embedding_fingerprint = index_profile.get("embedding_fingerprint")
        if (
            not isinstance(embedding_fingerprint, str)
            or len(embedding_fingerprint) != 64
        ):
            mismatches.append("persisted embedding fingerprint is missing or invalid")
        if not isinstance(index_profile.get("embedding_model"), str):
            mismatches.append("persisted index profile has no embedding model")
        dimensions = index_profile.get("embedding_dimensions")
        if (
            not isinstance(dimensions, int)
            or isinstance(dimensions, bool)
            or dimensions < 1
        ):
            mismatches.append(
                "persisted index profile has invalid embedding dimensions"
            )
    if mismatches:
        raise ValueError(
            f"server profile does not match variant {variant.name!r}: "
            + "; ".join(mismatches)
        )
    return actual_profile


def _token_totals(stats: dict[str, object]) -> dict[str, dict[str, int]]:
    output: dict[str, dict[str, int]] = {}
    tokens = stats.get("tokens", [])
    if not isinstance(tokens, list):
        return output
    for item in tokens:
        if not isinstance(item, dict) or not item.get("kind"):
            continue
        output[str(item["kind"])] = {
            "calls": int(item.get("calls", 0)),
            "prompt_tokens": int(item.get("prompt_tokens", 0)),
            "completion_tokens": int(item.get("completion_tokens", 0)),
            "total_tokens": int(item.get("total_tokens", 0)),
        }
    return output


def _token_delta(
    before: dict[str, object],
    after: dict[str, object],
) -> dict[str, dict[str, int]]:
    first = _token_totals(before)
    second = _token_totals(after)
    output: dict[str, dict[str, int]] = {}
    for kind in sorted(first.keys() | second.keys()):
        output[kind] = {
            field: second.get(kind, {}).get(field, 0)
            - first.get(kind, {}).get(field, 0)
            for field in ("calls", "prompt_tokens", "completion_tokens", "total_tokens")
        }
    return output


def _estimate_cost(
    token_delta: dict[str, dict[str, int]],
    prices: dict[str, object],
) -> float | None:
    priced = [kind for kind in token_delta if kind in prices]
    if not priced:
        return None
    return sum(
        token_delta[kind]["total_tokens"] * float(prices[kind]) / 1_000_000
        for kind in priced
    )


def _selected_cases(
    cases: Sequence[BenchmarkCase], args: argparse.Namespace
) -> list[BenchmarkCase]:
    selected = list(cases)
    if args.repository:
        selected = [case for case in selected if case.repository in args.repository]
    if args.category:
        selected = [case for case in selected if case.category in args.category]
    if args.case:
        selected = [case for case in selected if case.id in args.case]
    if not selected:
        raise ValueError("case filters selected no benchmark cases")
    return selected


def run_benchmark(args: argparse.Namespace) -> dict[str, object]:
    variant_name = getattr(args, "variant", None)
    label = getattr(args, "label", None)
    if bool(variant_name) == bool(label):
        raise ValueError("provide exactly one of variant or label")
    variant: BenchmarkVariant | None = None
    if variant_name:
        variants = load_variants(getattr(args, "variants", DEFAULT_VARIANTS))
        try:
            variant = variants[variant_name]
        except KeyError as exc:
            raise ValueError(f"unknown benchmark variant: {variant_name}") from exc
        label = variant.name

    repositories = load_repositories(args.repositories)
    corpus = load_cases(args.cases)
    cases = _selected_cases(corpus, args)
    workdir = args.workdir.resolve()
    validate_corpus(repositories, cases, workdir)
    outcomes = _load_outcomes(args.task_outcomes, {case.id for case in corpus})
    metadata = _parse_key_values(args.metadata)
    prices = _parse_key_values(args.price, numeric=True)
    variants_path = Path(getattr(args, "variants", DEFAULT_VARIANTS))
    manifest_hashes = {
        "repositories": _file_fingerprint(Path(args.repositories)),
        "cases": _file_fingerprint(Path(args.cases)),
        "variants": _file_fingerprint(variants_path),
    }
    api_url = args.api_url or os.environ.get("OCE_API_URL", "http://127.0.0.1:8986")
    api_key = os.environ.get("OCE_API_KEY", "sk-opencontextengine")
    admin_key = os.environ.get("OCE_ADMIN_API_KEY")
    server_profile: dict[str, object] | None = None
    if variant is not None:
        if not admin_key:
            raise ValueError(
                "OCE_ADMIN_API_KEY is required to verify a named benchmark variant"
            )
        server_profile = verify_variant_profile(
            variant,
            _admin_index_stats(api_url, admin_key),
        )

    state_context = (
        tempfile.TemporaryDirectory(prefix="oce-benchmark-")
        if args.state_dir is None
        else None
    )
    state_dir = (
        Path(state_context.name)
        if state_context is not None
        else args.state_dir.resolve()
    )
    state_dir.mkdir(parents=True, exist_ok=True)
    runtimes: dict[str, ClientRuntime] = {}
    sync: dict[str, object] = {}
    sync_errors: dict[str, Exception] = {}
    results: list[CaseResult] = []
    stats_before: dict[str, object] | None = None
    stats_after: dict[str, object] | None = None
    try:
        for name in sorted({case.repository for case in cases}):
            started = time.perf_counter()
            try:
                settings = ClientSettings(
                    root=(workdir / name).resolve(),
                    api_url=api_url,
                    api_key=api_key,
                    state_path=state_dir / f"{name}.sqlite3",
                )
                runtime = ClientRuntime(settings)
                runtimes[name] = runtime
                sync_result = runtime.context().sync()
            except Exception as exc:
                sync_errors[name] = exc
                sync[name] = {
                    "status": "error",
                    "elapsed_ms": int((time.perf_counter() - started) * 1000),
                    "error_type": type(exc).__name__,
                    "error": _safe_error_message(exc),
                }
            else:
                sync[name] = {
                    "status": "ok",
                    "elapsed_ms": int((time.perf_counter() - started) * 1000),
                    "uploaded_blobs": len(sync_result.uploaded_blob_names),
                    "checkpoint_id_present": bool(sync_result.checkpoint_id),
                }

        if admin_key:
            time.sleep(args.metrics_settle_seconds)
            stats_before = _admin_stats(api_url, admin_key)

        for case in cases:
            solved = outcomes.get(case.id)
            if error := sync_errors.get(case.repository):
                results.append(failed_case(case, error, agent_solved=solved))
                continue
            try:
                retrieval = runtimes[case.repository].context().retrieve(case.query)
                paths = parse_retrieved_paths(retrieval.formatted_retrieval)
                results.append(
                    score_case(
                        case,
                        paths,
                        returned_chars=len(retrieval.formatted_retrieval),
                        elapsed_ms=retrieval.elapsed_ms,
                        agent_solved=solved,
                    )
                )
            except Exception as exc:
                results.append(failed_case(case, exc, agent_solved=solved))

        if admin_key:
            time.sleep(args.metrics_settle_seconds)
            stats_after = _admin_stats(api_url, admin_key)
    finally:
        for runtime in runtimes.values():
            runtime.close()
        if state_context is not None:
            state_context.cleanup()

    token_delta = (
        _token_delta(stats_before, stats_after)
        if stats_before is not None and stats_after is not None
        else None
    )
    summary = aggregate(results)
    summary["external_model_tokens"] = token_delta
    summary["estimated_external_cost"] = (
        _estimate_cost(token_delta, prices) if token_delta is not None else None
    )
    return {
        "schema_version": BENCHMARK_SCHEMA_VERSION,
        "label": label,
        "created_at": datetime.now(timezone.utc).isoformat(),
        "api_url": api_url,
        "corpus_fingerprint": corpus_fingerprint(repositories, cases),
        "manifest_hashes": manifest_hashes,
        "repositories": {
            name: asdict(repositories[name])
            for name in sorted({case.repository for case in cases})
        },
        "variant": asdict(variant) if variant is not None else None,
        "server_profile": server_profile,
        "metadata": metadata,
        "sync": sync,
        "summary": summary,
        "cases": [asdict(item) for item in results],
    }


def _format_value(value: object, *, percent: bool = False) -> str:
    if value is None:
        return "—"
    if percent:
        return f"{float(value) * 100:.1f}%"
    if isinstance(value, float):
        return f"{value:.2f}"
    return str(value)


def _result_corpus_fingerprint(payload: dict[str, object], path: Path) -> str:
    raw_repositories = payload.get("repositories")
    raw_cases = payload.get("cases")
    if not isinstance(raw_repositories, dict) or not isinstance(raw_cases, list):
        raise ValueError(f"result has no reproducible corpus material: {path}")
    repositories: dict[str, RepositorySpec] = {}
    for name, value in raw_repositories.items():
        if not isinstance(name, str) or not isinstance(value, dict):
            raise ValueError(f"invalid result repository material: {path}")
        repositories[name] = RepositorySpec(
            name=name,
            url=str(value.get("url", "")),
            revision=str(value.get("revision", "")),
        )
        if not repositories[name].url or len(repositories[name].revision) != 40:
            raise ValueError(f"invalid result repository identity: {path}")
    cases: list[BenchmarkCase] = []
    for value in raw_cases:
        if not isinstance(value, dict) or not isinstance(
            value.get("expected_paths"), (list, tuple)
        ):
            raise ValueError(f"invalid result case material: {path}")
        cases.append(
            BenchmarkCase(
                id=str(value.get("id", "")),
                repository=str(value.get("repository", "")),
                category=str(value.get("category", "")),
                query=str(value.get("query", "")),
                expected_paths=tuple(str(item) for item in value["expected_paths"]),
            )
        )
        if (
            not cases[-1].id
            or not cases[-1].repository
            or not cases[-1].query.strip()
            or not cases[-1].expected_paths
        ):
            raise ValueError(f"invalid result case identity: {path}")
    if not cases or len({case.id for case in cases}) != len(cases):
        raise ValueError(f"result cases are empty or duplicated: {path}")
    try:
        return corpus_fingerprint(repositories, cases)
    except KeyError as exc:
        raise ValueError(f"result references an unknown repository: {path}") from exc


def _comparison_identity(
    payload: dict[str, object],
    path: Path,
    *,
    allow_unverified: bool,
) -> tuple[str, str | None, str]:
    if payload.get("schema_version") != BENCHMARK_SCHEMA_VERSION:
        raise ValueError(
            f"unsupported benchmark schema in {path}; rerun with schema "
            f"{BENCHMARK_SCHEMA_VERSION}"
        )
    recorded_corpus = payload.get("corpus_fingerprint")
    computed_corpus = _result_corpus_fingerprint(payload, path)
    if recorded_corpus != computed_corpus:
        raise ValueError(
            f"result corpus fingerprint does not match its contents: {path}"
        )

    label = str(payload.get("label", path.stem))
    variant = payload.get("variant")
    server_profile = payload.get("server_profile")
    profile = (
        server_profile.get("profile") if isinstance(server_profile, dict) else None
    )
    if isinstance(variant, dict):
        if not isinstance(profile, dict) or profile.get("state") != "compatible":
            raise ValueError(f"named variant has no compatible server profile: {path}")
        embedding_fingerprint = profile.get("embedding_fingerprint")
        if (
            not isinstance(embedding_fingerprint, str)
            or len(embedding_fingerprint) != 64
        ):
            raise ValueError(f"named variant has no embedding fingerprint: {path}")
    else:
        if not allow_unverified:
            raise ValueError(
                f"unverified ad hoc result cannot be compared: {path}; "
                "pass --allow-unverified to override"
            )
        embedding_fingerprint = (
            profile.get("embedding_fingerprint") if isinstance(profile, dict) else None
        )
        if not isinstance(embedding_fingerprint, str):
            embedding_fingerprint = None
    return computed_corpus, embedding_fingerprint, label


def compare_results(
    paths: Sequence[Path],
    *,
    allow_unverified: bool = False,
    allow_embedding_change: bool = False,
) -> str:
    headers = (
        "Variant",
        "Cases",
        "Top-1",
        "Recall@10",
        "MRR",
        "nDCG@10",
        "Agent solved",
        "Latency ms",
        "Returned chars",
        "External tokens",
        "Est. cost",
    )
    rows = []
    expected_corpus: str | None = None
    expected_embedding: str | None = None
    labels: set[str] = set()
    for path in paths:
        payload = _read_json(path)
        if not isinstance(payload, dict) or not isinstance(
            payload.get("summary"), dict
        ):
            raise ValueError(f"invalid benchmark result: {path}")
        corpus, embedding, label = _comparison_identity(
            payload,
            path,
            allow_unverified=allow_unverified,
        )
        if expected_corpus is None:
            expected_corpus = corpus
        elif corpus != expected_corpus:
            raise ValueError(
                f"benchmark results use different corpora or case filters: {path}"
            )
        if embedding is not None:
            if expected_embedding is None:
                expected_embedding = embedding
            elif embedding != expected_embedding and not allow_embedding_change:
                raise ValueError(
                    f"benchmark results use different embedding profiles: {path}; "
                    "pass --allow-embedding-change for an explicit model comparison"
                )
        if label in labels:
            raise ValueError(f"duplicate benchmark label {label!r}: {path}")
        labels.add(label)
        summary = payload["summary"]
        token_data = summary.get("external_model_tokens")
        total_tokens = (
            sum(int(value.get("total_tokens", 0)) for value in token_data.values())
            if isinstance(token_data, dict)
            else None
        )
        rows.append(
            (
                label,
                _format_value(summary.get("cases")),
                _format_value(summary.get("top1"), percent=True),
                _format_value(summary.get("recall_at_10"), percent=True),
                _format_value(summary.get("mrr")),
                _format_value(summary.get("ndcg_at_10")),
                _format_value(summary.get("agent_solved_rate"), percent=True),
                _format_value(summary.get("mean_elapsed_ms")),
                _format_value(summary.get("mean_returned_chars")),
                _format_value(total_tokens),
                _format_value(summary.get("estimated_external_cost")),
            )
        )
    lines = [
        "| " + " | ".join(headers) + " |",
        "| " + " | ".join("---" for _ in headers) + " |",
    ]
    lines.extend("| " + " | ".join(row) + " |" for row in rows)
    return "\n".join(lines)


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(description="Run the OCE retrieval benchmark")
    parser.add_argument("--repositories", type=Path, default=DEFAULT_REPOSITORIES)
    parser.add_argument("--cases", type=Path, default=DEFAULT_CASES)
    parser.add_argument("--variants", type=Path, default=DEFAULT_VARIANTS)
    subparsers = parser.add_subparsers(dest="command", required=True)

    validate = subparsers.add_parser(
        "validate", help="validate manifests and optional workspaces"
    )
    validate.add_argument("--workdir", type=Path)

    prepare = subparsers.add_parser("prepare", help="clone pinned benchmark workspaces")
    prepare.add_argument("--workdir", type=Path, required=True)

    variants = subparsers.add_parser(
        "variants", help="show reproducible server configurations"
    )
    variants.add_argument("name", nargs="?")

    run = subparsers.add_parser(
        "run", help="sync workspaces and execute retrieval cases"
    )
    run.add_argument("--workdir", type=Path, required=True)
    run.add_argument("--output", type=Path, required=True)
    identity = run.add_mutually_exclusive_group(required=True)
    identity.add_argument(
        "--variant",
        help="checked-in variant whose live server profile must match",
    )
    identity.add_argument(
        "--label",
        help="ad hoc, unverified configuration label",
    )
    run.add_argument("--api-url")
    run.add_argument("--state-dir", type=Path)
    run.add_argument("--repository", action="append", default=[])
    run.add_argument("--category", action="append", default=[])
    run.add_argument("--case", action="append", default=[])
    run.add_argument("--task-outcomes", type=Path)
    run.add_argument("--metadata", action="append", default=[], metavar="KEY=VALUE")
    run.add_argument(
        "--price",
        action="append",
        default=[],
        metavar="KIND=USD_PER_MTOK",
    )
    run.add_argument("--metrics-settle-seconds", type=float, default=6.0)

    compare = subparsers.add_parser("compare", help="render a Markdown variant table")
    compare.add_argument("results", nargs="+", type=Path)
    compare.add_argument(
        "--allow-unverified",
        action="store_true",
        help="include ad hoc --label results that lack a verified variant profile",
    )
    compare.add_argument(
        "--allow-embedding-change",
        action="store_true",
        help="explicitly compare results produced by different embedding profiles",
    )
    return parser


def main(argv: Sequence[str] | None = None) -> int:
    args = build_parser().parse_args(argv)
    if args.command == "validate":
        repositories = load_repositories(args.repositories)
        cases = load_cases(args.cases)
        variants = load_variants(args.variants)
        validate_corpus(
            repositories, cases, args.workdir.resolve() if args.workdir else None
        )
        print(
            json.dumps(
                {
                    "repositories": len(repositories),
                    "cases": len(cases),
                    "variants": len(variants),
                },
                sort_keys=True,
            )
        )
        return 0
    if args.command == "prepare":
        repositories = load_repositories(args.repositories)
        prepare_workspaces(repositories, args.workdir.resolve())
        validate_corpus(repositories, load_cases(args.cases), args.workdir.resolve())
        return 0
    if args.command == "variants":
        available = load_variants(args.variants)
        if args.name:
            try:
                payload = asdict(available[args.name])
            except KeyError as exc:
                raise ValueError(f"unknown benchmark variant: {args.name}") from exc
        else:
            payload = {name: asdict(available[name]) for name in sorted(available)}
        print(json.dumps(payload, indent=2, sort_keys=True))
        return 0
    if args.command == "run":
        if args.metrics_settle_seconds < 0:
            raise ValueError("metrics-settle-seconds must not be negative")
        payload = run_benchmark(args)
        args.output.parent.mkdir(parents=True, exist_ok=True)
        args.output.write_text(
            json.dumps(payload, ensure_ascii=False, indent=2, sort_keys=True) + "\n",
            encoding="utf-8",
        )
        print(json.dumps(payload["summary"], ensure_ascii=False, sort_keys=True))
        return 0
    if args.command == "compare":
        print(
            compare_results(
                args.results,
                allow_unverified=args.allow_unverified,
                allow_embedding_change=args.allow_embedding_change,
            )
        )
        return 0
    raise ValueError(f"unknown command: {args.command}")


if __name__ == "__main__":
    raise SystemExit(main())
