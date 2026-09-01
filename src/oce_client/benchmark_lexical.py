from __future__ import annotations

import os
import re
import shutil
import subprocess
from dataclasses import dataclass
from pathlib import Path
from typing import Sequence


LEXICAL_BASELINE_VERSION = 1
MAX_RESULTS = 10
MAX_EXCERPT_CHARS = 1_600

_TOKEN = re.compile(
    r"[A-Za-z][A-Za-z0-9]*(?:[_.:/-][A-Za-z0-9]+)*|"
    r"[\u3400-\u4dbf\u4e00-\u9fff]{2,}"
)
_CAMEL_PART = re.compile(r"[A-Z]+(?=[A-Z][a-z]|\d|$)|[A-Z]?[a-z]+|[A-Z]+|\d+")
_STOP_WORDS = frozenset(
    """
    a an and are as at be by class code defined definition diagnose do does file
    find for from how implementation implemented in into is it locate of on or
    source the through to trace what when where which with would you
    哪里 在哪 定义 实现 如何 怎么 什么 哪些 检查 源码 代码 文件
    """.split()
)


@dataclass(frozen=True)
class LexicalRetrieval:
    paths: tuple[str, ...]
    formatted_context: str


def resolve_ripgrep() -> str:
    executable = shutil.which("rg")
    if executable is None:
        raise RuntimeError("ripgrep (rg) is required for the lexical baseline")
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
        "kind": "ripgrep_lexical",
        "version": LEXICAL_BASELINE_VERSION,
        "ripgrep_version": version,
        "agent_workflow": False,
    }


def lexical_query_terms(query: str) -> tuple[str, ...]:
    candidates: list[str] = []
    for token in _TOKEN.findall(query):
        if token.isascii():
            candidates.append(token)
            for component in re.split(r"[_.:/-]+", token):
                candidates.extend(_CAMEL_PART.findall(component))
        else:
            if len(token) <= 6:
                candidates.append(token)
            candidates.extend(
                token[index : index + 2] for index in range(len(token) - 1)
            )

    terms: list[str] = []
    seen: set[str] = set()
    for candidate in candidates:
        normalized = candidate.casefold()
        if len(normalized) < 2 or normalized in _STOP_WORDS or normalized in seen:
            continue
        seen.add(normalized)
        terms.append(candidate)
        if len(terms) == 32:
            break
    return tuple(terms)


def _path_batches(paths: Sequence[str]):
    batch: list[str] = []
    size = 0
    for path in paths:
        path_size = len(os.fsencode(path)) + 1
        if batch and size + path_size > 24_000:
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
    executable: str,
) -> set[str]:
    found: set[str] = set()
    if not paths or not terms:
        return found
    patterns = [argument for term in terms for argument in ("-e", term)]
    for batch in _path_batches(paths):
        completed = subprocess.run(
            [
                executable,
                "--no-config",
                "-l0",
                "--ignore-case",
                "--fixed-strings",
                "--no-ignore",
                "--hidden",
                *patterns,
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
            if raw_path:
                found.add(Path(os.fsdecode(raw_path)).as_posix().removeprefix("./"))
    return found


def _rank_documents(
    documents: dict[str, str], candidates: set[str], terms: Sequence[str]
) -> tuple[str, ...]:
    ranked: list[tuple[int, int, int, str]] = []
    for path in candidates:
        content = documents.get(path)
        if content is None:
            continue
        folded_path = path.casefold()
        folded_content = content.casefold()
        path_hits = sum(term.casefold() in folded_path for term in terms)
        content_hits = sum(term.casefold() in folded_content for term in terms)
        occurrences = sum(
            min(folded_content.count(term.casefold()), 3) for term in terms
        )
        score = path_hits * 10 + content_hits * 3 + occurrences
        if score:
            ranked.append((score, path_hits, content_hits, path))
    ranked.sort(key=lambda item: (-item[0], -item[1], -item[2], item[3]))
    return tuple(item[3] for item in ranked[:MAX_RESULTS])


def _excerpt(content: str, terms: Sequence[str]) -> str:
    if len(content) <= MAX_EXCERPT_CHARS:
        return content
    folded = content.casefold()
    positions = [folded.find(term.casefold()) for term in terms]
    position = min((item for item in positions if item >= 0), default=0)
    start = max(0, position - MAX_EXCERPT_CHARS // 3)
    end = min(len(content), start + MAX_EXCERPT_CHARS)
    start = max(0, end - MAX_EXCERPT_CHARS)
    return (
        ("…\n" if start else "")
        + content[start:end]
        + ("\n…" if end < len(content) else "")
    )


def retrieve_lexically(
    root: Path,
    documents: dict[str, str],
    query: str,
    *,
    executable: str,
) -> LexicalRetrieval:
    terms = lexical_query_terms(query)
    paths = sorted(documents)
    candidates = _ripgrep_candidates(root, paths, terms, executable)
    candidates.update(
        path
        for path in paths
        if any(term.casefold() in path.casefold() for term in terms)
    )
    selected = _rank_documents(documents, candidates, terms)
    sections = ["The following lexical code excerpts were retrieved:"]
    for path in selected:
        sections.extend(
            ("", f"Path: {path}", "Excerpt:", _excerpt(documents[path], terms))
        )
    return LexicalRetrieval(selected, "\n".join(sections))
