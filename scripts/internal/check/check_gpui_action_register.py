#!/usr/bin/env python3
"""Validate and render the reviewed OPT-1867 source register (not native parity)."""

from __future__ import annotations

import argparse
import copy
import json
import re
from collections import Counter
from pathlib import Path

from check_gpui_owner_boundary import DEFAULT_ROOT, blank_non_code, production_text
from index_gpui_reference_controls import index

REGISTER = Path("docs/gpui-reference-action-register.json")
REPORT = Path("docs/GPUI_REFERENCE_ACTION_REGISTER.md")


def anchor(root: Path, entry: dict, test: bool = False) -> str:
    path = root / entry["path"]
    if not path.resolve().is_relative_to(root.resolve()) or path.suffix != ".rs":
        raise ValueError("anchor must be a Rust file within the checkout")
    text = path.read_text()
    if test:
        text = blank_non_code(text)
        # Require an actual annotated test, not a helper with a plausible name.
        pattern = r"#\[(?:test|gpui::test)(?:\([^\]]*\))?\]\s*(?:async\s+)?fn\s+"
    else:
        text, errors = production_text(text)
        if errors:
            raise ValueError(f"{path}: {errors}")
        pattern = r"\bfn\s+"
    match = re.search(pattern + re.escape(entry["symbol"]) + r"\b", text)
    if not match:
        raise ValueError(f"missing {'annotated test' if test else 'production function'}: {entry}")
    line = text.count("\n", 0, match.start()) + 1
    return f"[{entry['symbol']}](../{entry['path']}#L{line})"


def validate(root: Path, data: dict, source: dict) -> None:
    if data["schema_version"] != 1 or data["issue"] != "OPT-1867":
        raise ValueError("unexpected register schema/issue")
    names = [row["message"] for row in data["rows"]]
    expected = {row["message"] for row in source["reference_messages"]}
    if len(names) != len(set(names)) or set(names) != expected:
        raise ValueError("duplicate, unreviewed or retired reference messages")
    for row in data["rows"]:
        if row["state"] not in ("partial", "gap", "internal"):
            raise ValueError(f"invalid state: {row['message']}")
        if not re.fullmatch(r"OPT-\d+", row["owner"]) or not row["note"].strip():
            raise ValueError(f"missing concrete owner/scope: {row['message']}")
        if row["state"] == "partial":
            anchor(root, row["handler"])
        elif row["handler"] is not None or row["automated_evidence"]:
            raise ValueError("gap/internal rows cannot claim a handler or test")
        for test in row["automated_evidence"]:
            anchor(root, test, test=True)
        expected_native = "not a standalone action" if row["state"] == "internal" else "unverified — OPT-2009"
        if row["native_evidence"] != expected_native:
            raise ValueError("native acceptance must remain explicitly unverified")


def render(root: Path, data: dict, source: dict) -> str:
    validate(root, data, source)
    counts = Counter(row["state"] for row in data["rows"])
    lines = [
        "# Reviewed reference action register (OPT-1867)", "",
        f"Source review baseline: `{data['baseline']}`. {len(data['rows'])} distinct source references: "
        + ", ".join(f"{counts[state]} {state}" for state in ("partial", "gap", "internal")) + ".", "",
        "The finite inventory covers archived native chrome, native keyboard routes and waveform interactions. "
        "It includes match/forwarding references; it does not prove runtime reachability. External and inline "
        "test modules are excluded. The broad legacy controller action catalog is outside this scope.", "",
        "**No row certifies complete parity.** Partial rows identify a production control or supporting model "
        "and state the exact limits of any fixture. A dash in the test column means no direct test is claimed. "
        "Gap rows name the open implementation owner; internal rows are forwarding/viewport protocols. "
        "Every standalone row still requires native evidence in "
        "[OPT-2009](https://linear.app/boostnlvp/issue/OPT-2009), including visibility, focus, platform cfgs, "
        "aliases and macro expansion. Source review completion does not close release acceptance OPT-1863.", "",
        "Regenerate/validate with `python3 scripts/internal/check/check_gpui_action_register.py --write` "
        "and `--check`. The JSON preserves reviewed input hashes; the live lexical index records current "
        "hashes separately. Validation requires exact message coverage and existing production/test anchors. "
        "It cannot judge semantic equivalence or whether native tests have passed.", "",
        "The owner column links implementation families; it does not claim ownership has already been wired "
        "for a missing physical command. Publication/cleanup remain unsupported under OPT-1868. "
        "Prepared DSP, clipboard pathname text and vocabulary-only updates do not complete those effects.", "",
        "| Reference / first source witness | State | GPUI source anchor | Scoped automated fixture | Owner / gap | Review limits |", 
        "| --- | --- | --- | --- | --- | --- |",
    ]
    refs = {row["message"]: row["witnesses"][0] for row in source["reference_messages"]}
    for row in data["rows"]:
        witness = refs[row["message"]]
        reference = f"[{row['message']}](../{witness['path']}#L{witness['line']})"
        handler = anchor(root, row["handler"]) if row["handler"] else "—"
        tests = "<br>".join(anchor(root, item, test=True) for item in row["automated_evidence"]) or "—"
        owner = f"[{row['owner']}](https://linear.app/boostnlvp/issue/{row['owner']})"
        note = row["note"].replace("|", "\\|").replace("\n", " ")
        lines.append(f"| {reference} | {row['state']} | {handler} | {tests} | {owner} | {note} |")
    lines.extend(["", "GPUI additions outside the archived reference inventory:", "",
        "| Surface | Source / scope | Native follow-up |", "| --- | --- | --- |",
        "| Built-in visual effects and reviewed custom WGSL | `browser/general_settings.rs`; settings/revision fencing and shader validation exist; renderer acceptance remains open | OPT-1974 |",
        "| Text scaling, theme and motion settings | `browser/general_settings.rs`, `browser/mod.rs`; profile-backed settings; native layout/contrast acceptance remains open | OPT-1959, OPT-1937 |",
        "| Feedback issue composer | `browser/feedback_issue.rs`; bounded issue URL generation and user-triggered external open | OPT-1882 |",
        "| Volume pointer capture | `browser/view.rs`, `browser/transport.rs`; source counterpart listed above; window/OS capture unverified | OPT-1936 |", "",
        "See [source index scope](GPUI_REFERENCE_CONTROL_INDEX.md), "
        "[destructive action decision](GPUI_REFERENCE_EDIT_ACTIONS.md), and "
        "[migration evidence](GPUI_MIGRATION.md).", ""])
    return "\n".join(lines)


def self_test(root: Path, data: dict, source: dict) -> None:
    validate(root, data, source)
    cases = []
    duplicate = copy.deepcopy(data)
    duplicate["rows"].append(duplicate["rows"][0])
    cases.append(duplicate)
    missing = copy.deepcopy(data)
    missing["rows"].pop()
    cases.append(missing)
    helper = copy.deepcopy(data)
    partial = next(row for row in helper["rows"] if row["handler"])
    partial["automated_evidence"] = [partial["handler"]]
    cases.append(helper)
    stale = copy.deepcopy(data)
    next(row for row in stale["rows"] if row["handler"])["handler"]["symbol"] = "missing_function"
    cases.append(stale)
    for case in cases:
        try:
            validate(root, case, source)
        except ValueError:
            continue
        raise AssertionError("invalid register must fail closed")


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--root", type=Path, default=DEFAULT_ROOT)
    mode = parser.add_mutually_exclusive_group(required=True)
    mode.add_argument("--check", action="store_true")
    mode.add_argument("--write", action="store_true")
    mode.add_argument("--self-test", action="store_true")
    args = parser.parse_args()
    root = args.root.resolve()
    data = json.loads((root / REGISTER).read_text())
    source = index(root)
    if args.self_test:
        self_test(root, data, source)
    else:
        report = render(root, data, source)
        if args.write:
            (root / REPORT).write_text(report)
        elif (root / REPORT).read_text() != report:
            raise SystemExit("action register report is stale; run --write and review changes")
    print(f"[gpui_action_register] {len(data['rows'])} reviewed references OK")


if __name__ == "__main__":
    main()
