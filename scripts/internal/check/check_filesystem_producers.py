#!/usr/bin/env python3
"""Review finite filesystem/journal producer callsites in active workspace crates.

Lexical tripwire only: an inventory entry is not a capability or qualification.
No automatic inventory update is provided. New/removed calls require review.
"""
from __future__ import annotations

import argparse
from collections import Counter
import json
from pathlib import Path
import re
import sys
import tempfile
import tomllib

from check_gpui_owner_boundary import MUTATIONS, blank_non_code, production_text

ROOT = Path(__file__).resolve().parents[3]
INVENTORY = "docs/filesystem-producer-inventory.json"
FUNCTION = re.compile(r"\bfn\s+(\w+)\s*(?:<[^\n{]*>)?\s*\(")
NATIVE = {"rename", "renameat", "renameat2", "unlink", "unlinkat", "mkdir", "mkdirat",
          "link", "linkat", "symlink", "symlinkat", "ftruncate", "truncate"}
ISSUE_ID = re.compile(r"(?:OPT-[0-9]+|PULSE-[1-9][0-9]*)\Z")


def calls_in(path: str, text: str) -> tuple[Counter, list[str]]:
    code, errors = production_text(text)
    functions = list(FUNCTION.finditer(code))
    found: dict[int, str] = {}
    aliases = {"fs"}
    imported = {}
    files, options, journals = {"File"}, {"OpenOptions"}, {"OperationJournalStore", "OperationJournalOwner"}
    for match in re.finditer(r"\buse\s+(?:std|tokio|async_std|cap_primitives|cap_std)\s*::\s*fs\s+as\s+(\w+)", code):
        aliases.add(match[1])
    for match in re.finditer(r"\buse\s+(?:std|tokio|async_std|cap_primitives|cap_std)\s*::\s*fs\s*::\s*(\{[^}]*\}|\w+(?:\s+as\s+\w+)?)\s*;", code):
        for item in match[1].strip("{}").split(","):
            part = re.fullmatch(r"\s*(\w+)(?:\s+as\s+(\w+))?\s*", item)
            if not part:
                continue
            name, alias = part[1], part[2] or part[1]
            if name == "self":
                aliases.add(alias)
            elif name in MUTATIONS:
                imported[alias] = name
            elif name == "File":
                files.add(alias)
            elif name == "OpenOptions":
                options.add(alias)
    for match in re.finditer(r"\b(OperationJournalStore|OperationJournalOwner)\s+as\s+(\w+)", code):
        journals.add(match[2])
    for alias in aliases:
        for match in re.finditer(rf"\b{alias}\s*::\s*(\w+)\s*\(", code):
            if match[1] in MUTATIONS:
                found[match.start()] = "fs::" + match[1]
    for alias, name in imported.items():
        for match in re.finditer(rf"\b{alias}\s*\(", code):
            if not re.search(r"\bfn\s*$", code[max(0, match.start()-10):match.start()]):
                found[match.start()] = "fs::" + name
    for alias in files:
        for match in re.finditer(rf"\b{alias}\s*::\s*(create|create_new)\s*\(", code):
            found[match.start()] = "File::" + match[1]
    # Track constructor chains and named builders; do not mistake Vec::truncate
    # or unrelated write methods for writable OpenOptions.
    builders = set()
    for alias in options:
        for match in re.finditer(rf"\b{alias}\s*::\s*(?:new|default)\s*\(\)", code):
            prefix = code[max(0, code.rfind(";", 0, match.start()) + 1):match.start()]
            binding = re.search(r"\blet\s+(?:mut\s+)?(\w+)(?:\s*:[^=]+)?\s*=\s*$", prefix)
            if binding:
                builders.add(binding[1])
            end = code.find(";", match.end())
            tail = code[match.end():end if end >= 0 else len(code)]
            for flag in re.finditer(r"\.\s*(write|append|create|create_new|truncate)\s*\(", tail):
                found[match.end() + flag.start()] = "OpenOptions::" + flag[1]
    for builder in builders:
        for match in re.finditer(rf"\b{builder}\s*(\.[^;]+)", code):
            for flag in re.finditer(r"\.\s*(write|append|create|create_new|truncate)\s*\(", match[1]):
                found[match.start(1) + flag.start()] = "OpenOptions::" + flag[1]
    for match in re.finditer(r"\.\s*(rename|remove_file|remove_dir|remove_dir_all|create_dir|create_dir_all|hard_link|symlink|set_permissions|persist|persist_noclobber|tempfile|tempfile_in|file_dump)\s*\(", code):
        found[match.start()] = "method::" + match[1]
    for match in re.finditer(r"\b(NamedTempFile|WavWriter)\s*::\s*(new|new_in|create)\s*\(", code):
        found[match.start()] = match[1] + "::" + match[2]
    for alias in journals:
        for match in re.finditer(rf"\b{alias}\s*::\s*((?:open|start)\w*)\s*\(", code):
            found[match.start()] = "journal::" + match[1]
    for match in re.finditer(r"\blibc\s*::\s*(\w+)\s*\(", code):
        if match[1] in NATIVE:
            found[match.start()] = "libc::" + match[1]
    calls = Counter()
    for position, operation in found.items():
        before = [fn for fn in functions if fn.start() < position]
        function = before[-1][1] if before else "<module>"
        calls[(path, function, operation)] += 1
    return calls, [f"{path}: {error}" for error in errors]


def collect(root: Path) -> tuple[Counter, list[str]]:
    calls, errors = Counter(), []
    members = tomllib.loads((root / "Cargo.toml").read_text())["workspace"]["members"]
    for member in members:
        source = root / member / "src"
        if not source.is_dir():
            errors.append(f"missing workspace source: {member}")
        paths = sorted(source.rglob("*.rs"))
        module_scopes = {}
        for path in paths:
            text = path.read_text()
            code = blank_non_code(text)
            # A module declared in both test and production configurations is
            # production. Comments, strings, and a tests filename confer no exemption.
            for match in re.finditer(r"(?:#\[[^\]]*\]\s*)*(?:pub(?:\([^)]*\))?\s+)?mod\s+(\w+)\s*;", code):
                base = path.parent if path.name in {"lib.rs", "main.rs", "mod.rs"} else path.parent / path.stem
                original = text[match.start():match.end()]
                explicit = re.search(r'path\s*=\s*"([^"]+)"', original)
                targets = [path.parent / explicit[1]] if explicit else [base / (match[1] + ".rs"), base / match[1]]
                is_test = bool(re.search(r"#\[cfg\(test\)\]", match[0]))
                for target in targets:
                    module_scopes.setdefault(target, set()).add(is_test)
            production, _ = production_text(text)
            for match in re.finditer(r'include!\s*\(\s*"([^"]+)"\s*\)', text):
                # Literal include! in a masked inline test module is a fixture;
                # a production include of the same file cancels the exemption.
                if not code[match.start():].startswith("include!"):
                    continue
                target = path.parent / match[1]
                is_test = not production[match.start():match.start()+8].strip()
                module_scopes.setdefault(target, set()).add(is_test)
        test_roots = [path for path, scopes in module_scopes.items() if scopes == {True}]
        for path in paths:
            relative = path.relative_to(root).as_posix()
            if any(path == fixture or fixture in path.parents for fixture in test_roots):
                continue
            found, parse_errors = calls_in(relative, path.read_text())
            calls.update(found)
            errors.extend(parse_errors)
    return calls, errors


def validate(calls: Counter, rows: list[dict]) -> list[str]:
    expected, errors = Counter(), []
    for row in rows:
        key = (row["path"], row["function"], row["operation"])
        if key in expected or row["count"] <= 0:
            errors.append(f"duplicate/invalid reviewed entry: {key}")
        if row["classification"] not in {"owner", "maintenance", "compatibility", "fixture"} or not row["reason"] or not row["retirement"] or not ISSUE_ID.fullmatch(row["issue"]):
            errors.append(f"incomplete classification: {key}")
        expected[key] = row["count"]
    for key in sorted(calls.keys() | expected.keys()):
        if calls[key] != expected[key]:
            errors.append(f"{key}: observed {calls[key]}, reviewed {expected[key]}; explicit review required")
    return errors


def self_test() -> None:
    snippets = [
        'fn bypass() { std::fs::rename(a,b); }',
        'use std::fs as disk; fn bypass() { disk::remove_file(a); }',
        'use std::fs::{self as disk}; fn bypass() { disk::rename(a,b); }',
        'use std::fs::{write as persist}; fn bypass() { persist(a,b); }',
        'use std::fs::File as Sink; fn bypass() { Sink::create(a); }',
        'use std::fs::OpenOptions as Builder; fn bypass() { let mut b = Builder::new(); b.write(flag); }',
        'fn bypass() { OperationJournalStore::open(profile); }',
        'use x::OperationJournalOwner as Journal; fn bypass() { Journal::start(store); }',
        'fn bypass() { libc::unlinkat(fd,path,0); }',
        'fn bypass() { NamedTempFile::new_in(dir); }',
        'fn bypass() { temp.persist(path); }',
        'use std::fs::OpenOptions; fn bypass() { let mut b = OpenOptions::new(); b.read(true).write(true); }',
    ]
    for text in snippets:
        calls, errors = calls_in("crates/yield-ui/src/new.rs", text)
        assert calls and not errors and validate(calls, [])
    assert not calls_in("read.rs", 'fn reads() { fs::metadata(a); File::open(a); } // fs::write(a,b);')[0]
    assert not calls_in("vector.rs", 'fn reads() { let o = OpenOptions::new(); o.read(true); vec.truncate(2); }')[0]
    fixture = '#[cfg(test)]\nmod tests { fn f() { fs::write(a,b); } }\nfn later() { fs::write(a,b); }'
    calls, errors = calls_in("fixture.rs", fixture)
    assert not errors and list(calls) == [("fixture.rs", "later", "fs::write")]
    assert calls_in("bad.rs", '#[cfg(test)]\nmod tests {')[1]
    row = dict(path="fixture.rs", function="later", operation="fs::write", count=1,
               classification="maintenance", reason="isolated disposable cache", retirement="remove with cache owner", issue="OPT-1636")
    assert not validate(calls, [row])
    assert not validate(calls, [{**row, "issue": "PULSE-79"}])
    for invalid_issue in ("PULSE-0", "PULSE-079", "PULSE-", "PULSE-7x", "OPT-", "OPT-7x"):
        assert validate(calls, [{**row, "issue": invalid_issue}])
    assert validate(calls + calls, [row])
    assert validate(Counter(), [row])
    assert validate(calls, [{**row, "retirement": ""}])
    assert validate(calls, [{**row, "path": "other.rs"}])


    # Real scanner fixtures: a name/comment alone never establishes test-only scope.
    with tempfile.TemporaryDirectory() as temporary:
        root = Path(temporary)
        (root / "Cargo.toml").write_text('[workspace]\nmembers = ["crates/example"]\n')
        source = root / "crates/example/src"
        source.mkdir(parents=True)
        (source / "tests.rs").write_text('fn bypass() { fs::write(a,b); }')
        lib = source / "lib.rs"
        lib.write_text('mod tests;')
        assert collect(root)[0]
        lib.write_text('#[cfg(test)]\nmod tests;')
        assert not collect(root)[0]
        lib.write_text('#[cfg(test)]\nmod tests;\n#[cfg(not(test))]\nmod tests;')
        assert collect(root)[0]
        lib.write_text('// #[cfg(test)]\n// mod tests;')
        assert collect(root)[0]
        lib.write_text('#[cfg(test)]\nmod fixtures { include!("tests.rs"); }')
        assert not collect(root)[0]
        lib.write_text('#[cfg(test)]\nmod fixtures { include!("tests.rs"); }\ninclude!("tests.rs");')
        assert collect(root)[0]


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--root", type=Path, default=ROOT)
    parser.add_argument("--inventory", action="store_true", help="print candidates only; never approves them")
    parser.add_argument("--self-test", action="store_true")
    args = parser.parse_args()
    if args.self_test:
        self_test()
        print("[filesystem_producers] self-test OK")
        return 0
    calls, errors = collect(args.root)
    if args.inventory:
        print(json.dumps([dict(path=p, function=f, operation=o, count=n) for (p,f,o),n in sorted(calls.items())], indent=2))
    else:
        errors.extend(validate(calls, json.loads((args.root / INVENTORY).read_text())["rows"]))
    for error in errors:
        print(f"[filesystem_producers] {error}", file=sys.stderr)
    if not errors and not args.inventory:
        print(f"[filesystem_producers] {sum(calls.values())} reviewed calls OK")
    return bool(errors)


if __name__ == "__main__":
    raise SystemExit(main())
