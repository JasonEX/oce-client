from __future__ import annotations

import argparse
import importlib.metadata
import importlib.resources
import json
import os
import shutil
import sys
from dataclasses import asdict, is_dataclass
from enum import Enum
from pathlib import Path
from typing import Sequence

from .context import BlobCompatibilityError, CheckpointResetRequired
from .filesystem import FileAdmissionError
from .http import OceApiError
from .runtime import (
    DEFAULT_API_URL,
    ClientConfigurationError,
    ClientRuntime,
    ClientSettings,
    iter_runtime_patterns,
)


try:
    VERSION = importlib.metadata.version("opencontextengine-client")
except importlib.metadata.PackageNotFoundError:
    VERSION = "0.1.0"


def _json_default(value: object) -> object:
    if isinstance(value, Enum):
        return value.value
    if isinstance(value, Path):
        return value.as_posix()
    if is_dataclass(value):
        return asdict(value)
    raise TypeError(f"cannot encode {type(value).__name__}")


def _dump(value: object) -> None:
    print(json.dumps(value, default=_json_default, ensure_ascii=False, sort_keys=True))


def _snapshot_payload(snapshot: object) -> dict[str, object]:
    files = getattr(snapshot, "files")
    return {
        "checkpoint_id": getattr(snapshot, "checkpoint_id"),
        "generation": getattr(snapshot, "generation"),
        "files": {
            path: {
                "blob_name": record.blob_name,
                "committed_blob_name": record.committed_blob_name,
                "status": record.status.value,
                "size": record.size,
                "source": record.source,
                "generation": record.generation,
            }
            for path, record in files.items()
        },
    }


def _skill_source() -> Path:
    packaged = importlib.resources.files("oce_client").joinpath("skill")
    if packaged.is_dir():
        return Path(str(packaged))
    source_tree = Path(__file__).resolve().parents[2] / "skills" / "oce-client"
    if source_tree.is_dir():
        return source_tree
    raise ClientConfigurationError("the oce-client skill is not present in this installation")


def _codex_skill_target() -> Path:
    return Path(os.environ.get("CODEX_HOME", Path.home() / ".codex")) / "skills" / "oce-client"


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(
        prog="oce-client",
        description="Synchronize a local workspace with OpenContextEngine.",
    )
    parser.add_argument("--version", action="version", version=f"oce-client {VERSION}")
    parser.add_argument("--root", default=None, help="workspace directory (default: OCE_WORKSPACE or .)")
    parser.add_argument(
        "--api-url",
        default=None,
        help=f"OCE API URL (default: OCE_API_URL or {DEFAULT_API_URL})",
    )
    parser.add_argument(
        "--state-path",
        default=None,
        help="SQLite state path (default: OCE_STATE_PATH or workspace/.oce-client)",
    )
    parser.add_argument(
        "--ignore",
        action="append",
        default=None,
        metavar="PATTERN",
        help="runtime ignore pattern; can be repeated or comma-separated (OCE_IGNORE)",
    )
    subparsers = parser.add_subparsers(dest="command", required=True)

    sync = subparsers.add_parser("sync", help="reconcile and upload the workspace")
    sync.add_argument("--json", action="store_true", dest="as_json")

    status = subparsers.add_parser("status", help="show local inventory and checkpoint state")
    status.add_argument("--json", action="store_true", dest="as_json")

    retrieve = subparsers.add_parser("retrieve", help="retrieve formatted code context")
    retrieve.add_argument("query")
    retrieve.add_argument("--scope", choices=("workspace", "working_set"), default="workspace")
    retrieve.add_argument("--json", action="store_true", dest="as_json")

    observe = subparsers.add_parser("observe", help="stage explicit file content")
    observe.add_argument("path")
    observe.add_argument("--content", default=None)
    observe.add_argument("--file", dest="content_file", default=None, help="read content from a file")
    observe.add_argument("--json", action="store_true", dest="as_json")

    remove = subparsers.add_parser("remove", help="stage a file deletion")
    remove.add_argument("path")
    remove.add_argument("--json", action="store_true", dest="as_json")

    watch = subparsers.add_parser("watch", help="watch the workspace and sync on changes")
    watch.add_argument("--debounce-ms", type=int, default=300)

    skill = subparsers.add_parser("skill", help="locate or install the Codex skill")
    skill_subparsers = skill.add_subparsers(dest="skill_command", required=True)
    skill_path = skill_subparsers.add_parser("path", help="print the bundled skill path")
    skill_path.add_argument("--json", action="store_true", dest="as_json")
    skill_install = skill_subparsers.add_parser("install", help="install the skill into Codex skills")
    skill_install.add_argument("--target", default=None, help="installation directory")
    skill_install.add_argument("--force", action="store_true", help="replace an existing installation")
    skill_install.add_argument("--json", action="store_true", dest="as_json")
    return parser


def _settings(args: argparse.Namespace) -> ClientSettings:
    return ClientSettings.from_environment(
        root=args.root,
        api_url=args.api_url,
        state_path=args.state_path,
        runtime_patterns=(
            iter_runtime_patterns(iter(args.ignore)) if args.ignore else None
        ),
        require_api_key=args.command in {"sync", "retrieve", "watch"},
    )


def _run(args: argparse.Namespace) -> int:
    if args.command == "skill":
        source = _skill_source()
        if args.skill_command == "path":
            payload = {"path": source.resolve().as_posix()}
            if args.as_json:
                _dump(payload)
            else:
                print(payload["path"])
            return 0
        target = Path(args.target).expanduser() if args.target else _codex_skill_target()
        target = target.resolve()
        if target == source.resolve():
            raise ValueError("skill target is already the bundled skill directory")
        if target.exists() and not args.force:
            raise FileExistsError(f"skill target already exists: {target}; pass --force to replace it")
        target.parent.mkdir(parents=True, exist_ok=True)
        shutil.copytree(source, target, dirs_exist_ok=args.force)
        payload = {"path": target.as_posix(), "status": "installed"}
        if args.as_json:
            _dump(payload)
        else:
            print(f"installed skill at {target}")
        return 0
    settings = _settings(args)
    with ClientRuntime(settings) as runtime:
        context = runtime.context()
        if args.command == "sync":
            result = context.sync()
            if args.as_json:
                _dump(result)
            else:
                print(f"synced checkpoint={result.checkpoint_id or '-'} uploaded={len(result.uploaded_blob_names)}")
            return 0
        if args.command == "status":
            snapshot = context.snapshot()
            if args.as_json:
                _dump(_snapshot_payload(snapshot))
            else:
                present = sum(record.status.value == "present" for record in snapshot.files.values())
                print(f"root={settings.root}")
                print(f"checkpoint={snapshot.checkpoint_id or '-'} generation={snapshot.generation} present={present}")
            return 0
        if args.command == "retrieve":
            result = context.retrieve(args.query, scope=args.scope)
            if args.as_json:
                _dump(result)
            else:
                print(result.formatted_retrieval)
            return 0
        if args.command == "observe":
            if args.content is not None and args.content_file is not None:
                raise ValueError("use either --content or --file, not both")
            if args.content_file is not None:
                content = Path(args.content_file).read_text(encoding="utf-8")
            elif args.content is not None:
                content = args.content
            else:
                content = sys.stdin.read()
            context.observe_file(args.path, content)
            payload = {"path": args.path, "status": "present"}
            if args.as_json:
                _dump(payload)
            else:
                print(f"observed {args.path}")
            return 0
        if args.command == "remove":
            context.remove_file(args.path)
            payload = {"path": args.path, "status": "deleted"}
            if args.as_json:
                _dump(payload)
            else:
                print(f"removed {args.path}")
            return 0
        if args.command == "watch":
            handle = context.start_watching(debounce_ms=args.debounce_ms)
            print(f"watching {settings.root}; press Ctrl-C to stop", file=sys.stderr)
            try:
                handle.join()
            except KeyboardInterrupt:
                handle.stop()
                handle.join(2.0)
            return 0
    raise ValueError(f"unknown command: {args.command}")


def main(argv: Sequence[str] | None = None) -> int:
    args = build_parser().parse_args(argv)
    try:
        return _run(args)
    except (ClientConfigurationError, FileAdmissionError, OceApiError, BlobCompatibilityError, CheckpointResetRequired, TimeoutError, ValueError, OSError) as exc:
        print(f"oce-client: {exc}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
