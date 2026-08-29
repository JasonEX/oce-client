from __future__ import annotations

import argparse
import asyncio
import hashlib
import os
import sys
from collections.abc import Callable, Sequence
from contextlib import asynccontextmanager
from pathlib import Path
from typing import Annotated, Any

from .indexer import WorkspaceIndexer
from .runtime import (
    ClientConfigurationError,
    ClientRuntime,
    ClientSettings,
    iter_runtime_patterns,
)


TOOL_DESCRIPTION = """This tool is Open Context Engine(oce), Open source codebase context engine. It:
1. Takes in a natural language description of the code you are looking for;
2. Uses a proprietary retrieval/embedding model suite that produces the highest-quality recall of relevant code snippets from across the codebase;
3. Maintains a real-time index of the codebase, so the results are always up-to-date and reflects the current state of the codebase;
4. Can retrieve across different programming languages;
5. Only reflects the current state of the codebase on the disk, and has no information on version control or code history."""


def _require_sdk() -> Any:
    try:
        from mcp.server.fastmcp import FastMCP
    except ImportError as exc:
        raise ClientConfigurationError(
            "MCP support is not installed; install opencontextengine-client with the 'mcp' extra"
        ) from exc
    return FastMCP


def _state_path(state_dir: Path, root: Path) -> Path:
    identity = os.path.normcase(str(root.resolve())).encode("utf-8")
    name = hashlib.sha256(identity).hexdigest()[:16]
    return state_dir.expanduser().resolve() / f"{name}.sqlite3"


def create_server(
    settings: ClientSettings,
    *,
    workspace_roots: Sequence[Path] | None = None,
    state_dir: Path | None = None,
    debounce_ms: int = 500,
    initial_sync: str = "background",
    ready_timeout: float = 3.0,
    log_level: str = "WARNING",
    runtime_factory: Callable[[ClientSettings], ClientRuntime] = ClientRuntime,
) -> Any:
    if debounce_ms < 0:
        raise ClientConfigurationError("debounce-ms must not be negative")
    if ready_timeout < 0:
        raise ClientConfigurationError("ready-timeout must not be negative")
    if initial_sync not in {"background", "blocking", "off"}:
        raise ClientConfigurationError(f"unknown initial sync mode: {initial_sync}")

    roots = tuple(
        dict.fromkeys(
            root.expanduser().resolve()
            for root in (workspace_roots or (settings.root,))
        )
    )
    if not roots:
        raise ClientConfigurationError("at least one workspace is required")
    for root in roots:
        if not root.is_dir():
            raise ClientConfigurationError(f"workspace is not a directory: {root}")

    FastMCP = _require_sdk()
    indexers: dict[Path, WorkspaceIndexer] = {}
    for root in roots:
        if state_dir is not None:
            state_path = _state_path(state_dir, root)
        elif root == settings.root.resolve():
            state_path = settings.state_path
        else:
            state_path = None
        root_settings = ClientSettings(
            root=root,
            api_url=settings.api_url,
            api_key=settings.api_key,
            state_path=state_path,
            runtime_patterns=settings.runtime_patterns,
        )
        indexers[root] = WorkspaceIndexer(
            root_settings,
            runtime_factory=runtime_factory,
            debounce_ms=debounce_ms,
        )

    def indexer_for(workspace_folder: str | None) -> WorkspaceIndexer:
        if workspace_folder is None:
            if len(indexers) != 1:
                raise ValueError(
                    "workspace_folder is required when multiple workspaces are configured"
                )
            return next(iter(indexers.values()))
        if not workspace_folder.strip():
            raise ValueError("workspace_folder must not be empty")
        requested = Path(workspace_folder).expanduser().resolve()
        indexer = indexers.get(requested)
        if indexer is None:
            allowed = ", ".join(str(root) for root in roots)
            raise ValueError(
                f"workspace_folder is not configured: {requested}; allowed: {allowed}"
            )
        return indexer

    @asynccontextmanager
    async def lifespan(_server: Any):
        try:
            if initial_sync != "off":
                for indexer in indexers.values():
                    indexer.start(initial_sync=True)
                if initial_sync == "blocking":
                    for indexer in indexers.values():
                        status = indexer.wait_until_ready(None)
                        if status == "error":
                            detail = indexer.status().get("error")
                            raise ClientConfigurationError(
                                f"initial workspace synchronization failed: {detail}"
                            )
            yield
        finally:
            for indexer in indexers.values():
                indexer.stop()

    server = FastMCP("oce-client", lifespan=lifespan, log_level=log_level)

    async def codebase_retrieval(
        information_request: str,
        workspace_folder: str | None = None,
    ) -> dict[str, object]:
        """Retrieve code context after the background index reaches the latest change."""
        if not information_request.strip():
            raise ValueError("information_request must not be empty")
        return await asyncio.to_thread(
            indexer_for(workspace_folder).retrieve,
            information_request,
            ready_timeout,
        )

    # FastMCP derives JSON Schema descriptions from Pydantic Field metadata. Keep
    # pydantic behind the optional MCP extra so the base CLI remains dependency-free.
    from pydantic import Field

    codebase_retrieval.__annotations__ = {
        "information_request": Annotated[
            str,
            Field(description="A description of the information you need."),
        ],
        "workspace_folder": Annotated[
            str,
            Field(
                description=(
                    "Path to the workspace folder to search. Required when multiple "
                    "workspace folders are open. Use the folder paths shown in your "
                    "system prompt."
                )
            ),
        ],
        "return": dict[str, object],
    }
    server.tool(name="codebase-retrieval", description=TOOL_DESCRIPTION)(
        codebase_retrieval
    )
    server._oce_indexers = indexers
    return server


def run_mcp(
    settings: ClientSettings,
    *,
    workspace_roots: Sequence[Path] | None = None,
    state_dir: Path | None = None,
    debounce_ms: int = 500,
    initial_sync: str = "background",
    ready_timeout: float = 3.0,
    log_level: str = "WARNING",
) -> None:
    server = create_server(
        settings,
        workspace_roots=workspace_roots,
        state_dir=state_dir,
        debounce_ms=debounce_ms,
        initial_sync=initial_sync,
        ready_timeout=ready_timeout,
        log_level=log_level,
    )
    server.run(transport="stdio")


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(
        prog="oce-client-mcp",
        description="Run the OpenContextEngine MCP server over stdio.",
    )
    parser.add_argument(
        "--workspace",
        action="append",
        default=[],
        metavar="PATH",
        help="allowed workspace folder; repeat for multiple workspaces",
    )
    parser.add_argument("--api-url", default=None, help="OCE API URL")
    parser.add_argument(
        "--state-dir",
        default=os.environ.get("OCE_STATE_DIR"),
        help="directory for per-workspace SQLite state",
    )
    parser.add_argument(
        "--ignore",
        action="append",
        default=[],
        metavar="PATTERN",
        help="runtime ignore pattern; repeat or comma-separate",
    )
    parser.add_argument(
        "--debounce-ms",
        type=int,
        default=os.environ.get("OCE_DEBOUNCE_MS", "500"),
        help="filesystem watcher debounce interval",
    )
    parser.add_argument(
        "--initial-sync",
        choices=("background", "blocking", "off"),
        default=os.environ.get("OCE_INITIAL_SYNC", "background"),
        help="initial workspace synchronization strategy",
    )
    parser.add_argument(
        "--ready-timeout",
        type=float,
        default=os.environ.get("OCE_READY_TIMEOUT", "3"),
        help="seconds a retrieval waits for the latest index generation",
    )
    parser.add_argument(
        "--log-level",
        choices=("debug", "info", "warning", "error", "critical"),
        default=os.environ.get("OCE_LOG_LEVEL", "warning").lower(),
    )
    return parser


def main(argv: Sequence[str] | None = None) -> int:
    try:
        args = build_parser().parse_args(argv)
        roots = tuple(Path(value).expanduser().resolve() for value in args.workspace)
        root = roots[0] if roots else None
        settings = ClientSettings.from_environment(
            root=root,
            api_url=args.api_url,
            runtime_patterns=iter_runtime_patterns(iter(args.ignore)),
            require_api_key=True,
        )
        if len(roots) > 1 and settings.state_path is not None and args.state_dir is None:
            raise ClientConfigurationError(
                "OCE_STATE_PATH cannot be shared by multiple workspaces; use --state-dir"
            )
        run_mcp(
            settings,
            workspace_roots=roots or None,
            state_dir=Path(args.state_dir) if args.state_dir else None,
            debounce_ms=args.debounce_ms,
            initial_sync=args.initial_sync,
            ready_timeout=args.ready_timeout,
            log_level=args.log_level.upper(),
        )
    except (ClientConfigurationError, OSError, ValueError) as exc:
        print(f"oce-client-mcp: {exc}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
