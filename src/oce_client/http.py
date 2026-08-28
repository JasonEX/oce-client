from __future__ import annotations

import time
from typing import Sequence

import httpx

from .defaults import DEFAULT_API_KEY, DEFAULT_API_URL
from .models import (
    BlobStatusResult,
    BlobUpload,
    CheckpointResult,
    MissingResult,
    RetrievalResult,
    UploadResult,
)


class OceApiError(RuntimeError):
    def __init__(self, status_code: int, detail: str) -> None:
        super().__init__(f"OCE API request failed ({status_code}): {detail}")
        self.status_code = status_code
        self.detail = detail


class OceHttpClient:
    """Synchronous adapter for the public ACE-compatible OCE endpoints."""

    def __init__(
        self,
        api_url: str = DEFAULT_API_URL,
        api_key: str = DEFAULT_API_KEY,
        *,
        timeout: float | httpx.Timeout = 60.0,
        client: httpx.Client | None = None,
    ) -> None:
        self.api_url = api_url.rstrip("/")
        self._owns_client = client is None
        self._client = client or httpx.Client(timeout=timeout)
        self._headers = {
            "Authorization": f"Bearer {api_key}",
            "Content-Type": "application/json",
            "User-Agent": "oce-client/0.1",
        }

    def close(self) -> None:
        if self._owns_client:
            self._client.close()

    def _post(self, endpoint: str, payload: dict[str, object]) -> dict[str, object]:
        response = self._client.post(
            f"{self.api_url}/{endpoint}", json=payload, headers=self._headers
        )
        if response.is_error:
            try:
                detail = str(response.json().get("detail", response.text))
            except ValueError:
                detail = response.text
            raise OceApiError(response.status_code, detail)
        data = response.json()
        if not isinstance(data, dict):
            raise OceApiError(response.status_code, "response must be a JSON object")
        return data

    def find_missing(self, blob_names: Sequence[str]) -> MissingResult:
        data = self._post("find-missing", {"mem_object_names": list(blob_names)})
        return MissingResult(
            tuple(str(v) for v in data.get("unknown_memory_names", [])),
            tuple(str(v) for v in data.get("nonindexed_blob_names", [])),
        )

    def batch_upload(self, blobs: Sequence[BlobUpload]) -> UploadResult:
        data = self._post(
            "batch-upload",
            {"blobs": [{"path": b.path, "content": b.content} for b in blobs]},
        )
        return UploadResult(tuple(str(v) for v in data.get("blob_names", [])))

    @staticmethod
    def _blobs_payload(
        checkpoint_id: str | None,
        added_blobs: Sequence[str],
        deleted_blobs: Sequence[str],
    ) -> dict[str, object]:
        return {
            "checkpoint_id": checkpoint_id or "",
            "added_blobs": list(added_blobs),
            "deleted_blobs": list(deleted_blobs),
        }

    def blob_status(
        self, blob_names: Sequence[str], checkpoint_id: str | None = None
    ) -> BlobStatusResult:
        data = self._post(
            "agents/blob-status",
            {"blobs": self._blobs_payload(checkpoint_id, blob_names, ())},
        )
        return BlobStatusResult(
            tuple(str(v) for v in data.get("unknown_blob_names", [])),
            tuple(str(v) for v in data.get("nonindexed_blob_names", [])),
            bool(data.get("checkpoint_not_found", False)),
        )

    def checkpoint(
        self,
        checkpoint_id: str | None,
        added_blobs: Sequence[str],
        deleted_blobs: Sequence[str],
    ) -> CheckpointResult:
        data = self._post(
            "checkpoint-blobs",
            {"blobs": self._blobs_payload(checkpoint_id, added_blobs, deleted_blobs)},
        )
        return CheckpointResult(str(data["new_checkpoint_id"]))

    def retrieve(
        self,
        query: str,
        checkpoint_id: str | None,
        added_blobs: Sequence[str],
        deleted_blobs: Sequence[str],
    ) -> RetrievalResult:
        started = time.perf_counter()
        data = self._post(
            "agents/codebase-retrieval",
            {
                "information_request": query,
                "blobs": self._blobs_payload(checkpoint_id, added_blobs, deleted_blobs),
                "chat_history": [],
            },
        )
        elapsed = int((time.perf_counter() - started) * 1000)
        return RetrievalResult(str(data.get("formatted_retrieval", "")), elapsed)
