from __future__ import annotations

from dataclasses import dataclass, field
from enum import Enum


class FileStatus(str, Enum):
    PRESENT = "present"
    SKIPPED = "skipped"
    DELETED = "deleted"


class BlobStatus(str, Enum):
    LOCAL = "local"
    UPLOADED = "uploaded"
    READY = "ready"
    FAILED = "failed"


@dataclass(frozen=True)
class FileRecord:
    path: str
    blob_name: str | None
    status: FileStatus
    content: str | None = None
    size: int = 0
    mtime_ns: int | None = None
    source: str = "filesystem"
    generation: int = 0
    committed_blob_name: str | None = None


@dataclass(frozen=True)
class BlobUpload:
    path: str
    content: str
    blob_name: str


@dataclass(frozen=True)
class BlobDelta:
    checkpoint_id: str | None
    added_blobs: tuple[str, ...] = ()
    deleted_blobs: tuple[str, ...] = ()

    def to_api_dict(self) -> dict[str, object]:
        return {
            "checkpoint_id": self.checkpoint_id or "",
            "added_blobs": list(self.added_blobs),
            "deleted_blobs": list(self.deleted_blobs),
        }


@dataclass(frozen=True)
class UploadPlan:
    uploads: tuple[BlobUpload, ...]
    delta: BlobDelta
    skipped_paths: tuple[str, ...] = ()


@dataclass(frozen=True)
class UploadResult:
    blob_names: tuple[str, ...]


@dataclass(frozen=True)
class MissingResult:
    unknown_blob_names: tuple[str, ...] = ()
    nonindexed_blob_names: tuple[str, ...] = ()


@dataclass(frozen=True)
class BlobStatusResult:
    unknown_blob_names: tuple[str, ...] = ()
    nonindexed_blob_names: tuple[str, ...] = ()
    checkpoint_not_found: bool = False


@dataclass(frozen=True)
class CheckpointResult:
    new_checkpoint_id: str


@dataclass(frozen=True)
class RetrievalResult:
    formatted_retrieval: str
    elapsed_ms: int = 0


@dataclass(frozen=True)
class RetrievalPathsResult:
    paths: tuple[str, ...]
    elapsed_ms: int = 0


@dataclass(frozen=True)
class ProjectOverviewResult:
    key_docs: tuple[dict[str, object], ...] = ()
    sections: tuple[dict[str, object], ...] = ()
    working_set_paths: tuple[str, ...] = ()
    working_set_paths_total: int = 0
    elapsed_ms: int = 0


@dataclass(frozen=True)
class SyncResult:
    uploaded_blob_names: tuple[str, ...]
    checkpoint_id: str | None
    added_blobs: tuple[str, ...]
    deleted_blobs: tuple[str, ...]


@dataclass
class WorkspaceSnapshot:
    files: dict[str, FileRecord] = field(default_factory=dict)
    checkpoint_id: str | None = None
    generation: int = 0
