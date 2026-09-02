from __future__ import annotations

import argparse
import json
import math
import os
import shutil
import subprocess
import tempfile
import time
from dataclasses import asdict, dataclass
from datetime import datetime, timezone
from pathlib import Path
from statistics import fmean
from typing import Iterable, Sequence

import httpx

from .benchmark_lexical import (
    lexical_baseline_profile,
    resolve_ripgrep,
    retrieve_lexically,
    ripgrep_version,
)


ROOT = Path(__file__).resolve().parents[2] / "benchmarks"
DEFAULT_REPOSITORIES = ROOT / "repositories.json"
DEFAULT_CASES = ROOT / "cases.jsonl"
DEFAULT_VARIANTS = ROOT / "variants.json"
DEFAULT_API_URL = "http://127.0.0.1:8986"


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


def resolve_client_binary(configured: str | None = None) -> str:
    selected = configured or os.environ.get("OCE_CLIENT_BINARY")
    if selected:
        candidate = Path(selected).expanduser()
        if candidate.is_file():
            return str(candidate.resolve())
        if executable := shutil.which(selected):
            return executable
        raise FileNotFoundError(f"oce-client binary not found: {selected}")

    executable_name = "oce-client.exe" if os.name == "nt" else "oce-client"
    local_builds = [
        candidate
        for profile in ("release", "debug")
        if (candidate := ROOT.parent / "target" / profile / executable_name).is_file()
    ]
    if local_builds:
        # Prefer the most recent local build so a stale profile is not picked up.
        newest = max(local_builds, key=lambda candidate: candidate.stat().st_mtime)
        return str(newest.resolve())
    if executable := shutil.which("oce-client"):
        return executable
    raise FileNotFoundError(
        "oce-client binary not found; build it with cargo or pass --client-binary"
    )


def _run_client_json(
    binary: str,
    root: Path,
    state_path: Path,
    command: Sequence[str],
    *,
    api_url: str | None = None,
    api_key: str | None = None,
) -> dict[str, object]:
    environment = os.environ.copy()
    if api_key is not None:
        environment["OCE_API_KEY"] = api_key
    arguments = [binary, "--root", str(root), "--state-path", str(state_path)]
    if api_url is not None:
        arguments.extend(["--api-url", api_url])
    completed = subprocess.run(
        [*arguments, *command],
        check=False,
        capture_output=True,
        text=True,
        encoding="utf-8",
        errors="replace",
        env=environment,
    )
    if completed.returncode != 0:
        detail = completed.stderr.strip() or completed.stdout.strip()
        raise RuntimeError(
            f"oce-client {command[0]} failed with exit code "
            f"{completed.returncode}: {detail[:1000]}"
        )
    try:
        payload = json.loads(completed.stdout)
    except json.JSONDecodeError as exc:
        raise RuntimeError(f"oce-client {command[0]} returned invalid JSON") from exc
    if not isinstance(payload, dict):
        raise RuntimeError(f"oce-client {command[0]} returned a non-object payload")
    return payload


def _admitted_documents(binary: str, root: Path, state_path: Path) -> dict[str, str]:
    """Load the files the Rust client would upload, keyed by workspace-relative path."""
    payload = _run_client_json(binary, root, state_path, ("list-files", "--json"))
    files = payload.get("files")
    if not isinstance(files, list) or not all(isinstance(item, str) for item in files):
        raise RuntimeError("oce-client list-files returned an invalid payload")
    return {path: (root / path).read_text(encoding="utf-8") for path in files}


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


@dataclass(frozen=True)
class SelectedCorpus:
    repositories: dict[str, RepositorySpec]
    cases: list[BenchmarkCase]
    all_case_ids: set[str]
    workdir: Path

    def repository_names(self) -> list[str]:
        return sorted({case.repository for case in self.cases})

    def root(self, name: str) -> Path:
        return (self.workdir / name).resolve()


def _load_selected_corpus(args: argparse.Namespace) -> SelectedCorpus:
    repositories = load_repositories(args.repositories)
    corpus = load_cases(args.cases)
    cases = _selected_cases(corpus, args)
    workdir = args.workdir.resolve()
    validate_corpus(repositories, cases, workdir)
    return SelectedCorpus(
        repositories=repositories,
        cases=cases,
        all_case_ids={case.id for case in corpus},
        workdir=workdir,
    )


def _status_entry(
    started: float,
    error: Exception | None = None,
    **fields: object,
) -> dict[str, object]:
    entry: dict[str, object] = {
        "status": "error" if error is not None else "ok",
        "elapsed_ms": int((time.perf_counter() - started) * 1000),
    }
    if error is not None:
        entry["error_type"] = type(error).__name__
        entry["error"] = _safe_error_message(error)
    entry.update(fields)
    return entry


def _result_payload(
    *,
    label: str,
    api_url: str | None,
    corpus: SelectedCorpus,
    variant: BenchmarkVariant | None,
    baseline: dict[str, object] | None,
    metadata: dict[str, object],
    sync: dict[str, object],
    summary: dict[str, object],
    results: Sequence[CaseResult],
) -> dict[str, object]:
    return {
        "label": label,
        "created_at": datetime.now(timezone.utc).isoformat(),
        "api_url": api_url,
        "repositories": {
            name: asdict(corpus.repositories[name])
            for name in corpus.repository_names()
        },
        "variant": asdict(variant) if variant is not None else None,
        "baseline": baseline,
        "metadata": metadata,
        "sync": sync,
        "summary": summary,
        "cases": [asdict(item) for item in results],
    }


def run_benchmark(args: argparse.Namespace) -> dict[str, object]:
    if bool(args.variant) == bool(args.label):
        raise ValueError("provide exactly one of variant or label")
    variant: BenchmarkVariant | None = None
    label = args.label
    if args.variant:
        variants = load_variants(args.variants)
        try:
            variant = variants[args.variant]
        except KeyError as exc:
            raise ValueError(f"unknown benchmark variant: {args.variant}") from exc
        label = variant.name

    corpus = _load_selected_corpus(args)
    outcomes = _load_outcomes(args.task_outcomes, corpus.all_case_ids)
    metadata = _parse_key_values(args.metadata)
    prices = _parse_key_values(args.price, numeric=True)
    api_url = args.api_url or os.environ.get("OCE_API_URL", DEFAULT_API_URL)
    api_key = os.environ.get("OCE_API_KEY", "sk-opencontextengine")
    admin_key = os.environ.get("OCE_ADMIN_API_KEY")
    client_binary = resolve_client_binary(args.client_binary)
    sync: dict[str, object] = {}
    sync_errors: dict[str, Exception] = {}
    results: list[CaseResult] = []
    stats_before: dict[str, object] | None = None
    stats_after: dict[str, object] | None = None
    with tempfile.TemporaryDirectory(prefix="oce-benchmark-") as temporary_state:
        state_dir = (
            Path(temporary_state) if args.state_dir is None else args.state_dir.resolve()
        )
        state_dir.mkdir(parents=True, exist_ok=True)
        state_paths = {
            name: state_dir / f"{name}.sqlite3" for name in corpus.repository_names()
        }
        for name in corpus.repository_names():
            started = time.perf_counter()
            try:
                sync_result = _run_client_json(
                    client_binary,
                    corpus.root(name),
                    state_paths[name],
                    ("sync", "--json"),
                    api_url=api_url,
                    api_key=api_key,
                )
                uploaded = sync_result.get("uploaded_blob_names")
                checkpoint_id = sync_result.get("checkpoint_id")
                if not isinstance(uploaded, list) or not isinstance(checkpoint_id, str):
                    raise RuntimeError("oce-client sync returned an invalid payload")
            except Exception as exc:
                sync_errors[name] = exc
                sync[name] = _status_entry(started, exc)
            else:
                sync[name] = _status_entry(
                    started,
                    uploaded_blobs=len(uploaded),
                    checkpoint_id_present=True,
                )

        if admin_key:
            time.sleep(args.metrics_settle_seconds)
            stats_before = _admin_stats(api_url, admin_key)

        for case in corpus.cases:
            solved = outcomes.get(case.id)
            if error := sync_errors.get(case.repository):
                results.append(failed_case(case, error, agent_solved=solved))
                continue
            try:
                retrieval = _run_client_json(
                    client_binary,
                    corpus.root(case.repository),
                    state_paths[case.repository],
                    ("retrieve", case.query, "--json"),
                    api_url=api_url,
                    api_key=api_key,
                )
                formatted = retrieval.get("formatted_retrieval")
                elapsed_ms = retrieval.get("elapsed_ms")
                if not isinstance(formatted, str) or not isinstance(elapsed_ms, int):
                    raise RuntimeError("oce-client retrieve returned an invalid payload")
                results.append(
                    score_case(
                        case,
                        parse_retrieved_paths(formatted),
                        returned_chars=len(formatted),
                        elapsed_ms=elapsed_ms,
                        agent_solved=solved,
                    )
                )
            except Exception as exc:
                results.append(failed_case(case, exc, agent_solved=solved))

        if admin_key:
            time.sleep(args.metrics_settle_seconds)
            stats_after = _admin_stats(api_url, admin_key)

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
    return _result_payload(
        label=label,
        api_url=api_url,
        corpus=corpus,
        variant=variant,
        baseline=None,
        metadata=metadata,
        sync=sync,
        summary=summary,
        results=results,
    )


def run_lexical_baseline(args: argparse.Namespace) -> dict[str, object]:
    corpus = _load_selected_corpus(args)
    metadata = _parse_key_values(args.metadata)
    executable = resolve_ripgrep()
    baseline = lexical_baseline_profile(ripgrep_version(executable))
    client_binary = resolve_client_binary(args.client_binary)

    documents: dict[str, dict[str, str]] = {}
    scan_errors: dict[str, Exception] = {}
    sync: dict[str, object] = {}
    with tempfile.TemporaryDirectory(prefix="oce-benchmark-") as temporary_state:
        for name in corpus.repository_names():
            started = time.perf_counter()
            try:
                admitted = _admitted_documents(
                    client_binary,
                    corpus.root(name),
                    Path(temporary_state) / f"{name}.sqlite3",
                )
                if not admitted:
                    raise ValueError(
                        f"no admissible benchmark files found: {corpus.root(name)}"
                    )
                documents[name] = admitted
            except Exception as exc:
                scan_errors[name] = exc
                sync[name] = _status_entry(started, exc)
            else:
                sync[name] = _status_entry(
                    started,
                    admitted_files=len(admitted),
                    admitted_chars=sum(len(content) for content in admitted.values()),
                )

    results: list[CaseResult] = []
    for case in corpus.cases:
        if error := scan_errors.get(case.repository):
            results.append(failed_case(case, error))
            continue
        started = time.perf_counter()
        try:
            retrieval = retrieve_lexically(
                corpus.root(case.repository),
                documents[case.repository],
                case.query,
                executable=executable,
            )
            results.append(
                score_case(
                    case,
                    retrieval.paths,
                    returned_chars=len(retrieval.formatted_context),
                    elapsed_ms=int((time.perf_counter() - started) * 1000),
                )
            )
        except Exception as exc:
            results.append(failed_case(case, exc))

    summary = aggregate(results)
    summary["external_model_tokens"] = {}
    summary["estimated_external_cost"] = 0.0
    return _result_payload(
        label=f"ripgrep-lexical-v{baseline['version']}",
        api_url=None,
        corpus=corpus,
        variant=None,
        baseline=baseline,
        metadata=metadata,
        sync=sync,
        summary=summary,
        results=results,
    )


def _format_value(value: object, *, percent: bool = False) -> str:
    if value is None:
        return "—"
    if percent:
        return f"{float(value) * 100:.1f}%"
    if isinstance(value, float):
        return f"{value:.2f}"
    return str(value)


def compare_results(paths: Sequence[Path]) -> str:
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
    for path in paths:
        payload = _read_json(path)
        if not isinstance(payload, dict) or not isinstance(
            payload.get("summary"), dict
        ):
            raise ValueError(f"invalid benchmark result: {path}")
        label = str(payload.get("label", path.stem))
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


def _add_selection_arguments(parser: argparse.ArgumentParser) -> None:
    parser.add_argument("--workdir", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument(
        "--client-binary",
        help="oce-client executable (default: OCE_CLIENT_BINARY, local target, or PATH)",
    )
    parser.add_argument("--repository", action="append", default=[])
    parser.add_argument("--category", action="append", default=[])
    parser.add_argument("--case", action="append", default=[])
    parser.add_argument("--metadata", action="append", default=[], metavar="KEY=VALUE")


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
    _add_selection_arguments(run)
    identity = run.add_mutually_exclusive_group(required=True)
    identity.add_argument("--variant", help="checked-in diagnostic configuration")
    identity.add_argument("--label", help="ad hoc configuration label")
    run.add_argument("--api-url")
    run.add_argument("--state-dir", type=Path)
    run.add_argument("--task-outcomes", type=Path)
    run.add_argument(
        "--price",
        action="append",
        default=[],
        metavar="KIND=USD_PER_MTOK",
    )
    run.add_argument("--metrics-settle-seconds", type=float, default=6.0)

    baseline = subparsers.add_parser(
        "baseline",
        help="run the deterministic no-model ripgrep lexical baseline",
    )
    _add_selection_arguments(baseline)

    compare = subparsers.add_parser("compare", help="render a Markdown variant table")
    compare.add_argument("results", nargs="+", type=Path)
    return parser


def _write_result(output: Path, payload: dict[str, object]) -> None:
    output.parent.mkdir(parents=True, exist_ok=True)
    output.write_text(
        json.dumps(payload, ensure_ascii=False, indent=2, sort_keys=True) + "\n",
        encoding="utf-8",
    )
    print(json.dumps(payload["summary"], ensure_ascii=False, sort_keys=True))


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
        _write_result(args.output, run_benchmark(args))
        return 0
    if args.command == "baseline":
        _write_result(args.output, run_lexical_baseline(args))
        return 0
    if args.command == "compare":
        print(compare_results(args.results))
        return 0
    raise ValueError(f"unknown command: {args.command}")


if __name__ == "__main__":
    raise SystemExit(main())
