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
    McpConfiguration,
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


def add_mcp_arguments(parser: argparse.ArgumentParser) -> None:
    """Add the MCP server launch options to the standalone entry point."""
    parser.add_argument(
        "--workspace",
        action="append",
        default=argparse.SUPPRESS,
        metavar="PATH",
        help="allowed workspace folder; repeat for multiple workspaces",
    )
    parser.add_argument(
        "--api-url",
        default=argparse.SUPPRESS,
        help="OCE API URL (default: OCE_API_URL or the local default)",
    )
    parser.add_argument(
        "--state-path",
        default=argparse.SUPPRESS,
        help="SQLite state file; only valid with one workspace",
    )
    parser.add_argument(
        "--state-dir",
        default=argparse.SUPPRESS,
        help="directory for per-workspace SQLite state (OCE_STATE_DIR)",
    )
    parser.add_argument(
        "--ignore",
        action="append",
        default=argparse.SUPPRESS,
        metavar="PATTERN",
        help="runtime ignore pattern; repeat or comma-separate (OCE_IGNORE)",
    )
    parser.add_argument(
        "--debounce-ms",
        type=int,
        default=argparse.SUPPRESS,
        help="filesystem watcher debounce interval",
    )
    parser.add_argument(
        "--initial-sync",
        choices=("background", "blocking", "off"),
        default=argparse.SUPPRESS,
        help="initial workspace synchronization strategy",
    )
    parser.add_argument(
        "--ready-timeout",
        type=float,
        default=argparse.SUPPRESS,
        help="seconds a retrieval waits for the latest index generation",
    )
    parser.add_argument(
        "--log-level",
        choices=("debug", "info", "warning", "error", "critical"),
        default=argparse.SUPPRESS,
        help="MCP server log level",
    )


def mcp_configuration_from_args(args: argparse.Namespace) -> McpConfiguration:
    values = getattr(args, "workspace", None)
    return McpConfiguration.from_environment(
        workspace_roots=tuple(values) if values else None,
        api_url=getattr(args, "api_url", None),
        state_path=getattr(args, "state_path", None),
        state_dir=getattr(args, "state_dir", None),
        runtime_patterns=(
            iter_runtime_patterns(args.ignore)
            if getattr(args, "ignore", None) is not None
            else None
        ),
        debounce_ms=getattr(args, "debounce_ms", None),
        initial_sync=getattr(args, "initial_sync", None),
        ready_timeout=getattr(args, "ready_timeout", None),
        log_level=getattr(args, "log_level", None),
    )


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

    server = FastMCP("oce-client", lifespan=lifespan, log_level=log_level.upper())

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


def _state_path(state_dir: Path, root: Path) -> Path:
    identity = os.path.normcase(str(root.resolve())).encode("utf-8")
    name = hashlib.sha256(identity).hexdigest()[:16]
    return state_dir.expanduser().resolve() / f"{name}.sqlite3"


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


def run_mcp_configuration(configuration: McpConfiguration) -> None:
    run_mcp(
        configuration.client,
        workspace_roots=configuration.workspace_roots,
        state_dir=configuration.state_dir,
        debounce_ms=configuration.debounce_ms,
        initial_sync=configuration.initial_sync,
        ready_timeout=configuration.ready_timeout,
        log_level=configuration.log_level,
    )


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(
        prog="oce-client-mcp",
        description="Run the OpenContextEngine MCP server over stdio.",
    )
    add_mcp_arguments(parser)
    return parser


def main(argv: Sequence[str] | None = None) -> int:
    try:
        args = build_parser().parse_args(argv)
        run_mcp_configuration(mcp_configuration_from_args(args))
    except (ClientConfigurationError, OSError, ValueError) as exc:
        print(f"oce-client-mcp: {exc}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
