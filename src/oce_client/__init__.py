"""OpenContextEngine client public API."""

__version__ = "0.1.0"

from .context import BlobCompatibilityError, CheckpointResetRequired, WorkspaceContext
from .defaults import DEFAULT_API_KEY, DEFAULT_API_URL
from .filesystem import FileAdmissionError, LocalFileSource
from .http import OceApiError, OceHttpClient
from .identity import Sha256BlobIdentity
from .ignore import LayeredIgnoreMatcher
from .models import (
    BlobDelta,
    BlobStatus,
    BlobStatusResult,
    BlobUpload,
    CheckpointResult,
    FileRecord,
    FileStatus,
    MissingResult,
    RetrievalResult,
    SyncResult,
    UploadPlan,
    UploadResult,
    WorkspaceSnapshot,
)
from .ports import BlobApi, BlobIdentity, FileSource, IgnoreMatcher, StateStore, Watcher
from .runtime import ClientConfigurationError, ClientRuntime, ClientSettings
from .state import SQLiteStateStore
