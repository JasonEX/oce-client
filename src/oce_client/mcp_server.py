from __future__ import annotations

import sys
import threading
from contextlib import asynccontextmanager
from typing import Any

from .runtime import ClientConfigurationError, ClientRuntime, ClientSettings


def _require_sdk() -> Any:
    try:
        from mcp.server.fastmcp import FastMCP
    except ImportError as exc:
        raise ClientConfigurationError(
            "MCP support is not installed; install opencontextengine-client with the 'mcp' extra"
        ) from exc
    return FastMCP


def create_server(settings: ClientSettings) -> Any:
    FastMCP = _require_sdk()
    runtime = ClientRuntime(settings)
    lock = threading.RLock()

    @asynccontextmanager
    async def lifespan(_server: Any):
        try:
            yield
        finally:
            runtime.close()

    server = FastMCP("oce-client", lifespan=lifespan)

    @server.tool(
        name="sync_workspace",
        description="Reconcile local files, upload missing blobs, and advance the workspace checkpoint.",
    )
    def sync_workspace() -> dict[str, object]:
        with lock:
            result = runtime.context().sync()
        return {
            "uploaded_blob_names": list(result.uploaded_blob_names),
            "checkpoint_id": result.checkpoint_id,
            "added_blobs": list(result.added_blobs),
            "deleted_blobs": list(result.deleted_blobs),
        }

    @server.tool(
        name="retrieve_code",
        description="Retrieve formatted code context for a natural-language request.",
    )
    def retrieve_code(query: str, scope: str = "workspace") -> dict[str, object]:
        with lock:
            result = runtime.context().retrieve(query, scope=scope)
        return {"formatted_retrieval": result.formatted_retrieval, "elapsed_ms": result.elapsed_ms}

    @server.tool(
        name="observe_file",
        description="Stage explicit text for a workspace-relative file before a later sync.",
    )
    def observe_file(path: str, content: str) -> dict[str, str]:
        with lock:
            runtime.context().observe_file(path, content)
        return {"path": path, "status": "present"}

    @server.tool(
        name="remove_file",
        description="Stage deletion of a workspace-relative file before a later sync.",
    )
    def remove_file(path: str) -> dict[str, str]:
        with lock:
            runtime.context().remove_file(path)
        return {"path": path, "status": "deleted"}

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
