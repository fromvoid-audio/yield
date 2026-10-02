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
import tempfile
from collections import defaultdict
from pathlib import Path

from check_gpui_owner_boundary import DEFAULT_ROOT, production_text


MESSAGES = re.compile(
    r"\b(FolderBrowserMessage|GuiMessage|MetadataMessage|SettingsMessage|WaveformInteraction|PlaymarkLabelMessage)"
    r"\s*::\s*(\w+)"
)
FUNCTIONS = re.compile(r"\bfn\s+(\w+)\s*(?:<[^{};]*>)?\s*\(")


def production_paths(paths: list[Path]) -> tuple[list[Path], list[Path]]:
    """Exclude external cfg(test) module trees as well as inline test bodies."""
    excluded_roots = []
    for path in paths:
        code, errors = production_text(path.read_text())
        if errors:
            raise ValueError(f"{path}: {'; '.join(errors)}")
        base = path.parent if path.name in ("mod.rs", "lib.rs", "main.rs") else path.with_suffix("")
        for match in re.finditer(r"#\[cfg\(test\)\]\s*mod\s+(\w+)\s*;", code):
            excluded_roots.append(base / match.group(1))
    included, excluded = [], []
    for path in paths:
        if any(path == root.with_suffix(".rs") or root in path.parents for root in excluded_roots):
            excluded.append(path)
        else:
            included.append(path)
    return included, excluded


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
    shortcuts = root / "reference/radiant/src/native_app/shell/shortcuts.rs"
    waveform = root / "reference/radiant/src/native_app/waveform.rs"
    if not entry.is_file() or not chrome.is_dir() or not ui.is_dir():
        raise ValueError("missing reference chrome or yield-ui source tree")
    references: dict[str, list[dict]] = defaultdict(list)
    functions: dict[str, list[dict]] = defaultdict(list)
    inputs = []
    excluded_inputs = []
    for paths, pattern, target, channel in (
        ([entry, *sorted(chrome.rglob("*.rs"))], MESSAGES, references, "chrome"),
        ([shortcuts, *sorted(shortcuts.with_suffix("").rglob("*.rs"))], MESSAGES, references, "keyboard"),
        ([waveform, *sorted(waveform.with_suffix("").rglob("*.rs"))], MESSAGES, references, "waveform"),
        (sorted(ui.rglob("*.rs")), FUNCTIONS, functions, "gpui"),
    ):
        included, excluded = production_paths(paths)
        excluded_inputs.extend(path.relative_to(root).as_posix() for path in excluded)
        for path in included:
            raw = path.read_bytes()
            relative = path.relative_to(root).as_posix()
            inputs.append({"path": relative, "sha256": hashlib.sha256(raw).hexdigest()})
            for name, line in witnesses(raw.decode("utf-8"), pattern):
                target[name].append({"path": relative, "line": line, "channel": channel})
    head = subprocess.check_output(
        ["git", "-C", str(root), "rev-parse", "HEAD"], text=True
    ).strip()
    return {
        "schema_version": 2,
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
        "excluded_test_inputs": sorted(excluded_inputs),
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
    with tempfile.TemporaryDirectory() as directory:
        root = Path(directory)
        module = root / "controls.rs"
        module.write_text("#[cfg(test)]\nmod fixtures;\nfn live() {}")
        tree = root / "controls/fixtures"
        tree.mkdir(parents=True)
        fixture = root / "controls/fixtures.rs"
        fixture.write_text("GuiMessage::TestOnly;")
        child = tree / "nested.rs"
        child.write_text("GuiMessage::NestedTestOnly;")
        live = root / "controls/live.rs"
        live.write_text("GuiMessage::StopPlayback;")
        included, excluded = production_paths([module, fixture, child, live])
        assert included == [module, live]
        assert excluded == [fixture, child]


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
