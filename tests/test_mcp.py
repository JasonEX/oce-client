from __future__ import annotations

import asyncio
import json
import os
from pathlib import Path
import subprocess
import sys

import pytest

pytest.importorskip("mcp.server.fastmcp")

from oce_client.mcp_server import create_server
from oce_client.runtime import ClientSettings


def _call(server, name: str, arguments: dict[str, object]):
    return asyncio.run(server._tool_manager.call_tool(name, arguments))


class FakeContext:
    def __init__(self, calls: list[str]):
        self.calls = calls

    def sync(self):
        self.calls.append("sync")

    def retrieve(self, information_request: str):
        self.calls.append(f"retrieve:{information_request}")
        return type("Result", (), {"formatted_retrieval": "retrieved context", "elapsed_ms": 7})()


class FakeRuntime:
    def __init__(self, settings: ClientSettings, calls: list[str]):
        self.settings = settings
        self.calls = calls
        self._context = FakeContext(calls)

    def context(self) -> FakeContext:
        self.calls.append(f"context:{self.settings.root}")
        return self._context

    def close(self) -> None:
        self.calls.append(f"close:{self.settings.root}")


def test_mcp_exposes_only_codebase_retrieval_and_auto_syncs(tmp_path: Path):
    calls: list[str] = []

    def factory(settings: ClientSettings) -> FakeRuntime:
        return FakeRuntime(settings, calls)

    server = create_server(
        ClientSettings(tmp_path, "http://oce.test", "test-key"),
        runtime_factory=factory,
    )
    tools = asyncio.run(server.list_tools())
    assert {tool.name for tool in tools} == {"codebase-retrieval"}
    schema = tools[0].inputSchema
    assert schema["required"] == ["information_request"]
    assert schema["properties"]["information_request"]["description"] == (
        "A description of the information you need."
    )
    assert schema["properties"]["workspace_folder"]["description"].startswith(
        "Path to the workspace folder"
    )

    result = _call(server, "codebase-retrieval", {"information_request": "find x"})
    assert result == {"formatted_retrieval": "retrieved context", "elapsed_ms": 7}
    assert calls == [f"context:{tmp_path.resolve()}", "sync", "retrieve:find x"]


def test_mcp_retrieval_supports_explicit_workspace_folder(tmp_path: Path):
    calls: list[str] = []
    workspace = tmp_path / "workspace"
    workspace.mkdir()

    def factory(settings: ClientSettings) -> FakeRuntime:
        return FakeRuntime(settings, calls)

    server = create_server(
        ClientSettings(tmp_path, "http://oce.test", "test-key"),
        runtime_factory=factory,
    )
    result = _call(
        server,
        "codebase-retrieval",
        {"information_request": "find x", "workspace_folder": str(workspace)},
    )
    assert result["formatted_retrieval"] == "retrieved context"
    assert calls[0] == f"context:{workspace.resolve()}"

    with pytest.raises(Exception):
        _call(server, "codebase-retrieval", {"information_request": "   "})
    with pytest.raises(Exception):
        _call(
            server,
            "codebase-retrieval",
            {"information_request": "x", "workspace_folder": " "},
        )


def test_mcp_stdio_initialize_and_list_tools(tmp_path: Path):
    requests = [
        {
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": "2025-03-26",
                "capabilities": {},
                "clientInfo": {"name": "oce-client-test", "version": "1"},
            },
        },
        {"jsonrpc": "2.0", "method": "notifications/initialized", "params": {}},
        {"jsonrpc": "2.0", "id": 2, "method": "tools/list", "params": {}},
    ]
    environment = os.environ.copy()
    environment["OCE_API_KEY"] = "unused-in-test"
    environment["OCE_WORKSPACE"] = str(tmp_path)
    result = subprocess.run(
        [sys.executable, "-m", "oce_client.mcp_server"],
        input="\n".join(json.dumps(request) for request in requests) + "\n",
        text=True,
        capture_output=True,
        env=environment,
        check=True,
        timeout=10,
    )
    responses = [json.loads(line) for line in result.stdout.splitlines() if line]
    assert responses[0]["id"] == 1
    assert responses[0]["result"]["serverInfo"]["name"] == "oce-client"
    tools = responses[1]["result"]["tools"]
    assert {tool["name"] for tool in tools} == {"codebase-retrieval"}
    schema = tools[0]["inputSchema"]
    assert schema["required"] == ["information_request"]
