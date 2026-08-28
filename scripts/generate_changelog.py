"""Generate Keep a Changelog sections from Conventional Commits."""

from __future__ import annotations

import argparse
import re
import subprocess
import sys
from datetime import datetime, timezone
from pathlib import Path


REPO_ROOT = Path(__file__).resolve().parent.parent
CHANGELOG = REPO_ROOT / "CHANGELOG.md"

_HEADER_RE = re.compile(
    r"^(?P<type>[a-z]+)(?:\((?P<scope>[\w.-]+)\))?"
    r"(?P<breaking>!)?: (?P<desc>.+)$",
    re.IGNORECASE,
)
_GROUP_BY_TYPE = {
    "feat": "Added",
    "fix": "Fixed",
    "perf": "Changed",
    "refactor": "Changed",
    "docs": "Changed",
    "style": "Changed",
    "build": "Changed",
    "deprecate": "Deprecated",
    "remove": "Removed",
    "security": "Security",
}
_NOISE = {"chore", "ci", "test", "revert"}
_CHANGELOG_HEADER = (
    "# Changelog\n\n"
    "This project follows [Semantic Versioning](https://semver.org/).\n"
    "Entries are generated from Conventional Commits.\n\n"
)
_UNRELEASED = "## [Unreleased]\n\n"


def _stdout() -> None:
    try:
        sys.stdout.reconfigure(encoding="utf-8")
    except AttributeError:
        pass


def git_log(since: str | None, to: str) -> list[tuple[str, str, str]]:
    format_string = "%H%x1f%s%x1f%b%x1e"
    revision = to if since is None else f"{since}..{to}"
    result = subprocess.run(
        ["git", "log", "--no-merges", f"--format={format_string}", revision],
        cwd=REPO_ROOT,
        check=True,
        capture_output=True,
        text=True,
        encoding="utf-8",
        errors="replace",
    )
    commits: list[tuple[str, str, str]] = []
    for record in result.stdout.split("\x1e"):
        record = record.strip("\n")
        if not record:
            continue
        parts = record.split("\x1f")
        commits.append(
            (
                parts[0],
                parts[1] if len(parts) > 1 else "",
                parts[2] if len(parts) > 2 else "",
            )
        )
    return commits


def latest_tag() -> str | None:
    result = subprocess.run(
        ["git", "describe", "--tags", "--abbrev=0"],
        cwd=REPO_ROOT,
        capture_output=True,
        text=True,
        encoding="utf-8",
        errors="replace",
    )
    return result.stdout.strip() if result.returncode == 0 else None


def group_commits(commits: list[tuple[str, str, str]]) -> dict[str, list[str]]:
    groups: dict[str, list[str]] = {
        "Added": [],
        "Fixed": [],
        "Changed": [],
        "Deprecated": [],
        "Removed": [],
        "Security": [],
        "Breaking Changes": [],
    }
    for _, subject, body in commits:
        match = _HEADER_RE.match(subject)
        if match is None:
            continue
        commit_type = match.group("type").lower()
        if commit_type in _NOISE:
            continue
        description = match.group("desc")
        scope = match.group("scope")
        entry = f"- **{scope}**: {description}" if scope else f"- {description}"
        breaking = bool(match.group("breaking")) or "BREAKING CHANGE:" in body.upper()
        if breaking:
            groups["Breaking Changes"].append(entry)
            continue
        group = _GROUP_BY_TYPE.get(commit_type)
        if group is not None:
            groups[group].append(entry)
    return {name: entries for name, entries in groups.items() if entries}


def render(title: str, groups: dict[str, list[str]]) -> str:
    lines = [f"## {title}", ""]
    for name, entries in groups.items():
        lines.extend((f"### {name}", "", *entries, ""))
    return "\n".join(lines).rstrip() + "\n"


def build_section(
    version: str | None,
    since: str | None = None,
    to: str = "HEAD",
) -> str:
    if since is None:
        since = latest_tag()
    groups = group_commits(git_log(since, to))
    title = (
        f"[{version}] - {datetime.now(timezone.utc).date().isoformat()}"
        if version
        else "[Unreleased]"
    )
    return render(title, groups)


def prepend_release(section: str) -> None:
    if not CHANGELOG.exists():
        CHANGELOG.write_text(_CHANGELOG_HEADER + _UNRELEASED + section, encoding="utf-8")
        return

    original = CHANGELOG.read_text(encoding="utf-8")
    first_section = re.search(r"^## ", original, re.MULTILINE)
    header = original[: first_section.start()] if first_section else original.rstrip() + "\n\n"
    sections = original[first_section.start() :] if first_section else ""
    sections = re.sub(
        r"^## \[Unreleased\]\s*.*?(?=^## |\Z)",
        "",
        sections,
        count=1,
        flags=re.MULTILINE | re.DOTALL,
    ).lstrip()
    content = header.rstrip() + "\n\n" + _UNRELEASED + section
    if sections:
        content += "\n" + sections
    CHANGELOG.write_text(content.rstrip() + "\n", encoding="utf-8")


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(
        description="Generate a changelog section from Conventional Commits"
    )
    parser.add_argument("--version", help="release version, or Unreleased when omitted")
    parser.add_argument("--since", help="starting Git ref; defaults to the latest tag")
    parser.add_argument("--to", default="HEAD", help="ending Git ref")
    parser.add_argument("--prepend", action="store_true", help="write into CHANGELOG.md")
    args = parser.parse_args(argv)

    section = build_section(args.version, args.since, args.to)
    if args.prepend:
        if args.version is None:
            raise SystemExit("ERROR: --prepend requires --version")
        prepend_release(section)
        print(f"Updated {CHANGELOG}")
    else:
        sys.stdout.write(section)
    return 0


if __name__ == "__main__":
    _stdout()
    raise SystemExit(main())
