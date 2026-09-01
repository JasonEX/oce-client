from __future__ import annotations

import math
import os
import re
import shutil
import subprocess
from dataclasses import dataclass
from pathlib import Path
from typing import Sequence


LEXICAL_BASELINE_KIND = "ripgrep_lexical"
LEXICAL_BASELINE_VERSION = 1
LEXICAL_MAX_RESULTS = 10
LEXICAL_MAX_EXCERPT_CHARS = 1_600

_ASCII_TOKEN = re.compile(r"[A-Za-z][A-Za-z0-9]*(?:[_.:/-][A-Za-z0-9]+)*")
_CAMEL_PART = re.compile(r"[A-Z]+(?=[A-Z][a-z]|\d|$)|[A-Z]?[a-z]+|[A-Z]+|\d+")
_CJK_SPAN = re.compile(r"[\u3400-\u4dbf\u4e00-\u9fff]{2,}")

_ENGLISH_STOP_WORDS = frozenset(
    {
        "a",
        "an",
        "and",
        "are",
        "as",
        "at",
        "be",
        "by",
        "class",
        "code",
        "defined",
        "definition",
        "diagnose",
        "do",
        "does",
        "file",
        "find",
        "for",
        "from",
        "how",
        "implementation",
        "implemented",
        "in",
        "into",
        "is",
        "it",
        "locate",
        "of",
        "on",
        "or",
        "source",
        "the",
        "through",
        "to",
        "trace",
        "what",
        "when",
        "where",
        "which",
        "with",
        "would",
        "you",
    }
)
_CJK_STOP_WORDS = frozenset(
    {
        "哪里",
        "在哪",
        "定义",
        "实现",
        "如何",
        "怎么",
        "什么",
        "哪些",
        "检查",
        "源码",
        "代码",
        "文件",
    }
)


@dataclass(frozen=True)
class LexicalRetrieval:
    paths: tuple[str, ...]
    formatted_context: str


def resolve_ripgrep() -> str:
    executable = shutil.which("rg")
    if executable is None:
        raise RuntimeError(
            "ripgrep (rg) is required for the deterministic lexical baseline"
        )
    return executable


def ripgrep_version(executable: str) -> str:
    completed = subprocess.run(
        [executable, "--version"],
        check=True,
        capture_output=True,
        text=True,
    )
    first_line = completed.stdout.splitlines()[0].strip()
    if not first_line:
        raise RuntimeError("ripgrep returned an empty version string")
    return first_line


def lexical_baseline_profile(version: str) -> dict[str, object]:
    return {
        "kind": LEXICAL_BASELINE_KIND,
        "version": LEXICAL_BASELINE_VERSION,
        "ripgrep_version": version,
        "agent_workflow": False,
    }


def lexical_query_terms(query: str) -> tuple[str, ...]:
    candidates: list[str] = []
    for token in _ASCII_TOKEN.findall(query):
        candidates.append(token)
        for component in re.split(r"[_.:/-]+", token):
            candidates.extend(_CAMEL_PART.findall(component))
    for span in _CJK_SPAN.findall(query):
        if len(span) <= 6:
            candidates.append(span)
        candidates.extend(span[index : index + 2] for index in range(len(span) - 1))

    terms: list[str] = []
    seen: set[str] = set()
    for candidate in candidates:
        normalized = candidate.casefold()
        if (
            len(normalized) < 2
            or normalized in _ENGLISH_STOP_WORDS
            or normalized in _CJK_STOP_WORDS
            or normalized in seen
        ):
            continue
        seen.add(normalized)
        terms.append(candidate)
        if len(terms) == 32:
            break
    return tuple(terms)


def _term_weight(term: str) -> float:
    if any(character.isupper() for character in term) or any(
        separator in term for separator in "_.:/-"
    ):
        return 4.0
    if term.isascii() and len(term) >= 8:
        return 2.0
    return 1.0


def _path_batches(paths: Sequence[str], *, max_argument_bytes: int = 24_000):
    batch: list[str] = []
    size = 0
    for path in paths:
        path_size = len(os.fsencode(path)) + 1
        if batch and size + path_size > max_argument_bytes:
            yield batch
            batch = []
            size = 0
        batch.append(path)
        size += path_size
    if batch:
        yield batch


def _ripgrep_candidates(
    root: Path,
    paths: Sequence[str],
    terms: Sequence[str],
    *,
    executable: str,
) -> set[str]:
    candidates: set[str] = set()
    if not paths or not terms:
        return candidates
    pattern_arguments = [argument for term in terms for argument in ("-e", term)]
    for batch in _path_batches(paths):
        completed = subprocess.run(
            [
                executable,
                "--no-config",
                "--files-with-matches",
                "--null",
                "--ignore-case",
                "--fixed-strings",
                "--no-ignore",
                "--hidden",
                "--color=never",
                *pattern_arguments,
                "--",
                *batch,
            ],
            cwd=root,
            check=False,
            capture_output=True,
        )
        if completed.returncode not in {0, 1}:
            detail = completed.stderr.decode("utf-8", errors="replace").strip()
            raise RuntimeError(
                f"ripgrep failed with exit code {completed.returncode}: {detail[:500]}"
            )
        for raw_path in completed.stdout.split(b"\0"):
            if not raw_path:
                continue
            normalized = Path(os.fsdecode(raw_path)).as_posix()
            while normalized.startswith("./"):
                normalized = normalized[2:]
            candidates.add(normalized)
    return candidates


def _document_score(
    path: str, content: str, terms: Sequence[str]
) -> tuple[float, float, int]:
    folded_path = path.casefold()
    folded_content = content.casefold()
    total = 0.0
    coverage = 0.0
    path_hits = 0
    for term in terms:
        folded_term = term.casefold()
        in_path = folded_term in folded_path
        content_count = folded_content.count(folded_term)
        if not in_path and content_count == 0:
            continue
        weight = _term_weight(term)
        coverage += weight
        if in_path:
            path_hits += 1
            total += 12.0 * weight
        if content_count:
            total += weight * (6.0 + min(3.0, math.log2(content_count + 1)))
    return total, coverage, path_hits


def _bounded_excerpt(content: str, terms: Sequence[str]) -> str:
    if len(content) <= LEXICAL_MAX_EXCERPT_CHARS:
        return content
    folded = content.casefold()
    matches = [
        (_term_weight(term), folded.find(term.casefold()))
        for term in terms
        if folded.find(term.casefold()) >= 0
    ]
    position = min(matches, key=lambda item: (-item[0], item[1]))[1] if matches else 0
    start = max(0, position - LEXICAL_MAX_EXCERPT_CHARS // 3)
    end = min(len(content), start + LEXICAL_MAX_EXCERPT_CHARS)
    start = max(0, end - LEXICAL_MAX_EXCERPT_CHARS)
    prefix = "…\n" if start else ""
    suffix = "\n…" if end < len(content) else ""
    return prefix + content[start:end] + suffix


def retrieve_lexically(
    root: Path,
    documents: dict[str, str],
    query: str,
    *,
    executable: str,
) -> LexicalRetrieval:
    terms = lexical_query_terms(query)
    ordered_paths = sorted(documents)
    candidates = _ripgrep_candidates(
        root,
        ordered_paths,
        terms,
        executable=executable,
    )
    for path in ordered_paths:
        folded_path = path.casefold()
        if any(term.casefold() in folded_path for term in terms):
            candidates.add(path)

    ranked: list[tuple[float, float, int, str]] = []
    for path in candidates:
        content = documents.get(path)
        if content is None:
            continue
        score, coverage, path_hits = _document_score(path, content, terms)
        if score > 0:
            ranked.append((score, coverage, path_hits, path))
    ranked.sort(key=lambda item: (-item[0], -item[1], -item[2], item[3]))
    paths = tuple(item[3] for item in ranked[:LEXICAL_MAX_RESULTS])

    sections = ["The following lexical code excerpts were retrieved:"]
    for path in paths:
        sections.extend(
            (
                "",
                f"Path: {path}",
                "Excerpt:",
                _bounded_excerpt(documents[path], terms),
            )
        )
    return LexicalRetrieval(
        paths=paths,
        formatted_context="\n".join(sections),
    )
