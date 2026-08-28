"""Prepare a local release commit and annotated tag without pushing them."""

from __future__ import annotations

import argparse
import subprocess
import sys
from datetime import datetime, timezone
from pathlib import Path


sys.path.insert(0, str(Path(__file__).resolve().parent))

import bump_version
import generate_changelog


REPO_ROOT = Path(__file__).resolve().parent.parent
RELEASE_FILES = [
    "pyproject.toml",
    "src/oce_client/__init__.py",
    "uv.lock",
    "CHANGELOG.md",
]


def _stdout() -> None:
    try:
        sys.stdout.reconfigure(encoding="utf-8")
    except AttributeError:
        pass


def ensure_clean() -> None:
    result = subprocess.run(
        ["git", "status", "--porcelain"],
        cwd=REPO_ROOT,
        check=True,
        capture_output=True,
        text=True,
        encoding="utf-8",
        errors="replace",
    )
    dirty = [line for line in result.stdout.splitlines() if line.strip()]
    if dirty:
        raise SystemExit(
            "ERROR: release requires a clean worktree:\n" + "\n".join(dirty)
        )


def run(command: list[str]) -> None:
    print("+", " ".join(command))
    subprocess.run(command, cwd=REPO_ROOT, check=True)


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(
        description="Bump version, update changelog, build, commit, and tag locally"
    )
    parser.add_argument("version_or_part", help="major, minor, patch, or an exact version")
    parser.add_argument("--dry-run", action="store_true", help="print without changing files")
    args = parser.parse_args(argv)

    ensure_clean()
    current = bump_version.read_current_version()
    target = bump_version.resolve_version(args.version_or_part, current)
    if current == target:
        raise SystemExit(f"ERROR: version is already {current}")

    print(f"Preparing release {target} (current {current})")
    print("Plan:")
    print(f"  1. Update pyproject.toml, __version__, and uv.lock to {target}")
    print("  2. Generate and prepend the changelog section")
    print("  3. Build wheel and sdist")
    print(f"  4. Commit chore(release): v{target} and create annotated tag v{target}")
    if args.dry_run:
        print("[dry-run] No files, commits, tags, or remotes were changed")
        return 0

    bump_version.update_all(target)
    section = generate_changelog.build_section(
        target,
        since=generate_changelog.latest_tag(),
    )
    if "### " not in section:
        raise SystemExit("ERROR: no releasable Conventional Commits found")
    generate_changelog.prepend_release(section)

    run(["uv", "build"])
    run(["git", "add", *RELEASE_FILES])
    run(["git", "commit", "-m", f"chore(release): v{target}"])
    run(
        [
            "git",
            "tag",
            "-a",
            f"v{target}",
            "-m",
            f"v{target} ({datetime.now(timezone.utc).date().isoformat()})",
        ]
    )

    print(f"Release v{target} prepared locally")
    print("Review the commit and tag, then push them explicitly when ready:")
    print("  git push origin master")
    print(f"  git push origin v{target}")
    print("Pushing the tag triggers the trusted-publishing release workflow.")
    return 0


if __name__ == "__main__":
    _stdout()
    raise SystemExit(main())
