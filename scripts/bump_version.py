"""Update the project version in all authoritative package metadata."""

from __future__ import annotations

import argparse
import re
import subprocess
import sys
import tomllib
from pathlib import Path


REPO_ROOT = Path(__file__).resolve().parent.parent
PYPROJECT = REPO_ROOT / "pyproject.toml"
INIT_FILE = REPO_ROOT / "src/oce_client/__init__.py"
LOCK_FILE = REPO_ROOT / "uv.lock"
VERSION_FILES = [
    "pyproject.toml",
    "src/oce_client/__init__.py",
    "uv.lock",
]

_PEP440_RE = re.compile(
    r"^\d+(?:\.\d+)*(?:[ab]|rc)?\d*(?:\.post\d+)?(?:\.dev\d+)?$"
)
_PARTS = ("major", "minor", "patch")


def _stdout() -> None:
    try:
        sys.stdout.reconfigure(encoding="utf-8")
    except AttributeError:
        pass


def read_current_version() -> str:
    with PYPROJECT.open("rb") as handle:
        data = tomllib.load(handle)
    version = data.get("project", {}).get("version")
    if not isinstance(version, str) or not version:
        raise SystemExit(f"ERROR: project.version is missing from {PYPROJECT}")
    return version


def resolve_version(value: str, current: str) -> str:
    if value in _PARTS:
        match = re.match(r"^(\d+)(?:\.(\d+))?(?:\.(\d+))?", current)
        if match is None:
            raise SystemExit(f"ERROR: cannot bump current version {current!r}")
        parts = [int(part or "0") for part in match.groups()]
        if value == "major":
            parts = [parts[0] + 1, 0, 0]
        elif value == "minor":
            parts = [parts[0], parts[1] + 1, 0]
        else:
            parts[2] += 1
        return ".".join(str(part) for part in parts)
    if not _PEP440_RE.fullmatch(value):
        raise SystemExit(f"ERROR: {value!r} is not a supported PEP 440 version")
    return value


def _replace_in_file(path: Path, pattern: re.Pattern[str], replacement: str) -> None:
    lines = path.read_text(encoding="utf-8").splitlines(keepends=True)
    matches = 0
    for index, line in enumerate(lines):
        if pattern.search(line):
            lines[index] = pattern.sub(replacement, line)
            matches += 1
    if matches != 1:
        raise SystemExit(
            f"ERROR: expected one version field in {path}, found {matches}"
        )
    path.write_text("".join(lines), encoding="utf-8")


def update_pyproject(version: str) -> None:
    _replace_in_file(
        PYPROJECT,
        re.compile(r'^version\s*=\s*"[^"]*"'),
        f'version = "{version}"',
    )


def update_init(version: str) -> None:
    _replace_in_file(
        INIT_FILE,
        re.compile(r'^__version__\s*=\s*"[^"]*"'),
        f'__version__ = "{version}"',
    )


def sync_lock() -> None:
    subprocess.run(["uv", "lock"], cwd=REPO_ROOT, check=True)


def verify_sync() -> None:
    init_text = INIT_FILE.read_text(encoding="utf-8")
    match = re.search(r'^__version__\s*=\s*"([^"]*)"', init_text, re.MULTILINE)
    init_version = match.group(1) if match else None
    project_version = read_current_version()
    if init_version != project_version:
        raise SystemExit(
            "ERROR: version mismatch: "
            f"pyproject={project_version!r}, __init__={init_version!r}"
        )
    subprocess.run(["uv", "lock", "--check"], cwd=REPO_ROOT, check=True)


def update_all(version: str) -> None:
    update_pyproject(version)
    update_init(version)
    sync_lock()
    verify_sync()


def git_commit(version: str) -> None:
    subprocess.run(
        ["git", "add", *VERSION_FILES],
        cwd=REPO_ROOT,
        check=True,
    )
    subprocess.run(
        ["git", "commit", "-m", f"chore(release): v{version}"],
        cwd=REPO_ROOT,
        check=True,
    )


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(
        description="Update pyproject.toml, oce_client.__version__, and uv.lock"
    )
    parser.add_argument("version_or_part", help="major, minor, patch, or an exact version")
    parser.add_argument("--commit", action="store_true", help="commit updated version files")
    parser.add_argument("--dry-run", action="store_true", help="print without changing files")
    args = parser.parse_args(argv)

    current = read_current_version()
    target = resolve_version(args.version_or_part, current)
    if current == target:
        print(f"Version is already {current}; no changes required")
        return 0
    if args.dry_run:
        print(f"[dry-run] {current} -> {target}")
        return 0

    update_all(target)
    if args.commit:
        git_commit(target)
    print(f"Version updated: {current} -> {target}")
    return 0


if __name__ == "__main__":
    _stdout()
    raise SystemExit(main())
