"""Validate actual NUL-delimited Git Markdown paths supplied by the Bash entrypoint."""

import os
from pathlib import Path
import re
import sys


def ignored(destination: str) -> bool:
    return (
        not destination
        or destination.lower().startswith(("http://", "https://", "mailto:", "tel:"))
        or destination.startswith(("#", "/"))
        or "${" in destination
        or "{{" in destination
    )


def main() -> int:
    paths = sorted(set(filter(None, sys.stdin.buffer.read().split(b"\0"))))
    if not paths:
        print("[md_links] No changed Markdown files detected.")
        return 0
    link = re.compile(r"!?\[[^\]]*\]\(([^)]+)\)")
    violations = []
    files = links = 0
    for raw_path in paths:
        path = Path(os.fsdecode(raw_path))
        if not path.is_file():
            # Explicit historical ranges still inspect current worktree contents.
            continue
        files += 1
        content = path.read_text(encoding="utf-8", errors="replace")
        for match in link.finditer(content):
            original = match.group(1).strip()
            destination = original
            if destination.startswith("<") and destination.endswith(">"):
                destination = destination[1:-1]
            if ignored(destination):
                continue
            destination = destination.split("#", 1)[0].split("?", 1)[0].strip()
            if ignored(destination):
                continue
            links += 1
            if not (path.parent / destination).resolve().exists():
                violations.append((os.fsdecode(raw_path), original))
    if violations:
        print("[md_links] Broken local file links detected:", file=sys.stderr)
        for file, destination in violations:
            print(f" - {file}: ({destination})", file=sys.stderr)
        return 1
    print(f"[md_links] OK ({files} Markdown files, {links} local links)")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
