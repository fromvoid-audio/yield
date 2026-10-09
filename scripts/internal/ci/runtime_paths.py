"""Canonicalize temporary-directory paths shared by managed local-CI entrypoints."""

from __future__ import annotations

import os
from pathlib import Path


def canonicalize_tmpdir() -> Path:
    requested = os.environ.get("TMPDIR") or "/tmp"
    try:
        resolved = Path(requested).resolve(strict=True)
    except (OSError, RuntimeError) as error:
        raise RuntimeError(f"TMPDIR must resolve to an existing directory: {requested}: {error}") from error
    if not resolved.is_dir():
        raise RuntimeError(f"TMPDIR must resolve to an existing directory: {requested}")
    os.environ["TMPDIR"] = str(resolved)
    return resolved
