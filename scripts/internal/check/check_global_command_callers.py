#!/usr/bin/env python3
"""Finite lexical tripwire for unframed global mutation calls outside their owner.

The library keeps compatibility methods for fixtures. Session/catalog/background producers
must use captured requests. UI uses its typed session facade and is separately checked by
check_gpui_owner_boundary.py. This check does not prove runtime authority or replay safety.
"""
from __future__ import annotations

import argparse
from pathlib import Path
import re
import sys
import tomllib

from check_gpui_owner_boundary import production_text

ROOT = Path(__file__).resolve().parents[3]
LEGACY_METHODS = (
    "replace_source_registry", "replace_ui_preferences_payload",
    "set_profile_tag_category", "delete_profile_tag_vocabulary",
)
CALL = re.compile(r"(?:\.\s*|\b[A-Z]\w*\s*::\s*)(?:r#)?(" + "|".join(LEGACY_METHODS) + r")\s*\(")


def check_source(path: str, text: str) -> list[str]:
    code, errors = production_text(text)
    for match in CALL.finditer(code):
        line = code.count("\n", 0, match.start()) + 1
        errors.append(f"{path}:{line}: unframed global mutation {match[1]}; use captured request/context")
    return errors


def scan(root: Path) -> list[str]:
    errors = []
    workspace = tomllib.loads((root / "Cargo.toml").read_text())["workspace"]
    for member in workspace["members"]:
        package = tomllib.loads((root / member / "Cargo.toml").read_text())["package"]["name"]
        # The library owns the SQL/legacy fixtures. UI has acquisition-taking facades, not raw
        # library writers; its direct raw-owner access is rejected by the existing UI guard.
        if package in {"yield-library", "yield-ui"}:
            continue
        for path in sorted((root / member / "src").rglob("*.rs")):
            errors.extend(check_source(path.relative_to(root).as_posix(), path.read_text()))
    return errors


def self_test() -> None:
    for method in LEGACY_METHODS:
        assert check_source("producer.rs", f"alias .\n {method}(payload);")
        assert check_source("producer.rs", f"Alias::{method}(&client, payload);")
        assert check_source("producer.rs", f"alias.r#{method}(payload);")
        assert not check_source("producer.rs", f'// alias.{method}(x)\nconst S: &str = "alias.{method}(x)";')
        assert not check_source("producer.rs", f"#[cfg(test)]\nmod fixtures {{ fn f() {{ alias.{method}(x); }} }}")
        assert check_source("tests.rs", f"alias.{method}(payload);")
    assert not check_source("producer.rs", "fn set_profile_tag_category(x: X) {} client.submit_captured_mutation(request);")
    assert check_source("producer.rs", "#[cfg(test)]\nmod tests { fn f() {}")
    print("[global_command_callers] self-test OK")


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--root", type=Path, default=ROOT)
    parser.add_argument("--self-test", action="store_true")
    args = parser.parse_args()
    if args.self_test:
        self_test()
        return 0
    errors = scan(args.root)
    if errors:
        print("\n".join(errors), file=sys.stderr)
        return 1
    print("[global_command_callers] captured production callers OK")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
