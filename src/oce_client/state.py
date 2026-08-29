from __future__ import annotations

import json
import sqlite3
import threading
import time
from contextlib import contextmanager
from pathlib import Path
from typing import Iterator

from .models import FileRecord, FileStatus, WorkspaceSnapshot


class SQLiteStateStore:
    """Durable workspace state with an outbox-friendly transactional boundary."""

    def __init__(self, path: Path) -> None:
        self.path = Path(path)
        self.path.parent.mkdir(parents=True, exist_ok=True)
        self._lock = threading.RLock()
        self._conn = sqlite3.connect(
            str(self.path), timeout=30.0, check_same_thread=False
        )
        self._conn.row_factory = sqlite3.Row
        self._conn.execute("PRAGMA journal_mode=WAL")
        self._conn.execute("PRAGMA busy_timeout=30000")
        self._init_schema()

    def close(self) -> None:
        with self._lock:
            self._conn.close()

    def _init_schema(self) -> None:
        with self._lock, self._conn:
            self._conn.executescript(
                """
                CREATE TABLE IF NOT EXISTS meta (
                    key TEXT PRIMARY KEY,
                    value TEXT NOT NULL
                );
                CREATE TABLE IF NOT EXISTS files (
                    path TEXT PRIMARY KEY,
                    blob_name TEXT,
                    committed_blob_name TEXT,
                    status TEXT NOT NULL,
                    content TEXT,
                    size INTEGER NOT NULL DEFAULT 0,
                    mtime_ns INTEGER,
                    source TEXT NOT NULL DEFAULT 'filesystem',
                    generation INTEGER NOT NULL DEFAULT 0,
                    skip_reason TEXT
                );
                CREATE TABLE IF NOT EXISTS outbox_operations (
                    id INTEGER PRIMARY KEY AUTOINCREMENT,
                    kind TEXT NOT NULL,
                    payload TEXT NOT NULL,
                    status TEXT NOT NULL DEFAULT 'pending',
                    attempts INTEGER NOT NULL DEFAULT 0,
                    last_error TEXT,
                    created_at REAL NOT NULL
                );
                """
            )

    @contextmanager
    def transaction(self) -> Iterator[sqlite3.Connection]:
        with self._lock:
            self._conn.execute("BEGIN IMMEDIATE")
            try:
                yield self._conn
            except Exception:
                self._conn.rollback()
                raise
            else:
                self._conn.commit()

    def get_meta(self, key: str) -> str | None:
        row = self._conn.execute("SELECT value FROM meta WHERE key = ?", (key,)).fetchone()
        return None if row is None else str(row["value"])

    def set_meta(self, key: str, value: str | None) -> None:
        with self.transaction() as conn:
            if value is None:
                conn.execute("DELETE FROM meta WHERE key = ?", (key,))
            else:
                conn.execute(
                    "INSERT INTO meta(key, value) VALUES (?, ?) "
                    "ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                    (key, value),
                )

    def load_snapshot(self) -> WorkspaceSnapshot:
        rows = self._conn.execute(
            "SELECT path, blob_name, committed_blob_name, status, "
            "CASE WHEN source = 'explicit' THEN content ELSE NULL END AS content, "
            "size, mtime_ns, source, generation, skip_reason "
            "FROM files ORDER BY path"
        ).fetchall()
        files = {
            row["path"]: FileRecord(
            path=row["path"],
            blob_name=row["blob_name"],
            status=FileStatus(row["status"]),
            committed_blob_name=row["committed_blob_name"],
            content=row["content"],
                size=row["size"],
                mtime_ns=row["mtime_ns"],
                source=row["source"],
                generation=row["generation"],
            )
            for row in rows
        }
        return WorkspaceSnapshot(
            files=files,
            checkpoint_id=self.get_meta("checkpoint_id"),
            generation=int(self.get_meta("generation") or "0"),
        )

    def load_file_rows(self) -> list[sqlite3.Row]:
        return self._conn.execute(
            "SELECT path, blob_name, committed_blob_name, status, size, mtime_ns, "
            "source, generation, skip_reason FROM files ORDER BY path"
        ).fetchall()

    def load_file_content(self, path: str) -> str | None:
        row = self._conn.execute(
            "SELECT content FROM files WHERE path = ?",
            (path,),
        ).fetchone()
        if row is None or row["content"] is None:
            return None
        return str(row["content"])

    def upsert_file(self, record: FileRecord, *, committed_blob_name: str | None = None) -> None:
        with self.transaction() as conn:
            existing = conn.execute(
                "SELECT committed_blob_name FROM files WHERE path = ?", (record.path,)
            ).fetchone()
            committed = (
                committed_blob_name
                if committed_blob_name is not None
                else (existing["committed_blob_name"] if existing else None)
            )
            conn.execute(
                """INSERT INTO files
                   (path, blob_name, committed_blob_name, status, content, size,
                    mtime_ns, source, generation, skip_reason)
                   VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
                   ON CONFLICT(path) DO UPDATE SET
                     blob_name=excluded.blob_name,
                     committed_blob_name=COALESCE(excluded.committed_blob_name, files.committed_blob_name),
                     status=excluded.status, content=excluded.content,
                     size=excluded.size, mtime_ns=excluded.mtime_ns,
                     source=excluded.source, generation=excluded.generation""",
                (
                    record.path,
                    record.blob_name,
                    committed,
                    record.status.value,
                    record.content,
                    record.size,
                    record.mtime_ns,
                    record.source,
                    record.generation,
                    None,
                ),
            )

    def apply_file_changes(
        self,
        records: list[FileRecord],
        deleted_paths: list[str],
        generation: int,
    ) -> None:
        with self.transaction() as conn:
            conn.executemany(
                """INSERT INTO files(path, blob_name, committed_blob_name, status,
                   content, size, mtime_ns, source, generation, skip_reason)
                   VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
                   ON CONFLICT(path) DO UPDATE SET blob_name=excluded.blob_name,
                   status=excluded.status, content=excluded.content,
                   size=excluded.size, mtime_ns=excluded.mtime_ns,
                   source=excluded.source, generation=excluded.generation""",
                [
                    (
                        record.path,
                        record.blob_name,
                        None,
                        record.status.value,
                        record.content,
                        record.size,
                        record.mtime_ns,
                        record.source,
                        generation,
                        None,
                    )
                    for record in records
                ],
            )
            if deleted_paths:
                conn.executemany(
                    "UPDATE files SET status = ?, blob_name = NULL, content = NULL, "
                    "source = 'filesystem', generation = ? WHERE path = ?",
                    [
                        (FileStatus.DELETED.value, generation, path)
                        for path in deleted_paths
                    ],
                )
            conn.execute(
                "INSERT INTO meta(key,value) VALUES('generation',?) "
                "ON CONFLICT(key) DO UPDATE SET value=excluded.value",
                (str(generation),),
            )

    def mark_missing_paths(self, paths: list[str], generation: int) -> None:
        if not paths:
            return
        with self.transaction() as conn:
            conn.executemany(
                "UPDATE files SET status = ?, blob_name = NULL, content = NULL, generation = ? "
                "WHERE path = ? AND source != 'explicit'",
                [(FileStatus.DELETED.value, generation, path) for path in paths],
            )

    def mark_deleted_paths(self, paths: list[str], generation: int) -> None:
        if not paths:
            return
        with self.transaction() as conn:
            conn.executemany(
                "UPDATE files SET status = ?, blob_name = NULL, content = NULL, "
                "source = 'filesystem', generation = ? WHERE path = ?",
                [(FileStatus.DELETED.value, generation, path) for path in paths],
            )

    def commit_sync(
        self,
        checkpoint_id: str,
        deleted_paths: list[str],
        generation: int,
    ) -> None:
        with self.transaction() as conn:
            conn.execute("UPDATE files SET committed_blob_name = blob_name WHERE status = 'present'")
            conn.execute("UPDATE files SET content = NULL WHERE source = 'filesystem'")
            if deleted_paths:
                conn.executemany("DELETE FROM files WHERE path = ?", [(p,) for p in deleted_paths])
            conn.execute(
                "INSERT INTO meta(key,value) VALUES('checkpoint_id',?) "
                "ON CONFLICT(key) DO UPDATE SET value=excluded.value",
                (checkpoint_id,),
            )
            conn.execute(
                "INSERT INTO meta(key,value) VALUES('generation',?) "
                "ON CONFLICT(key) DO UPDATE SET value=excluded.value",
                (str(generation),),
            )
            conn.execute(
                "INSERT INTO meta(key,value) VALUES('synced_generation',?) "
                "ON CONFLICT(key) DO UPDATE SET value=excluded.value",
                (str(generation),),
            )

    def add_outbox(self, kind: str, payload: dict[str, object]) -> int:
        with self.transaction() as conn:
            cursor = conn.execute(
                "INSERT INTO outbox_operations(kind,payload,created_at) VALUES(?,?,?)",
                (kind, json.dumps(payload, separators=(",", ":")), time.time()),
            )
            return int(cursor.lastrowid)

    def update_outbox(self, operation_id: int, status: str, error: str | None = None) -> None:
        with self.transaction() as conn:
            conn.execute(
                "UPDATE outbox_operations SET status=?, attempts=attempts+1, last_error=? WHERE id=?",
                (status, error, operation_id),
            )

    def pending_outbox(self) -> list[sqlite3.Row]:
        return self._conn.execute(
            "SELECT * FROM outbox_operations WHERE status != 'complete' ORDER BY id"
        ).fetchall()

    def supersede_pending(self) -> None:
        """Mark older attempts as superseded before a fresh sync is planned."""
        with self.transaction() as conn:
            conn.execute(
                "UPDATE outbox_operations SET status = 'superseded' "
                "WHERE status IN ('pending', 'failed')"
            )
