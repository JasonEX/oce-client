from __future__ import annotations

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
