from __future__ import annotations

import sys
import threading
from contextlib import asynccontextmanager
from pathlib import Path
from collections.abc import Callable
from typing import Annotated, Any

from .runtime import ClientConfigurationError, ClientRuntime, ClientSettings


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


def create_server(
    settings: ClientSettings,
    *,
    runtime_factory: Callable[[ClientSettings], ClientRuntime] = ClientRuntime,
) -> Any:
    FastMCP = _require_sdk()
    lock = threading.RLock()
    runtimes: dict[Path, ClientRuntime] = {}

    def runtime_for(workspace_folder: str | None) -> ClientRuntime:
        default_root = settings.root.resolve()
        if workspace_folder is None:
            root = default_root
        else:
            if not workspace_folder.strip():
                raise ValueError("workspace_folder must not be empty")
            root = Path(workspace_folder).expanduser().resolve()
        if root == default_root:
            runtime_settings = settings
        else:
            runtime_settings = ClientSettings(
                root=root,
                api_url=settings.api_url,
                api_key=settings.api_key,
                runtime_patterns=settings.runtime_patterns,
            )
        runtime = runtimes.get(root)
        if runtime is None:
            runtime = runtime_factory(runtime_settings)
            runtimes[root] = runtime
        return runtime

    @asynccontextmanager
    async def lifespan(_server: Any):
        try:
            yield
        finally:
            for runtime in runtimes.values():
                runtime.close()

    server = FastMCP("oce-client", lifespan=lifespan)

    def codebase_retrieval(
        information_request: str,
        workspace_folder: str | None = None,
    ) -> dict[str, object]:
        """Retrieve current code context after synchronizing the workspace."""
        if not information_request.strip():
            raise ValueError("information_request must not be empty")
        with lock:
            context = runtime_for(workspace_folder or None).context()
            context.sync()
            result = context.retrieve(information_request)
        return {
            "formatted_retrieval": result.formatted_retrieval,
            "elapsed_ms": result.elapsed_ms,
        }

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
    server.tool(name="codebase-retrieval", description=TOOL_DESCRIPTION)(codebase_retrieval)
    return server


def run_mcp(settings: ClientSettings) -> None:
    server = create_server(settings)
    server.run(transport="stdio")


def main() -> int:
    try:
        settings = ClientSettings.from_environment(require_api_key=True)
        run_mcp(settings)
    except (ClientConfigurationError, OSError, ValueError) as exc:
        print(f"oce-client-mcp: {exc}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
