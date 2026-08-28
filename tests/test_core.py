from __future__ import annotations

from pathlib import Path

import pytest

from oce_client import (
    BlobCompatibilityError,
    BlobStatusResult,
    CheckpointResult,
    FileStatus,
    LayeredIgnoreMatcher,
    MissingResult,
    ProjectOverviewResult,
    RetrievalPathsResult,
    RetrievalResult,
    Sha256BlobIdentity,
    UploadResult,
    WorkspaceContext,
)
from oce_client.http import OceHttpClient


class FakeApi:
    def __init__(self) -> None:
        self.known: set[str] = set()
        self.ready: set[str] = set()
        self.checkpoint_members: set[str] = set()
        self.checkpoint_id: str | None = None
        self.upload_calls = []
        self.checkpoint_calls = []
        self.fail_upload = False
        self.fail_checkpoint = False

    def find_missing(self, names):
        return MissingResult(tuple(n for n in names if n not in self.known), ())

    def batch_upload(self, blobs):
        if self.fail_upload:
            raise RuntimeError("upload failed")
        self.upload_calls.append(tuple(blobs))
        names = tuple(b.blob_name for b in blobs)
        self.known.update(names)
        self.ready.update(names)
        return UploadResult(names)

    def blob_status(self, names, checkpoint_id=None):
        return BlobStatusResult(tuple(n for n in names if n not in self.known), ())

    def checkpoint(self, checkpoint_id, added, deleted):
        if self.fail_checkpoint:
            raise RuntimeError("checkpoint failed")
        self.checkpoint_calls.append((checkpoint_id, tuple(added), tuple(deleted)))
        if checkpoint_id is not None and checkpoint_id != self.checkpoint_id:
            from oce_client import OceApiError

            raise OceApiError(404, "missing checkpoint")
        self.checkpoint_members.update(added)
        self.checkpoint_members.difference_update(deleted)
        self.checkpoint_id = "chain:1" if self.checkpoint_id is None else "chain:2"
        return CheckpointResult(self.checkpoint_id)

    def retrieve(self, query, checkpoint_id, added, deleted):
        return RetrievalResult(f"{query}:{checkpoint_id}")

    def retrieve_paths(self, query, checkpoint_id, added, deleted):
        return RetrievalPathsResult(("src/main.py#L1-2",))

    def project_overview(self, depth, checkpoint_id, added, deleted):
        return ProjectOverviewResult()


def test_hash_matches_oce_contract():
    assert Sha256BlobIdentity().calculate("src/main.py", "print(1)") == (
        "69077264fad0c3c3321a9a1c36b0595f6c8c362e1d9529ac6f17431c753edfff"
    )


def test_ignore_layers_and_negation(tmp_path: Path):
    (tmp_path / ".gitignore").write_text("*.log\nsecret.txt\n", encoding="utf-8")
    (tmp_path / ".oceignore").write_text("!secret.txt\n", encoding="utf-8")
    matcher = LayeredIgnoreMatcher(tmp_path, ["runtime.txt"])
    assert matcher.ignores("app.log")
    assert not matcher.ignores("secret.txt")
    assert matcher.ignores("runtime.txt")
    assert matcher.ignores(".git/config")


def test_sync_add_modify_delete_and_restore(tmp_path: Path):
    (tmp_path / "src").mkdir()
    (tmp_path / "src" / "main.py").write_text("one", encoding="utf-8")
    api = FakeApi()
    context = WorkspaceContext.open(tmp_path, api, ready_poll_attempts=1)
    try:
        first = context.sync()
        assert first.checkpoint_id == "chain:1"
        old_name = first.added_blobs[0]
        (tmp_path / "src" / "main.py").write_text("two", encoding="utf-8")
        second = context.sync()
        assert old_name in second.deleted_blobs
        assert len(second.added_blobs) == 1
        (tmp_path / "src" / "main.py").unlink()
        third = context.sync()
        assert second.added_blobs[0] in third.deleted_blobs
    finally:
        context.close()


def test_upload_failure_does_not_advance_checkpoint(tmp_path: Path):
    (tmp_path / "a.py").write_text("x", encoding="utf-8")
    api = FakeApi()
    context = WorkspaceContext.open(tmp_path, api, ready_poll_attempts=1)
    try:
        api.fail_upload = True
        with pytest.raises(RuntimeError):
            context.sync()
        assert context.snapshot().checkpoint_id is None
        assert context.state.pending_outbox()
    finally:
        context.close()


def test_service_hash_mismatch_is_rejected(tmp_path: Path):
    (tmp_path / "a.py").write_text("x", encoding="utf-8")
    api = FakeApi()
    original = api.batch_upload

    def bad_upload(blobs):
        result = original(blobs)
        return UploadResult(("0" * 64,))

    api.batch_upload = bad_upload
    context = WorkspaceContext.open(tmp_path, api, ready_poll_attempts=1)
    try:
        with pytest.raises(BlobCompatibilityError):
            context.sync()
    finally:
        context.close()


def test_explicit_overlay_survives_disk_reconcile(tmp_path: Path):
    path = tmp_path / "a.py"
    path.write_text("disk", encoding="utf-8")
    api = FakeApi()
    context = WorkspaceContext.open(tmp_path, api, ready_poll_attempts=1)
    try:
        context.observe_file("a.py", "unsaved")
        context.reconcile()
        record = context.snapshot().files["a.py"]
        assert record.source == "explicit"
        assert record.content == "unsaved"
        context.remove_file("a.py")
        assert context.snapshot().files["a.py"].status == FileStatus.DELETED
    finally:
        context.close()


def test_http_adapter_uses_oce_payloads():
    import httpx

    requests = []

    def handler(request: httpx.Request) -> httpx.Response:
        import json

        requests.append((request.url.path, json.loads(request.content)))
        if request.url.path == "/batch-upload":
            return httpx.Response(200, json={"blob_names": ["a" * 64]})
        return httpx.Response(200, json={"unknown_memory_names": [], "nonindexed_blob_names": []})

    client = httpx.Client(transport=httpx.MockTransport(handler))
    api = OceHttpClient("http://oce.test", "secret", client=client)
    try:
        from oce_client import BlobUpload

        api.batch_upload([BlobUpload("a.py", "x", "a" * 64)])
        assert requests == [
            (
                "/batch-upload",
                {"blobs": [{"path": "a.py", "content": "x"}]},
            )
        ]
    finally:
        api.close()
