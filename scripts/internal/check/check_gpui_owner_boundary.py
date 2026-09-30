#!/usr/bin/env python3
"""Tripwire against direct durable-owner and filesystem mutation access in yield-ui.

This is a deliberately finite lexical check, not a Rust parser or a proof of
runtime authority. Typed SourceRegistryGlobal/catalog/maintenance APIs and
read-only filesystem inspection remain allowed.
"""

from __future__ import annotations

import argparse
import re
import sys
from pathlib import Path


DEFAULT_ROOT = Path(__file__).resolve().parents[3]
TEST_MODULE = re.compile(r"(?m)^[ \t]*#\[cfg\(test\)\][ \t]*\n[ \t]*mod[ \t]+\w+[ \t]*\{")
MUTATIONS = {
    "write", "rename", "remove_file", "remove_dir", "remove_dir_all",
    "create_dir", "create_dir_all", "copy", "hard_link", "symlink",
    "set_permissions",
}
MUTATION_PATH = re.compile(
    r"\b(?:std\s*::\s*)?(?:os\s*::\s*\w+\s*::\s*)?fs\s*::\s*(\w+)\s*\("
)
OWNER_NAMES = ("GlobalLibraryWriter", "OperationJournalOwner", "OperationJournalStore")


def blank_non_code(text: str) -> str:
    """Replace comments and string/char literals with whitespace, keeping offsets."""
    out = list(text)
    i, n = 0, len(text)

    def blank(start: int, end: int) -> None:
        for pos in range(start, end):
            if out[pos] != "\n":
                out[pos] = " "

    while i < n:
        if text.startswith("//", i):
            end = text.find("\n", i)
            end = n if end < 0 else end
            blank(i, end)
            i = end
            continue
        if text.startswith("/*", i):
            start, depth = i, 1
            i += 2
            while i < n and depth:
                if text.startswith("/*", i):
                    depth += 1
                    i += 2
                elif text.startswith("*/", i):
                    depth -= 1
                    i += 2
                else:
                    i += 1
            blank(start, i)
            continue

        # Raw (possibly byte) strings: r###"..."### and br###"..."###.
        raw_start = i
        if text.startswith("br", i):
            raw_start += 1
        if raw_start < n and text[raw_start] == "r":
            quote = raw_start + 1
            while quote < n and text[quote] == "#":
                quote += 1
            if quote < n and text[quote] == '"':
                hashes = text[raw_start + 1 : quote]
                end_token = '"' + hashes
                end = text.find(end_token, quote + 1)
                end = n if end < 0 else end + len(end_token)
                blank(i, end)
                i = end
                continue

        # Ordinary strings and byte strings.
        quote_i = i + 1 if text[i] == "b" and i + 1 < n and text[i + 1] == '"' else i
        if text[quote_i] == '"':
            start = i
            i = quote_i + 1
            while i < n:
                if text[i] == "\\":
                    i += 2
                elif text[i] == '"':
                    i += 1
                    break
                else:
                    i += 1
            blank(start, i)
            continue

        # Character literals (but leave Rust lifetimes such as 'a untouched).
        if text[i] == "'" and i + 2 < n:
            end = i + 1
            if text[end] == "\\":
                end += 2
            else:
                end += 1
            if end < n and text[end] == "'":
                blank(i, end + 1)
                i = end + 1
                continue
        i += 1
    return "".join(out)


def production_text(text: str) -> tuple[str, list[str]]:
    code = blank_non_code(text)
    errors = []
    # Mask the balanced module body, not merely its first nested closing brace.
    for marker in reversed(list(TEST_MODULE.finditer(code))):
        depth = 1
        end = marker.end()
        while end < len(code) and depth:
            if code[end] == "{":
                depth += 1
            elif code[end] == "}":
                depth -= 1
            end += 1
        if depth:
            errors.append("cfg(test) module has no matching closing brace")
            continue
        start = marker.start()
        code = code[:start] + "".join("\n" if c == "\n" else " " for c in code[start:end]) + code[end:]
    return code, errors


def check_source(relative: str, text: str) -> list[str]:
    code, errors = production_text(text)
    line = lambda pos: code.count("\n", 0, pos) + 1
    for match in re.finditer(r"\brusqlite\s*::|\bConnection\s*::\s*open(?:_with_flags|_in_memory)?\s*\(", code):
        errors.append(f"{relative}:{line(match.start())}: direct SQLite access in yield-ui")
    for name in OWNER_NAMES:
        for match in re.finditer(rf"\b{name}\b", code):
            errors.append(f"{relative}:{line(match.start())}: direct durable-owner access ({name}) in yield-ui")

    imported_mutations: set[str] = set()
    # Recognize imported filesystem function aliases and `use std::fs as alias`.
    fs_aliases = {"fs"}
    for match in re.finditer(r"\buse\s+std\s*::\s*fs\s+as\s+(\w+)", code):
        fs_aliases.add(match.group(1))
    for match in re.finditer(r"\buse\s+std\s*::\s*fs\s*::\s*\{([^}]+)\}", code):
        for item in match.group(1).split(","):
            parts = re.match(r"\s*(\w+)(?:\s+as\s+(\w+))?\s*$", item)
            if parts and parts.group(1) in MUTATIONS:
                imported_mutations.add(parts.group(2) or parts.group(1))
    for match in re.finditer(r"\buse\s+std\s*::\s*fs\s*::\s*(\w+)(?:\s+as\s+(\w+))?\s*;", code):
        if match.group(1) in MUTATIONS:
            imported_mutations.add(match.group(2) or match.group(1))

    for match in MUTATION_PATH.finditer(code):
        if match.group(1) in MUTATIONS:
            errors.append(f"{relative}:{line(match.start())}: direct filesystem mutation ({match.group(1)}) in yield-ui")
    for alias in sorted(fs_aliases - {"fs"}):
        for match in re.finditer(rf"\b{re.escape(alias)}\s*::\s*(\w+)\s*\(", code):
            if match.group(1) in MUTATIONS:
                errors.append(f"{relative}:{line(match.start())}: aliased filesystem mutation ({match.group(1)}) in yield-ui")
    for name in sorted(imported_mutations):
        for match in re.finditer(rf"\b{re.escape(name)}\s*\(", code):
            errors.append(f"{relative}:{line(match.start())}: imported filesystem mutation ({name}) in yield-ui")

    file_aliases = {"File"}
    options_aliases = {"OpenOptions"}
    for match in re.finditer(r"\buse\s+std\s*::\s*fs\s*::\s*\{([^}]+)\}", code):
        for item in match.group(1).split(","):
            parts = re.match(r"\s*(File|OpenOptions)(?:\s+as\s+(\w+))?\s*$", item)
            if parts:
                (file_aliases if parts.group(1) == "File" else options_aliases).add(parts.group(2) or parts.group(1))
    for path in (
        r"std\s*::\s*fs\s*::\s*File",
        r"fs\s*::\s*File",
        r"std\s*::\s*fs\s*::\s*OpenOptions",
        r"fs\s*::\s*OpenOptions",
    ):
        found = re.search(rf"\b{path}\s+as\s+(\w+)", code)
        if found:
            if "File" in path:
                file_aliases.add(found.group(1))
            else:
                options_aliases.add(found.group(1))
    for alias in sorted(file_aliases):
        for match in re.finditer(rf"\b{re.escape(alias)}\s*::\s*create\s*\(", code):
            errors.append(f"{relative}:{line(match.start())}: File::create mutation in yield-ui")
    for alias in sorted(options_aliases):
        for match in re.finditer(rf"\b{re.escape(alias)}\s*::\s*new\s*\(\)([^;]{{0,320}})", code):
            if re.search(r"\.\s*(?:write|append|create|create_new|truncate)\s*\(\s*true\s*\)", match.group(1)):
                errors.append(f"{relative}:{line(match.start())}: writable OpenOptions in yield-ui")
    return errors


def scan(root: Path) -> list[str]:
    source_root = root / "crates/yield-ui/src"
    if not source_root.is_dir():
        return [f"{source_root}: missing yield-ui source tree"]
    errors = []
    for path in sorted(source_root.rglob("*.rs")):
        rel = path.relative_to(root).as_posix()
        errors.extend(check_source(rel, path.read_text(encoding="utf-8")))
    return errors


def self_test() -> None:
    good = """
use std::fs;
use yield_ui::SourceRegistryGlobal;
fn reads() { let _ = fs::metadata(\"x\"); let _ = std::fs::read(\"x\"); }
fn catalog(global: &SourceRegistryGlobal) { global.submit_catalog(command); }
// std::fs::remove_file(\"x\"); rusqlite::Connection::open(\"x\");
const S: &str = \"File::create(\";
const R: &str = r#\"rusqlite::Connection::open(\\\"x\\\")\"#;
/* nested comment: /* std::fs::remove_file(\\\"x\\\") */ */
fn readonly() { let _ = std::fs::File::open(\"x\"); }
#[cfg(test)]
mod tests { fn fixture() { std::fs::write(\"x\", b\"x\"); } }
fn after_fixture() { let _ = std::fs::metadata(\"x\"); }
"""
    assert not check_source("good.rs", good)
    bad_cases = (
        "use rusqlite::Connection;",
        "Connection::open(\"x\");",
        "use yield_library::GlobalLibraryWriter;",
        "OperationJournalStore::open();",
        "std::fs::remove_file(\"x\");",
        "use std::fs as disk; disk::rename(a, b);",
        "use std::fs::{write as persist}; persist(a, b);",
        "use std::fs::write; write(a, b);",
        "use std::fs::{write, rename}; write(a, b); rename(a, b);",
        "std::fs::File::create(\"x\");",
        "std::fs::OpenOptions::new().write(true).create(true);",
        "std::fs::OpenOptions::new()\n    .write(true)\n    .create_new(true)\n    .open(\"x\");",
        "#[cfg(test)]\nmod tests { fn fixture() {} }\nfn production() { std::fs::remove_file(\"x\"); }",
    )
    for fixture in bad_cases:
        assert check_source("bad.rs", fixture), fixture
    assert not check_source("fixture.rs", "#[cfg(test)]\nmod tests { fn f() { fs::remove_file(\"x\"); } }\nfn later() {}")
    assert check_source("malformed.rs", "#[cfg(test)]\nmod tests { fn f() {} ")


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--self-test", action="store_true")
    parser.add_argument("--root", type=Path, default=DEFAULT_ROOT, help=argparse.SUPPRESS)
    args = parser.parse_args()
    if args.self_test:
        self_test()
        print("[gpui_owner_boundary] self-test OK")
        return 0
    errors = scan(args.root.resolve())
    if errors:
        for error in errors:
            print(f"[gpui_owner_boundary] {error}", file=sys.stderr)
        return 1
    print("[gpui_owner_boundary] OK")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
