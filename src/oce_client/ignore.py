from __future__ import annotations

from pathlib import Path
from typing import Iterable

from pathspec.gitignore import GitIgnoreSpec


DEFAULT_PATTERNS = (
    ".git/",
    ".oce-client/",
    "__pycache__/",
    ".pytest_cache/",
    ".mypy_cache/",
    ".ruff_cache/",
    ".venv/",
    "venv/",
    "node_modules/",
    "target/",
    "dist/",
    "build/",
    "coverage/",
    ".idea/",
    ".vscode/",
)


class _RuleLayer:
    def __init__(self, lines: Iterable[str]) -> None:
        cleaned = []
        for raw in lines:
            line = raw.strip("\r\n")
            if line and not line.startswith("#"):
                cleaned.append(line)
        self.patterns = list(GitIgnoreSpec.from_lines(cleaned).patterns)

    def match(self, path: str, is_dir: bool) -> bool | None:
        candidate = path.rstrip("/") + ("/" if is_dir else "")
        result: bool | None = None
        for pattern in self.patterns:
            if pattern.match_file(candidate) is not None:
                # GitWildMatchPattern.include=True means the path is ignored;
                # a leading ! produces include=False and re-includes it.
                result = bool(pattern.include)
        return result


class LayeredIgnoreMatcher:
    """Merge runtime, project, git, and built-in ignore rules by precedence."""

    def __init__(
        self,
        root: Path,
        runtime_patterns: Iterable[str] = (),
        *,
        oceignore_name: str = ".oceignore",
        gitignore_name: str = ".gitignore",
    ) -> None:
        self.root = root
        self._hard = _RuleLayer((".git/", ".git/**", ".oce-client/", ".oce-client/**"))
        self._runtime = _RuleLayer(runtime_patterns)
        self._oce = _RuleLayer(self._read_lines(root / oceignore_name))
        self._git = _RuleLayer(self._read_lines(root / gitignore_name))
        self._defaults = _RuleLayer(DEFAULT_PATTERNS[2:])

    @staticmethod
    def _read_lines(path: Path) -> list[str]:
        try:
            return path.read_text(encoding="utf-8").splitlines()
        except (FileNotFoundError, UnicodeDecodeError, OSError):
            return []

    def ignores(self, path: str, *, is_dir: bool = False) -> bool:
        normalized = path.replace("\\", "/")
        while normalized.startswith("./"):
            normalized = normalized[2:]
        # A hard rule cannot be undone by a higher-priority negation.
        if self._hard.match(normalized, is_dir) is True:
            return True
        for layer in (self._runtime, self._oce, self._git, self._defaults):
            decision = layer.match(normalized, is_dir)
            if decision is not None:
                return decision
        return False
