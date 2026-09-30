#!/usr/bin/env python3
"""Reproducible source witnesses for OPT-1867; never a parity verdict.

This lexical index scans the archived native chrome rather than the broad
controller action enum. It includes forwarding and match references, and does
not evaluate platform cfgs or prove that a control is reachable on macOS.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import re
import subprocess
from collections import defaultdict
from pathlib import Path

from check_gpui_owner_boundary import DEFAULT_ROOT, production_text


MESSAGES = re.compile(
    r"\b(FolderBrowserMessage|GuiMessage|MetadataMessage|SettingsMessage)"
    r"\s*::\s*(\w+)"
)
FUNCTIONS = re.compile(r"\bfn\s+(\w+)\s*(?:<[^{};]*>)?\s*\(")


def witnesses(text: str, pattern: re.Pattern[str]) -> list[tuple[str, int]]:
    code, errors = production_text(text)
    if errors:
        raise ValueError("; ".join(errors))
    return [
        ("::".join(match.groups()), code.count("\n", 0, match.start()) + 1)
        for match in pattern.finditer(code)
    ]


def index(root: Path) -> dict:
    chrome = root / "reference/radiant/src/native_app/app_chrome"
    entry = chrome.with_suffix(".rs")
    ui = root / "crates/yield-ui/src"
    if not entry.is_file() or not chrome.is_dir() or not ui.is_dir():
        raise ValueError("missing reference chrome or yield-ui source tree")
    references: dict[str, list[dict]] = defaultdict(list)
    functions: dict[str, list[dict]] = defaultdict(list)
    inputs = []
    for paths, pattern, target in (
        ([entry, *sorted(chrome.rglob("*.rs"))], MESSAGES, references),
        (sorted(ui.rglob("*.rs")), FUNCTIONS, functions),
    ):
        for path in paths:
            raw = path.read_bytes()
            relative = path.relative_to(root).as_posix()
            inputs.append({"path": relative, "sha256": hashlib.sha256(raw).hexdigest()})
            for name, line in witnesses(raw.decode("utf-8"), pattern):
                target[name].append({"path": relative, "line": line})
    head = subprocess.check_output(
        ["git", "-C", str(root), "rev-parse", "HEAD"], text=True
    ).strip()
    return {
        "schema_version": 1,
        "issue": "OPT-1867",
        "checkout_head": head,
        "scope": "lexical source witnesses; no reachability or parity verdict",
        "limitations": [
            "Match arms and forwarding references are included.",
            "Platform cfgs, aliases, macro expansion, and runtime visibility are not evaluated.",
            "Test modules, comments, and string literals are excluded by the shared lexical scanner.",
            "GPUI function names are navigation aids, not semantic matches or automated evidence.",
            "Use input hashes to detect local changes; checkout_head alone does not certify the inputs.",
        ],
        "inputs": sorted(inputs, key=lambda row: row["path"]),
        "reference_messages": [
            {"message": name, "witnesses": references[name]}
            for name in sorted(references)
        ],
        "gpui_functions": [
            {"function": name, "witnesses": functions[name]}
            for name in sorted(functions)
        ],
    }


def self_test() -> None:
    source = '''// GuiMessage::Comment
const S: &str = "GuiMessage::String";
fn control() { GuiMessage::ToggleLoopPlayback; }
#[cfg(test)]
mod tests { fn fixture() { GuiMessage::Fixture; } }
fn after() { SettingsMessage :: SetVolume; }
'''
    assert witnesses(source, MESSAGES) == [
        ("GuiMessage::ToggleLoopPlayback", 3), ("SettingsMessage::SetVolume", 6)
    ]
    assert witnesses(source, FUNCTIONS) == [("control", 3), ("after", 6)]
    try:
        witnesses("#[cfg(test)]\nmod tests {", MESSAGES)
    except ValueError:
        pass
    else:
        raise AssertionError("malformed test module must fail closed")


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--root", type=Path, default=DEFAULT_ROOT)
    parser.add_argument("--self-test", action="store_true")
    args = parser.parse_args()
    if args.self_test:
        self_test()
        print("[gpui_reference_controls] self-test OK")
    else:
        print(json.dumps(index(args.root.resolve()), indent=2, sort_keys=True))


if __name__ == "__main__":
    main()
