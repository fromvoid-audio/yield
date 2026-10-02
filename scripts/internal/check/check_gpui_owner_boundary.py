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
import tempfile
import tomllib
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
OWNER_NAMES = (
    "GlobalLibraryWriter", "LibraryDatabase", "SourceDatabase", "SourceWriter",
    "FileOperationOwner", "OperationJournalOwner", "OperationJournalStore",
    "OperationJournalClient",
)
# UI consumes typed session observations/commands, not background worker crates.
UI_WORKER_CRATES = {"rusqlite", "yield_catalog", "yield_scan", "yield_analysis", "yield_source_watcher"}
UPPER_LAYERS = {
    "yield-library": {"yield-catalog", "yield-session", "yield-ui"},
    "yield-scan": {"yield-catalog", "yield-session", "yield-ui"},
    "yield-analysis": {"yield-catalog", "yield-session", "yield-ui"},
    "yield-source-watcher": {"yield-catalog", "yield-session", "yield-ui"},
    "yield-catalog": {"yield-session", "yield-ui"},
    "yield-session": {"yield-ui"},
    "yield-ui": {name.replace("_", "-") for name in UI_WORKER_CRATES},
}


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
    for name in sorted(UI_WORKER_CRATES):
        for match in re.finditer(rf"\b{name}\b", code):
            errors.append(f"{relative}:{line(match.start())}: direct worker import ({name}) in yield-ui")
    # Explicit data imports are allowed. A wildcard can import/reexport raw factories
    # without spelling the guarded owner, and must be reviewed as named imports.
    for match in re.finditer(r"\b(?:pub\s+)?use\s+yield_library\b[^;]*\*[^;]*;", code):
        errors.append(f"{relative}:{line(match.start())}: wildcard library import in yield-ui")
    raw_modules = [(match.start(), match.end(), match[1]) for match in re.finditer(r"\bsample_sources\s*::\s*(library|db)\b", code)]
    for group in re.finditer(r"\bsample_sources\s*::\s*\{([^;}]+)\}", code):
        raw_modules.extend((group.start(1) + match.start(), group.start(1) + match.end(), match[1]) for match in re.finditer(r"\b(library|db)\b", group[1]))
    for start, end, module in raw_modules:
        # Existing display-label projection uses this pure string/path normalizer.
        # It does not open a connection, capture a root or perform filesystem I/O.
        if module == "db" and re.match(r"\s*::\s*normalize_relative_path\b", code[end:]):
            continue
        errors.append(f"{relative}:{line(start)}: raw database module import in yield-ui")

    imported_mutations: set[str] = set()
    # Recognize imported filesystem function aliases and `use std::fs as alias`.
    fs_aliases = {"fs"}
    for match in re.finditer(r"\buse\s+std\s*::\s*fs\s+as\s+(\w+)", code):
        fs_aliases.add(match.group(1))
    for match in re.finditer(r"\buse\s+std\s*::\s*fs\s*::\s*\{([^}]+)\}", code):
        for item in match.group(1).split(","):
            parts = re.match(r"\s*(\w+)(?:\s+as\s+(\w+))?\s*$", item)
            if parts and parts.group(1) == "self":
                fs_aliases.add(parts.group(2) or "fs")
            elif parts and parts.group(1) in MUTATIONS:
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
        for match in re.finditer(rf"\b{re.escape(alias)}\s*::\s*create(?:_new)?\s*\(", code):
            errors.append(f"{relative}:{line(match.start())}: File::create mutation in yield-ui")
    for alias in sorted(options_aliases):
        builders = set()
        for match in re.finditer(rf"\b{re.escape(alias)}\s*::\s*(?:new|default)\s*\(\)([^;]*)", code):
            prefix = code[max(0, code.rfind(";", 0, match.start()) + 1):match.start()]
            binding = re.search(r"\blet\s+(?:mut\s+)?(\w+)(?:\s*:[^=]+)?\s*=\s*$", prefix)
            if binding:
                builders.add(binding[1])
            if re.search(r"\.\s*(?:write|append|create|create_new|truncate)\s*\(", match.group(1)):
                errors.append(f"{relative}:{line(match.start())}: writable OpenOptions in yield-ui")
        for builder in builders:
            for match in re.finditer(rf"\b{re.escape(builder)}\s*(\.[^;]+)", code):
                if re.search(r"\.\s*(?:write|append|create|create_new|truncate)\s*\(", match.group(1)):
                    errors.append(f"{relative}:{line(match.start())}: configured writable OpenOptions in yield-ui")
    return errors


def check_manifest(relative: str, text: str, workspace_dependencies: dict) -> list[str]:
    """Check ordinary and target-specific runtime edges, including renamed packages."""
    manifest = tomllib.loads(text)
    package = manifest.get("package", {}).get("name")
    forbidden = UPPER_LAYERS.get(package, set())
    errors = []
    groups = [manifest.get("dependencies", {})]
    groups.extend(target.get("dependencies", {}) for target in manifest.get("target", {}).values())
    # dev-dependencies are fixture-only; Cargo keeps them outside production builds.
    for group in groups:
        for alias, spec in group.items():
            resolved = spec
            if isinstance(spec, dict) and spec.get("workspace"):
                resolved = workspace_dependencies.get(alias, spec)
            dependency = resolved.get("package", alias) if isinstance(resolved, dict) else alias
            if dependency in forbidden:
                errors.append(f"{relative}: forbidden runtime edge {package} -> {dependency} (alias {alias})")
    return errors


def scan(root: Path) -> list[str]:
    source_root = root / "crates/yield-ui/src"
    if not source_root.is_dir():
        return [f"{source_root}: missing yield-ui source tree"]
    errors = []
    workspace = tomllib.loads((root / "Cargo.toml").read_text(encoding="utf-8"))["workspace"]
    for member in workspace["members"]:
        manifest = root / member / "Cargo.toml"
        errors.extend(check_manifest(manifest.relative_to(root).as_posix(), manifest.read_text(encoding="utf-8"), workspace.get("dependencies", {})))
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
        "use yield_library::{SourceDatabase as Raw}; Raw::open_for_scan(x);",
        "use yield_library::file_operation_owner::FileOperationOwner as Effects;",
        "pub use yield_library::*;",
        "use yield_library::sample_sources::{library as raw}; raw::load();",
        "yield_library::sample_sources::library::load();",
        "use yield_scan as scanner; scanner::scan_in_background(x);",
        "use yield_analysis::{analysis as work};",
        "use yield_source_watcher::*;",
        "OperationJournalClient::new();",
        "OperationJournalStore::open();",
        "std::fs::remove_file(\"x\");",
        "use std::fs as disk; disk::rename(a, b);",
        "use std::fs::{self as disk}; disk::rename(a, b);",
        "use std::fs::{write as persist}; persist(a, b);",
        "use std::fs::write; write(a, b);",
        "use std::fs::{write, rename}; write(a, b); rename(a, b);",
        "std::fs::File::create(\"x\");",
        "std::fs::File::create_new(\"x\");",
        "std::fs::OpenOptions::new().write(true).create(true);",
        "std::fs::OpenOptions::new()\n    .write(true)\n    .create_new(true)\n    .open(\"x\");",
        "use std::fs::OpenOptions as Builder; let mut b = Builder::default(); b.write(flag);",
        "let mut b = OpenOptions::new(); b.read(true).write(flag).open(path);",
        "#[cfg(test)]\nmod tests { fn fixture() {} }\nfn production() { std::fs::remove_file(\"x\"); }",
    )
    for fixture in bad_cases:
        assert check_source("bad.rs", fixture), fixture
    assert not check_source("fixture.rs", "#[cfg(test)]\nmod tests { fn f() { fs::remove_file(\"x\"); } }\nfn later() {}")
    assert check_source("malformed.rs", "#[cfg(test)]\nmod tests { fn f() {} ")
    assert not check_source("data.rs", "use yield_library::sample_sources::{SourceId, Rating};")
    assert not check_source("normalize.rs", "use yield_library::sample_sources::{SourceId, db::normalize_relative_path};")
    assert check_source("raw.rs", "use yield_library::sample_sources::{db as raw};")
    assert check_source("raw.rs", "use yield_library::sample_sources::{library as raw, db::normalize_relative_path};")
    assert check_source("raw.rs", "use yield_library::sample_sources::db::{normalize_relative_path, SourceDatabase};")
    assert not check_source("reader.rs", "let mut opts = OpenOptions::new(); opts.read(true);")
    assert not check_source("fixture.rs", "#[cfg(test)]\nmod tests { use yield_catalog::*; SourceDatabase::open_for_test_fixture_source_write(x); }")
    # A fixture filename or explanatory comment is never an exemption.
    assert check_source("tests.rs", "// cfg(test) fixture\nSourceWriter::new_for_scan(db);")

    ui = '[package]\nname = "yield-ui"\n'
    assert not check_manifest("ui.toml", ui + '[dev-dependencies]\nyield-catalog = "1"\n', {})
    assert not check_manifest("ui.toml", ui + '[dependencies]\nyield-session = "1"\nyield-library = "1"\n', {})
    assert check_manifest("ui.toml", ui + '[dependencies]\nworker = { package = "yield-scan", version = "1" }\n', {})
    assert check_manifest("ui.toml", ui + '[target.\'cfg(windows)\'.dependencies]\nworker = { package = "yield-analysis", version = "1" }\n', {})
    assert check_manifest("ui.toml", ui + '[dependencies]\nhidden = { workspace = true }\n', {"hidden": {"package": "yield-source-watcher"}})
    assert check_manifest("lib.toml", '[package]\nname = "yield-library"\n[dependencies]\nhidden = { package = "yield-session", version = "1", optional = true }\n', {})

    # Exercise the real repository scan so source masking and Cargo edge checks
    # cannot accidentally exist only in check_source/check_manifest unit fixtures.
    with tempfile.TemporaryDirectory() as temporary:
        root = Path(temporary)
        source = root / "crates/yield-ui/src"
        source.mkdir(parents=True)
        (root / "Cargo.toml").write_text('[workspace]\nmembers = ["crates/yield-ui"]\n')
        manifest = source.parent / "Cargo.toml"
        manifest.write_text(ui + '[dependencies]\nyield-session = "1"\n')
        fixture = source / "tests.rs"
        fixture.write_text("#[cfg(test)]\nmod tests { SourceDatabase::open_for_test_fixture_source_write(x); }")
        assert not scan(root)
        fixture.write_text("// fixture\nFileOperationOwner::start(x);")
        assert scan(root)
        fixture.write_text("fn production() {}")
        manifest.write_text(ui + '[dependencies]\nhidden = { package = "yield-catalog", version = "1" }\n')
        assert scan(root)
        fixture.unlink()
        source.rmdir()
        assert scan(root)


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
