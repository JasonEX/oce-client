"""Update the Rust package version."""

from __future__ import annotations

import argparse
import re
import subprocess
import sys
import tomllib
from pathlib import Path


REPO_ROOT = Path(__file__).resolve().parent.parent
MANIFEST = REPO_ROOT / "Cargo.toml"
VERSION_FILES = ["Cargo.toml", "Cargo.lock"]

_SEMVER_RE = re.compile(
    r"^\d+\.\d+\.\d+(?:-[0-9A-Za-z.-]+)?(?:\+[0-9A-Za-z.-]+)?$"
)
_PARTS = ("major", "minor", "patch")


def read_current_version() -> str:
    with MANIFEST.open("rb") as handle:
        data = tomllib.load(handle)
    version = data.get("package", {}).get("version")
    if not isinstance(version, str) or not version:
        raise SystemExit(f"ERROR: package.version is missing from {MANIFEST}")
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
    if not _SEMVER_RE.fullmatch(value):
        raise SystemExit(f"ERROR: {value!r} is not a supported SemVer version")
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


def update_manifest(version: str) -> None:
    _replace_in_file(
        MANIFEST,
        re.compile(r'^version\s*=\s*"[^"]*"'),
        f'version = "{version}"',
    )


def sync_lock() -> None:
    subprocess.run(["cargo", "check"], cwd=REPO_ROOT, check=True)


def verify_sync() -> None:
    subprocess.run(["cargo", "check", "--locked"], cwd=REPO_ROOT, check=True)


def update_all(version: str) -> None:
    update_manifest(version)
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
        description="Update Cargo.toml and Cargo.lock"
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
    sys.stdout.reconfigure(encoding="utf-8")
    raise SystemExit(main())
