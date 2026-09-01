"""Prepare a local release commit and annotated tag without pushing them.

The first release accepts the current project version when no local release tag
exists; later releases must use a higher version.
"""

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
    "Cargo.toml",
    "Cargo.lock",
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
    previous_tag = generate_changelog.latest_tag()
    initial_release = current == target and previous_tag is None
    if current == target and not initial_release:
        raise SystemExit(f"ERROR: version is already {current}")

    if initial_release:
        print(f"Preparing initial release {target}")
    else:
        print(f"Preparing release {target} (current {current})")
    print("Plan:")
    if initial_release:
        print(f"  1. Verify Cargo.toml and Cargo.lock are {target}")
    else:
        print(f"  1. Update Cargo.toml and Cargo.lock to {target}")
    print("  2. Generate and prepend the changelog section")
    print("  3. Build the release binary")
    print(f"  4. Commit chore(release): v{target} and create annotated tag v{target}")
    if args.dry_run:
        print("[dry-run] No files, commits, tags, or remotes were changed")
        return 0

    if not initial_release:
        bump_version.update_all(target)
    section = generate_changelog.build_section(
        target,
        since=previous_tag,
    )
    if "### " not in section:
        raise SystemExit("ERROR: no releasable Conventional Commits found")
    generate_changelog.prepend_release(section)

    run(["cargo", "build", "--release", "--locked"])
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
    print("Pushing the tag triggers the GitHub binary release workflow.")
    return 0


if __name__ == "__main__":
    _stdout()
    raise SystemExit(main())
