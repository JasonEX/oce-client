from __future__ import annotations

import threading
from collections.abc import Callable
from pathlib import Path

from watchfiles import watch


_INTERNAL_DIRECTORIES = {".git", ".oce-client"}


def _is_relevant(path: str) -> bool:
    return _INTERNAL_DIRECTORIES.isdisjoint(Path(path).parts)


class WatchHandle:
    def __init__(
        self,
        root: Path,
        callback: Callable[[set[Path]], None],
        debounce_ms: int = 300,
        on_error: Callable[[], None] | None = None,
    ) -> None:
        self._root = root
        self._callback = callback
        self._debounce_ms = debounce_ms
        self._on_error = on_error
        self._error: str | None = None
        self._error_lock = threading.Lock()
        self._stop = threading.Event()
        self._thread = threading.Thread(
            target=self._run,
            name="oce-client-watcher",
            daemon=True,
        )
        self._thread.start()

    def _run(self) -> None:
        try:
            changes = watch(
                self._root,
                stop_event=self._stop,
                debounce=self._debounce_ms,
                yield_on_timeout=False,
            )
            for batch in changes:
                if self._stop.is_set():
                    return
                relevant = {
                    Path(path)
                    for _change, path in batch
                    if _is_relevant(path)
                }
                if relevant:
                    self._callback(relevant)
            if not self._stop.is_set():
                self._record_error("filesystem watcher stopped unexpectedly")
        except Exception as exc:
            if self._stop.is_set():
                return
            self._record_error(
                f"filesystem watcher failed: {type(exc).__name__}: {exc}"
            )

    def _record_error(self, message: str) -> None:
        with self._error_lock:
            self._error = message
        if self._on_error is not None:
            self._on_error()

    @property
    def error(self) -> str | None:
        with self._error_lock:
            return self._error

    def stop(self) -> None:
        self._stop.set()

    def join(self, timeout: float | None = None) -> None:
        self._thread.join(timeout)
