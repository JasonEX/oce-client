from __future__ import annotations

import os
from pathlib import Path

from .ignore import LayeredIgnoreMatcher


class FileAdmissionError(ValueError):
    pass


class LocalFileSource:
    def __init__(self, *, max_file_size: int = 1_048_576) -> None:
        self.max_file_size = max_file_size

    def scan(self, root: Path, matcher: LayeredIgnoreMatcher) -> dict[str, str]:
        files: dict[str, str] = {}
        root = root.resolve()
        for directory, dirnames, filenames in os.walk(root, followlinks=False):
            directory_path = Path(directory)
            kept_dirs: list[str] = []
            for name in dirnames:
                path = directory_path / name
                relative = path.relative_to(root).as_posix()
                if not matcher.ignores(relative, is_dir=True):
                    kept_dirs.append(name)
            dirnames[:] = kept_dirs
            for name in filenames:
                path = directory_path / name
                try:
                    relative = path.relative_to(root).as_posix()
                except ValueError:
                    continue
                if matcher.ignores(relative):
                    continue
                try:
                    stat = path.stat()
                    if stat.st_size > self.max_file_size:
                        continue
                    content = path.read_bytes()
                    if b"\x00" in content:
                        continue
                    files[relative] = content.decode("utf-8", errors="strict")
                except (OSError, UnicodeDecodeError):
                    continue
        return files

    def read(self, path: Path) -> str:
        data = path.read_bytes()
        if len(data) > self.max_file_size:
            raise FileAdmissionError(f"file exceeds {self.max_file_size} bytes: {path}")
        if b"\x00" in data:
            raise FileAdmissionError(f"binary file is not supported: {path}")
        try:
            return data.decode("utf-8", errors="strict")
        except UnicodeDecodeError as exc:
            raise FileAdmissionError(f"file is not valid UTF-8: {path}") from exc
