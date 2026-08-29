from __future__ import annotations

import json
from pathlib import Path

from oce_client.cli import build_parser, main
from oce_client.runtime import DEFAULT_API_KEY, DEFAULT_API_URL, ClientSettings


def test_status_is_local_and_does_not_require_api_key(tmp_path: Path, capsys):
    assert main(["--root", str(tmp_path), "status", "--json"]) == 0
    payload = json.loads(capsys.readouterr().out)
    assert payload["checkpoint_id"] is None
    assert payload["generation"] == 0
    assert payload["files"] == {}


def test_observe_then_status_round_trip(tmp_path: Path, capsys):
    assert main(["--root", str(tmp_path), "observe", "src/main.py", "--content", "print(1)"]) == 0
    capsys.readouterr()
    assert main(["--root", str(tmp_path), "status", "--json"]) == 0
    payload = json.loads(capsys.readouterr().out)
    assert payload["files"]["src/main.py"]["source"] == "explicit"
    assert payload["files"]["src/main.py"]["status"] == "present"
    assert "content" not in payload["files"]["src/main.py"]


def test_default_service_settings(monkeypatch, tmp_path: Path):
    monkeypatch.delenv("OCE_API_URL", raising=False)
    monkeypatch.delenv("OCE_API_KEY", raising=False)
    settings = ClientSettings.from_environment(root=tmp_path)
    assert settings.api_url == DEFAULT_API_URL
    assert settings.api_key == DEFAULT_API_KEY


def test_explicit_empty_api_key_is_rejected(tmp_path: Path, capsys):
    assert main(["--root", str(tmp_path), "--api-key", "", "sync"]) == 1
    assert "OCE API key is required" in capsys.readouterr().err


def test_environment_values_are_used(tmp_path: Path, monkeypatch, capsys):
    monkeypatch.setenv("OCE_WORKSPACE", str(tmp_path))
    assert main(["status", "--json"]) == 0
    payload = json.loads(capsys.readouterr().out)
    assert payload["generation"] == 0


def test_mcp_runtime_options_are_available_from_unified_cli(tmp_path: Path):
    args = build_parser().parse_args(
        [
            "mcp",
            "--workspace",
            str(tmp_path),
            "--state-dir",
            str(tmp_path / "state"),
            "--debounce-ms",
            "250",
            "--initial-sync",
            "off",
            "--ready-timeout",
            "1.5",
            "--log-level",
            "error",
        ]
    )
    assert args.workspace == [str(tmp_path)]
    assert args.debounce_ms == 250
    assert args.initial_sync == "off"
    assert args.ready_timeout == 1.5
    assert args.log_level == "error"


def test_skill_can_be_located_and_installed(tmp_path: Path, capsys):
    assert main(["skill", "path", "--json"]) == 0
    source = Path(json.loads(capsys.readouterr().out)["path"])
    assert (source / "SKILL.md").is_file()

    target = tmp_path / "skills" / "oce-client"
    assert main(["skill", "install", "--target", str(target), "--json"]) == 0
    payload = json.loads(capsys.readouterr().out)
    assert payload["status"] == "installed"
    assert (target / "SKILL.md").is_file()
    assert (target / "agents" / "openai.yaml").is_file()

    assert main(["skill", "install", "--target", str(target)]) == 1
    assert "pass --force" in capsys.readouterr().err
