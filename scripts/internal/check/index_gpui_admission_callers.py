#!/usr/bin/env python3
"""Index production admission/owner witnesses; declarations are not callers."""

import argparse
import hashlib
import json
import re
import subprocess
from pathlib import Path

from check_gpui_owner_boundary import DEFAULT_ROOT, production_text
from index_gpui_reference_controls import production_paths

PATTERN = re.compile(
    r"\badmit_(?:full|prepared)\s*\(|\bPreparedAdmissionInput\b|"
    r"\bFileOperationOwner\s*::\s*start|"
    r"\bFileOperationCommand\s*::\s*for_\w+"
)


def index(root: Path) -> dict:
    paths, excluded = production_paths(sorted((root / "crates").rglob("*.rs")))
    witnesses = []
    digest = hashlib.sha256()
    for path in paths:
        raw = path.read_bytes()
        relative = path.relative_to(root).as_posix()
        digest.update(relative.encode() + b"\0" + hashlib.sha256(raw).digest())
        code, errors = production_text(raw.decode())
        if errors:
            raise ValueError(f"{relative}: {errors}")
        for match in PATTERN.finditer(code):
            witnesses.append({"path": relative,
                              "line": code.count("\n", 0, match.start()) + 1,
                              "text": match.group(),
                              "sha256": hashlib.sha256(raw).hexdigest()})
    return {"schema_version": 1, "issue": "OPT-2003",
            "checkout_head": subprocess.check_output(["git", "-C", str(root), "rev-parse", "HEAD"], text=True).strip(),
            "scope": "crates/**/*.rs production lexical witnesses, including declarations and worker forwarding",
            "limitations": "No alias/macro/cfg evaluation; no runtime caller or effect proof. Archived reference is excluded.",
            "production_input_count": len(paths), "production_input_digest": digest.hexdigest(),
            "excluded_test_inputs": [p.relative_to(root).as_posix() for p in excluded],
            "witnesses": witnesses}


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--root", type=Path, default=DEFAULT_ROOT)
    args = parser.parse_args()
    print(json.dumps(index(args.root.resolve()), indent=2))
