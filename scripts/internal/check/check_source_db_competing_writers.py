#!/usr/bin/env python3
"""Reject new production SQLite/source opens outside reviewed owner boundaries.

This is a lexical architecture tripwire, not an authority grant. Runtime profile and
source lease checks remain decisive. Test modules and fixture paths are excluded;
their role-specific exceptions are exercised by profile_admission integration tests.
"""

from __future__ import annotations

import re
import sys
from collections import Counter
from dataclasses import dataclass
from pathlib import Path


ROOT = Path(__file__).resolve().parents[3]
TEST_PARTS = {"tests", "fixtures", "source_db_mod_tests", "source_db_migration_tests"}
TEST_MODULE = re.compile(r"(?m)^[ \t]*#\[cfg\(test\)\][ \t]*\n[ \t]*mod[ \t]+\w+[ \t]*\{")
TOP_LEVEL_CLOSE = re.compile(r"(?m)^}[ \t]*$")
SOURCE_OPEN = re.compile(r"\bSourceDatabase\s*::\s*(open_\w+|open_with_role\w*)\s*\(")
RAW_OPEN = re.compile(
    r"\b(?:rusqlite\s*::\s*)?Connection\s*::\s*"
    r"(open|open_with_flags|open_in_memory)\s*\("
)
FUNCTION = re.compile(r"\bfn\s+(\w+)\s*(?:<[^\n{]*>)?\s*\(")

# (repository path, enclosing function, opener): maximum reviewed callsites.
# The three path-only source calls remain production compatibility APIs whose
# normal-build role gate returns ProfileOwnershipRequired before filesystem I/O.
SOURCE_EXCEPTIONS = {
    ("crates/yield-catalog/src/lib.rs", "register_source", "open_for_catalog_with_profile_guard"): 2,
    ("crates/yield-library/src/sample_sources/db/file_ops_journal/recovery_io.rs", "open", "open_for_source_write_with_database_root"): 1,
    ("crates/yield-library/src/sample_sources/mod.rs", "open_db", "open_for_source_write_with_database_root"): 1,
    ("crates/yield-scan/src/sample_sources/scanner/scan/runner.rs", "scan_in_background", "open_for_scan"): 1,
}

# Source owner, lease control, snapshot destination, global library, and journal
# opens are separate reviewed boundaries. New raw opens need an explicit review.
RAW_EXCEPTIONS = {
    ("crates/yield-library/src/operation_journal/store.rs", "open_inner_with_hooks", "open_with_flags"): 1,
    ("crates/yield-library/src/sample_sources/db/mod.rs", "snapshot_to_path_with_held_write_fence", "open"): 1,
    ("crates/yield-library/src/sample_sources/db/open.rs", "open_source_database_with_profile_guard_inner", "open_with_flags"): 1,
    ("crates/yield-library/src/sample_sources/db/open.rs", "open_source_database_with_flags", "open_with_flags"): 1,
    ("crates/yield-library/src/sample_sources/db/open/read_only.rs", "open_read_only_source_database", "open_with_flags"): 1,
    ("crates/yield-library/src/sample_sources/db/schema/mod.rs", "current_table_shape", "open_in_memory"): 1,
    ("crates/yield-library/src/sample_sources/db/source_lease.rs", "bootstrap", "open_with_flags"): 1,
    ("crates/yield-library/src/sample_sources/library/connection.rs", "open_for_profile_guard", "open_with_flags"): 1,
    ("crates/yield-library/src/sample_sources/library/connection.rs", "open_at", "open"): 1,
    ("crates/yield-library/src/sample_sources/library/read_only.rs", "observe_source_registry", "open_with_flags"): 1,
}

READ_ONLY_SOURCE_OPENS = {
    "open_for_ui_read",
    "open_for_ui_read_with_database_root",
    "open_for_background_read",
    "open_for_background_read_with_database_root",
}


@dataclass(frozen=True)
class Call:
    path: str
    line: int
    function: str
    opener: str
    kind: str

    def key(self) -> tuple[str, str, str]:
        return (self.path, self.function, self.opener)


def production_text(path: str, text: str) -> tuple[str, list[str]]:
    if any(part in TEST_PARTS for part in Path(path).parts) or path.endswith(
        ("_tests.rs", "_test.rs", "/tests.rs")
    ):
        return "", []
    errors = []
    # Rustfmt puts the closing brace of a top-level module at column zero.
    # Mask only the module, preserving offsets and scanning later production items.
    for marker in reversed(list(TEST_MODULE.finditer(text))):
        closing = TOP_LEVEL_CLOSE.search(text, marker.end())
        if closing is None:
            errors.append(f"{path}: cfg(test) module has no top-level closing brace")
            continue
        end = closing.end()
        masked = "".join("\n" if char == "\n" else " " for char in text[marker.start() : end])
        text = text[: marker.start()] + masked + text[end:]
    return text, errors


def calls_in(path: str, text: str) -> tuple[list[Call], list[str]]:
    text, errors = production_text(path, text)
    calls = []
    for kind, pattern in (("source", SOURCE_OPEN), ("raw", RAW_OPEN)):
        for match in pattern.finditer(text):
            prefix = text[: match.start()]
            functions = list(FUNCTION.finditer(prefix))
            function = functions[-1].group(1) if functions else "<module>"
            calls.append(
                Call(path, prefix.count("\n") + 1, function, match.group(1), kind)
            )
    return calls, errors


def validate(calls: list[Call]) -> list[str]:
    counts: Counter[tuple[str, str, str, str]] = Counter()
    errors = []
    for call in calls:
        if call.kind == "source" and call.opener in READ_ONLY_SOURCE_OPENS:
            continue
        allowed = SOURCE_EXCEPTIONS if call.kind == "source" else RAW_EXCEPTIONS
        key = (call.kind, *call.key())
        counts[key] += 1
        if counts[key] > allowed.get(call.key(), 0):
            errors.append(
                f"{call.path}:{call.line}: unreviewed {call.kind} open "
                f"{call.opener} in {call.function}"
            )
    return errors


def self_test() -> None:
    catalog = "crates/yield-catalog/src/lib.rs"
    source = "crates/yield-scan/src/sample_sources/scanner/scan/runner.rs"
    accepted, _ = calls_in(
        catalog,
        "fn register_source() { SourceDatabase::open_for_catalog_with_profile_guard(); }",
    )
    assert not validate(accepted)
    rejected, _ = calls_in(source, "fn new_writer() { SourceDatabase::open_for_scan(); }")
    assert validate(rejected)
    raw, _ = calls_in(source, "fn new_writer() { Connection::open_with_flags(); }")
    assert validate(raw)
    fixture, _ = calls_in(
        source,
        "fn production() {}\n#[cfg(test)]\nmod tests {\n"
        "    fn fixture() { SourceDatabase::open_for_scan(); }\n}\n"
        "fn later_production() { SourceDatabase::open_for_scan(); }\n",
    )
    assert len(fixture) == 1 and fixture[0].function == "later_production"
    assert validate(fixture)


def inventory_label(call: Call) -> str:
    if call.kind == "source":
        if call.opener in READ_ONLY_SOURCE_OPENS:
            return "legacy-read-only"
        if call.path.endswith("/sample_sources/mod.rs") or call.path.endswith(
            "/file_ops_journal/recovery_io.rs"
        ) or call.path.endswith("/scanner/scan/runner.rs"):
            return "path-only-compatibility"
        if call.path.endswith("/yield-catalog/src/lib.rs") and call.function == "register_source":
            return "source-owner"
        return "unclassified"
    if call.path.endswith("/operation_journal/store.rs"):
        return "operation-journal"
    if call.path.endswith("/library/connection.rs"):
        return "global-library"
    if call.path.endswith("/open/read_only.rs") or call.path.endswith(
        "/library/read_only.rs"
    ):
        return "legacy-read-only"
    if call.path.endswith("/open.rs"):
        if call.function == "open_source_database_with_flags":
            return "path-only-compatibility"
        return "source-owner"
    if call.path.endswith("/source_lease.rs"):
        return "source-lease-control"
    if call.path.endswith("/schema/mod.rs"):
        return "in-memory-schema-probe"
    if call.path.endswith("/mod.rs") and call.function == "snapshot_to_path_with_held_write_fence":
        return "snapshot-destination"
    return "unclassified"


def collect_calls() -> tuple[list[Call], list[str]]:
    calls = []
    errors = []
    for directory in ("crates", "src", "tools", "benches", "tests"):
        source_root = ROOT / directory
        if not source_root.is_dir():
            continue
        for path in sorted(source_root.rglob("*.rs")):
            relative = path.relative_to(ROOT).as_posix()
            found, diagnostics = calls_in(relative, path.read_text(encoding="utf-8"))
            calls.extend(found)
            errors.extend(diagnostics)
    return calls, errors


def main() -> int:
    if len(sys.argv) == 2 and sys.argv[1] == "--self-test":
        self_test()
        print("[source_db_competing_writers] self-test OK")
        return 0
    if len(sys.argv) > 2 or (len(sys.argv) == 2 and sys.argv[1] != "--inventory"):
        print("usage: check_source_db_competing_writers.py [--self-test|--inventory]", file=sys.stderr)
        return 2
    calls, errors = collect_calls()
    errors.extend(validate(calls))
    errors.extend(
        f"{call.path}:{call.line}: direct open has no inventory boundary"
        for call in calls
        if inventory_label(call) == "unclassified"
    )
    if errors:
        for error in errors:
            print(f"[source_db_competing_writers] {error}", file=sys.stderr)
        return 1
    if len(sys.argv) == 2:
        for call in calls:
            print(f"{inventory_label(call)}\t{call.path}:{call.line}\t{call.function}\t{call.opener}")
    print(f"[source_db_competing_writers] OK ({len(calls)} reviewed production opens)")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
