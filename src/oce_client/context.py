from __future__ import annotations

import os
import time
from pathlib import Path
from typing import Iterable

from .filesystem import FileAdmissionError, LocalFileSource
from .http import OceApiError
from .identity import Sha256BlobIdentity
from .ignore import LayeredIgnoreMatcher
from .models import (
    BlobDelta,
    BlobUpload,
    FileRecord,
    FileStatus,
    RetrievalResult,
    SyncResult,
    UploadPlan,
    WorkspaceSnapshot,
)
from .ports import BlobApi, BlobIdentity
from .state import SQLiteStateStore
from .watcher import WatchHandle


class CheckpointResetRequired(RuntimeError):
    pass


class BlobCompatibilityError(RuntimeError):
    pass


class WorkspaceContext:
    """Synchronous workspace inventory, blob synchronization, and retrieval facade."""

    def __init__(
        self,
        root: Path,
        api: BlobApi,
        state: SQLiteStateStore,
        *,
        identity: BlobIdentity | None = None,
        file_source: LocalFileSource | None = None,
        runtime_patterns: Iterable[str] = (),
        ready_poll_attempts: int = 20,
        ready_poll_seconds: float = 0.25,
        max_find_missing: int = 1000,
        max_upload_blobs: int = 1000,
        max_upload_bytes: int = 2 * 1024 * 1024,
    ) -> None:
        self.root = root.resolve()
        self.api = api
        self.state = state
        self.identity = identity or Sha256BlobIdentity()
        self.file_source = file_source or LocalFileSource()
        self.runtime_patterns = tuple(runtime_patterns)
        self.ready_poll_attempts = ready_poll_attempts
        self.ready_poll_seconds = ready_poll_seconds
        if max_find_missing < 1 or max_upload_blobs < 1 or max_upload_bytes < 1:
            raise ValueError("sync batch limits must be positive")
        self.max_find_missing = max_find_missing
        self.max_upload_blobs = max_upload_blobs
        self.max_upload_bytes = max_upload_bytes
        self._watch: WatchHandle | None = None

    @classmethod
    def open(
        cls,
        root: str | os.PathLike[str],
        api: BlobApi,
        *,
        state_path: str | os.PathLike[str] | None = None,
        identity: BlobIdentity | None = None,
        file_source: LocalFileSource | None = None,
        runtime_patterns: Iterable[str] = (),
        **kwargs: object,
    ) -> "WorkspaceContext":
        root_path = Path(root).resolve()
        if not root_path.is_dir():
            raise NotADirectoryError(root_path)
        db_path = Path(state_path) if state_path else root_path / ".oce-client" / "state.sqlite3"
        return cls(
            root_path,
            api,
            SQLiteStateStore(db_path),
            identity=identity,
            file_source=file_source,
            runtime_patterns=runtime_patterns,
            **kwargs,
        )

    def close(self) -> None:
        if self._watch is not None:
            self._watch.stop()
            self._watch.join(2.0)
            self._watch = None
        self.state.close()
        close = getattr(self.api, "close", None)
        if close is not None:
            close()

    def _matcher(self) -> LayeredIgnoreMatcher:
        return LayeredIgnoreMatcher(self.root, self.runtime_patterns)

    def _next_generation(self) -> int:
        return self.state.load_snapshot().generation + 1

    def _normalize_path(self, path: str | os.PathLike[str]) -> str:
        candidate = Path(path)
        if candidate.is_absolute():
            try:
                candidate = candidate.resolve().relative_to(self.root)
            except ValueError as exc:
                raise ValueError(f"path is outside workspace: {path}") from exc
        normalized = candidate.as_posix()
        while normalized.startswith("./"):
            normalized = normalized[2:]
        if not normalized or normalized == "." or normalized.startswith("../"):
            raise ValueError(f"invalid workspace-relative path: {path}")
        return normalized

    def observe_file(self, path: str, content: str) -> None:
        normalized = self._normalize_path(path)
        if self._matcher().ignores(normalized):
            raise ValueError(f"path is ignored: {normalized}")
        if not isinstance(content, str):
            raise TypeError("content must be text")
        if len(content.encode("utf-8")) > self.file_source.max_file_size:
            raise FileAdmissionError(f"file exceeds {self.file_source.max_file_size} bytes")
        if "\x00" in content:
            raise FileAdmissionError("binary content is not supported")
        generation = self._next_generation()
        record = FileRecord(
            normalized,
            self.identity.calculate(normalized, content),
            FileStatus.PRESENT,
            content,
            len(content.encode("utf-8")),
            None,
            "explicit",
            generation,
        )
        self.state.upsert_file(record)
        self.state.set_meta("generation", str(generation))

    def remove_file(self, path: str) -> None:
        normalized = self._normalize_path(path)
        generation = self._next_generation()
        self.state.mark_deleted_paths([normalized], generation)
        self.state.set_meta("generation", str(generation))

    def reconcile(self) -> WorkspaceSnapshot:
        generation = self._next_generation()
        scanned = self.file_source.scan(self.root, self._matcher())
        current = self.state.load_snapshot()
        explicit = {
            path: record
            for path, record in current.files.items()
            if record.source == "explicit" and record.status == FileStatus.PRESENT
        }
        records: list[FileRecord] = []
        for path, content in scanned.items():
            overlay = explicit.get(path)
            if overlay is not None:
                disk_blob_name = self.identity.calculate(path, content)
                if disk_blob_name != overlay.blob_name:
                    records.append(overlay)
                    continue
            stat_path = self.root / Path(path)
            try:
                mtime_ns = stat_path.stat().st_mtime_ns
            except OSError:
                mtime_ns = None
            records.append(
                FileRecord(
                    path,
                    self.identity.calculate(path, content),
                    FileStatus.PRESENT,
                    content,
                    len(content.encode("utf-8")),
                    mtime_ns,
                    "filesystem",
                    generation,
                )
            )
        scanned_paths = set(scanned)
        for path, record in explicit.items():
            if path not in scanned_paths:
                records.append(record)
        self.state.replace_files(records, generation)
        known_paths = {record.path for record in records}
        missing_paths = [path for path in current.files if path not in known_paths and path not in explicit]
        self.state.mark_missing_paths(missing_paths, generation)
        return self.state.load_snapshot()

    def snapshot(self) -> WorkspaceSnapshot:
        return self.state.load_snapshot()

    def plan_sync(self) -> UploadPlan:
        snapshot = self.state.load_snapshot()
        uploads: list[BlobUpload] = []
        current_names: set[str] = set()
        committed_names: set[str] = set()
        skipped: list[str] = []
        for record in snapshot.files.values():
            if record.status == FileStatus.PRESENT and record.blob_name:
                current_names.add(record.blob_name)
                if record.content is not None:
                    uploads.append(BlobUpload(record.path, record.content, record.blob_name))
            elif record.status == FileStatus.SKIPPED:
                skipped.append(record.path)
            if record.status != FileStatus.PRESENT and record.committed_blob_name:
                committed_names.add(record.committed_blob_name)
        for row in self.state.load_file_rows():
            if row["committed_blob_name"]:
                committed_names.add(str(row["committed_blob_name"]))
        added = tuple(sorted(current_names - committed_names))
        deleted = tuple(sorted(committed_names - current_names))
        return UploadPlan(
            tuple(uploads),
            BlobDelta(snapshot.checkpoint_id, added, deleted),
            tuple(sorted(skipped)),
        )

    def _wait_ready(self, names: tuple[str, ...], checkpoint_id: str | None) -> None:
        if not names:
            return
        pending = set(names)
        for attempt in range(self.ready_poll_attempts):
            status = self.api.blob_status(tuple(sorted(pending)), checkpoint_id)
            if status.checkpoint_not_found:
                raise CheckpointResetRequired("server checkpoint no longer exists")
            if status.unknown_blob_names:
                raise OceApiError(404, f"unknown blobs: {status.unknown_blob_names}")
            pending = set(status.nonindexed_blob_names)
            if not pending:
                return
            if attempt + 1 < self.ready_poll_attempts:
                time.sleep(self.ready_poll_seconds)
        raise TimeoutError(f"blobs did not become ready: {sorted(pending)}")

    def sync(self) -> SyncResult:
        self.reconcile()
        plan = self.plan_sync()
        payload = {
            "delta": plan.delta.to_api_dict(),
            "uploads": [
                {"path": upload.path, "blob_name": upload.blob_name}
                for upload in plan.uploads
            ],
        }
        # A new sync is a durable retry of any prior failed attempt. The current
        # inventory is authoritative, so retrying from a fresh plan is idempotent.
        self.state.supersede_pending()
        operation_id = self.state.add_outbox("sync", payload)
        try:
            current_names = tuple(
                sorted(
                    record.blob_name
                    for record in self.snapshot().files.values()
                    if record.status == FileStatus.PRESENT and record.blob_name
                )
            )
            unknown: set[str] = set()
            nonindexed: set[str] = set()
            for offset in range(0, len(current_names), self.max_find_missing):
                missing = self.api.find_missing(
                    current_names[offset : offset + self.max_find_missing]
                )
                unknown.update(missing.unknown_blob_names)
                nonindexed.update(missing.nonindexed_blob_names)
            to_upload = unknown | nonindexed
            upload_by_name = {upload.blob_name: upload for upload in plan.uploads}
            actual_uploads = [
                upload_by_name[name] for name in sorted(to_upload) if name in upload_by_name
            ]
            uploaded_names: set[str] = set()
            batch: list[BlobUpload] = []
            batch_bytes = 0
            for upload in actual_uploads:
                upload_bytes = len(upload.path.encode("utf-8")) + len(upload.content.encode("utf-8"))
                if batch and (
                    len(batch) >= self.max_upload_blobs
                    or batch_bytes + upload_bytes > self.max_upload_bytes
                ):
                    result = self.api.batch_upload(batch)
                    expected = {item.blob_name for item in batch}
                    received = set(result.blob_names)
                    if received != expected:
                        raise BlobCompatibilityError(
                            f"batch-upload returned {sorted(received)}; expected {sorted(expected)}"
                        )
                    uploaded_names.update(received)
                    batch = []
                    batch_bytes = 0
                batch.append(upload)
                batch_bytes += upload_bytes
            if batch:
                result = self.api.batch_upload(batch)
                expected = {item.blob_name for item in batch}
                received = set(result.blob_names)
                if received != expected:
                    raise BlobCompatibilityError(
                        f"batch-upload returned {sorted(received)}; expected {sorted(expected)}"
                    )
                uploaded_names.update(received)
            uploaded = tuple(sorted(uploaded_names))
            self._wait_ready(tuple(sorted(to_upload)), None)
            checkpoint_id = plan.delta.checkpoint_id
            try:
                if plan.delta.added_blobs or plan.delta.deleted_blobs or checkpoint_id is None:
                    result = self.api.checkpoint(
                        checkpoint_id,
                        plan.delta.added_blobs,
                        plan.delta.deleted_blobs,
                    )
                    checkpoint_id = result.new_checkpoint_id
            except OceApiError as exc:
                if exc.status_code != 404:
                    raise
                # The server lost the chain. Rebuild the current workspace from scratch.
                self.state.set_meta("checkpoint_id", None)
                result = self.api.checkpoint(None, current_names, ())
                checkpoint_id = result.new_checkpoint_id
            deleted_paths = [
                row["path"]
                for row in self.state.load_file_rows()
                if row["status"] != FileStatus.PRESENT
                and row["committed_blob_name"] in set(plan.delta.deleted_blobs)
            ]
            self.state.commit_sync(checkpoint_id or "", deleted_paths, self.snapshot().generation)
            self.state.update_outbox(operation_id, "complete")
            return SyncResult(uploaded, checkpoint_id, plan.delta.added_blobs, plan.delta.deleted_blobs)
        except Exception as exc:
            self.state.update_outbox(operation_id, "failed", str(exc))
            raise

    def retrieve(self, query: str, *, scope: str = "workspace") -> RetrievalResult:
        if scope not in {"workspace", "working_set"}:
            raise ValueError(f"unknown scope: {scope}")
        if self.state.get_meta("synced_generation") != self.state.get_meta("generation"):
            self.sync()
        snapshot = self.snapshot()
        names = tuple(sorted(record.blob_name for record in snapshot.files.values() if record.status == FileStatus.PRESENT and record.blob_name))
        return self.api.retrieve(query, snapshot.checkpoint_id, names if snapshot.checkpoint_id is None else (), ())

    def start_watching(self, *, debounce_ms: int = 300) -> WatchHandle:
        if self._watch is not None:
            return self._watch
        self._watch = WatchHandle(self.root, self.sync, debounce_ms)
        return self._watch
