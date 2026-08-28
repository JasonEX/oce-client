from __future__ import annotations

import threading
from collections.abc import Callable
from pathlib import Path

from watchfiles import Change, watch


class WatchHandle:
    def __init__(self, root: Path, callback: Callable[[], None], debounce_ms: int = 300) -> None:
        self._root = root
        self._callback = callback
        self._debounce_ms = debounce_ms
        self._stop = threading.Event()
        self._thread = threading.Thread(target=self._run, name="oce-client-watcher", daemon=True)
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
                    if ".oce-client" not in Path(path).parts
                }
                if relevant:
                    self._callback()
        except (OSError, RuntimeError):
            # The owning context remains usable; a later explicit reconcile can recover.
            return

    def stop(self) -> None:
        self._stop.set()

    def join(self, timeout: float | None = None) -> None:
        self._thread.join(timeout)
