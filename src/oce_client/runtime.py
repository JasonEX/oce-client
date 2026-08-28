from __future__ import annotations

import os
from dataclasses import dataclass
from pathlib import Path
from collections.abc import Iterable

from .context import WorkspaceContext
from .defaults import DEFAULT_API_KEY, DEFAULT_API_URL
from .http import OceHttpClient


class ClientConfigurationError(ValueError):
    """Raised when a CLI/MCP runtime cannot be configured safely."""


@dataclass(frozen=True)
class ClientSettings:
    root: Path
    api_url: str
    api_key: str
    state_path: Path | None = None
    runtime_patterns: tuple[str, ...] = ()

    @classmethod
    def from_environment(
        cls,
        *,
        root: str | os.PathLike[str] | None = None,
        api_url: str | None = None,
        api_key: str | None = None,
        state_path: str | os.PathLike[str] | None = None,
        runtime_patterns: tuple[str, ...] = (),
        require_api_key: bool = True,
    ) -> "ClientSettings":
        resolved_root = Path(root or os.environ.get("OCE_WORKSPACE", ".")).resolve()
        resolved_url = (api_url or os.environ.get("OCE_API_URL") or DEFAULT_API_URL).strip()
        resolved_key = (
            api_key
            if api_key is not None
            else os.environ.get("OCE_API_KEY", DEFAULT_API_KEY)
        )
        resolved_state = (
            Path(state_path).expanduser()
            if state_path is not None
            else (
                Path(os.environ["OCE_STATE_PATH"]).expanduser()
                if os.environ.get("OCE_STATE_PATH")
                else None
            )
        )
        if not resolved_url:
            raise ClientConfigurationError("OCE API URL must not be empty")
        if require_api_key and not resolved_key:
            raise ClientConfigurationError(
                "OCE API key is required; pass --api-key or set OCE_API_KEY"
            )
        return cls(
            root=resolved_root,
            api_url=resolved_url.rstrip("/"),
            api_key=resolved_key,
            state_path=resolved_state,
            runtime_patterns=tuple(runtime_patterns),
        )


class ClientRuntime:
    """Lazy, closeable context shared by one CLI command or MCP process."""

    def __init__(self, settings: ClientSettings) -> None:
        self.settings = settings
        self._context: WorkspaceContext | None = None

    def context(self) -> WorkspaceContext:
        if self._context is None:
            if not self.settings.root.is_dir():
                raise ClientConfigurationError(
                    f"workspace is not a directory: {self.settings.root}"
                )
            api = OceHttpClient(self.settings.api_url, self.settings.api_key)
            try:
                self._context = WorkspaceContext.open(
                    self.settings.root,
                    api,
                    state_path=self.settings.state_path,
                    runtime_patterns=self.settings.runtime_patterns,
                )
            except Exception:
                api.close()
                raise
        return self._context

    def close(self) -> None:
        if self._context is not None:
            self._context.close()
            self._context = None

    def __enter__(self) -> "ClientRuntime":
        return self

    def __exit__(self, *_: object) -> None:
        self.close()


def iter_runtime_patterns(values: Iterable[str]) -> tuple[str, ...]:
    return tuple(pattern for value in values for pattern in value.split(",") if pattern)
