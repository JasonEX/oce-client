from __future__ import annotations

from pathlib import Path
from collections.abc import Callable
from typing import Protocol, Sequence

from .models import (
    BlobStatusResult,
    BlobUpload,
    CheckpointResult,
    MissingResult,
    RetrievalResult,
    UploadResult,
)


class BlobIdentity(Protocol):
    def calculate(self, path: str, content: str) -> str: ...


class BlobApi(Protocol):
    def find_missing(self, blob_names: Sequence[str]) -> MissingResult: ...

    def batch_upload(self, blobs: Sequence[BlobUpload]) -> UploadResult: ...

    def blob_status(
        self, blob_names: Sequence[str], checkpoint_id: str | None = None
    ) -> BlobStatusResult: ...

    def checkpoint(
        self,
        checkpoint_id: str | None,
        added_blobs: Sequence[str],
        deleted_blobs: Sequence[str],
    ) -> CheckpointResult: ...

    def retrieve(
        self,
        query: str,
        checkpoint_id: str | None,
        added_blobs: Sequence[str],
        deleted_blobs: Sequence[str],
    ) -> RetrievalResult: ...


class StateStore(Protocol):
    def load_snapshot(self): ...


class WatchHandle(Protocol):
    def stop(self) -> None: ...

    def join(self, timeout: float | None = None) -> None: ...


class FileSource(Protocol):
    def scan(self, root: Path, matcher: IgnoreMatcher) -> dict[str, str]: ...

    def read(self, path: Path) -> str: ...


class IgnoreMatcher(Protocol):
    def ignores(self, path: str, *, is_dir: bool = False) -> bool: ...


class Watcher(Protocol):
    def start(self, callback: Callable[[], None]) -> WatchHandle: ...
