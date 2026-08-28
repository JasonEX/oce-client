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


def test_mcp_exposes_workspace_tools_and_local_state(tmp_path: Path):
    server = create_server(ClientSettings(tmp_path, "http://127.0.0.1:1", "test-key"))
    tools = asyncio.run(server.list_tools())
    assert {tool.name for tool in tools} == {
        "sync_workspace",
        "retrieve_code",
        "observe_file",
        "remove_file",
    }
    assert _call(server, "observe_file", {"path": "a.py", "content": "x"}) == {
        "path": "a.py",
        "status": "present",
    }
    assert "workspace_status" not in {tool.name for tool in tools}


def test_mcp_validates_tool_arguments(tmp_path: Path):
    server = create_server(ClientSettings(tmp_path, "http://127.0.0.1:1", "test-key"))
    with pytest.raises(Exception):
        _call(server, "retrieve_code", {"query": "x", "scope": "invalid"})


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
    names = {tool["name"] for tool in responses[1]["result"]["tools"]}
    assert names == {"sync_workspace", "retrieve_code", "observe_file", "remove_file"}
