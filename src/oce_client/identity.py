from __future__ import annotations

import hashlib


class Sha256BlobIdentity:
    """OCE-compatible content address: SHA-256 of normalized path + UTF-8 text."""

    def calculate(self, path: str, content: str) -> str:
        if not isinstance(path, str) or not path:
            raise ValueError("path must be a non-empty string")
        if not isinstance(content, str):
            raise TypeError("content must be text")
        digest = hashlib.sha256()
        digest.update(path.encode("utf-8"))
        digest.update(content.encode("utf-8"))
        return digest.hexdigest()
