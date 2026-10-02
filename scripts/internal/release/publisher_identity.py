#!/usr/bin/env python3
"""Bind publisher snapshots and server echoes to actual packaged identity."""
from __future__ import annotations

import argparse
import datetime as dt
import json
import os
from pathlib import Path
import re
import shutil
import zipfile

import verify_published_release as verifier

MAX_JSON = 512 * 1024
MAX_U64 = (1 << 64) - 1
ROOT = Path(__file__).resolve().parents[3]


def require(condition, message):
    if not condition:
        raise ValueError(message)


def bounded_json(raw):
    require(len(raw) <= MAX_JSON, "server metadata exceeds 512 KiB")
    def reject_constant(value):
        raise ValueError(f"nonfinite JSON number {value}")
    value = json.loads(raw, object_pairs_hook=verifier.reject_duplicate_manifest_fields,
                       parse_constant=reject_constant)
    def visit(item, depth=0):
        require(depth <= 16, "metadata nesting exceeds 16")
        if isinstance(item, str):
            require(len(item.encode("utf-8")) <= 16384, "metadata string exceeds 16 KiB")
        elif isinstance(item, dict):
            for key, child in item.items():
                visit(key, depth + 1)
                visit(child, depth + 1)
        elif isinstance(item, list):
            for child in item:
                visit(child, depth + 1)
    visit(value)
    return value


def read_json(path):
    with Path(path).open("rb") as source:
        return bounded_json(source.read(MAX_JSON + 1))


def positive_u64(value, label):
    require(type(value) is int and 0 < value <= MAX_U64, f"{label} must be a positive u64")
    return value


def timestamp(value):
    require(isinstance(value, str) and len(value) <= 64, "timestamp must be bounded text")
    parsed = dt.datetime.fromisoformat(value.replace("Z", "+00:00"))
    require(parsed.tzinfo is not None, "timestamp requires timezone")
    return parsed.astimezone(dt.timezone.utc)


def commit(value):
    require(isinstance(value, str) and re.fullmatch(r"[0-9a-fA-F]{40}", value)
            and int(value, 16) != 0, "packaged commit must be nonzero full 40-hex")
    return value


def version_context(version, channel, full_commit):
    require(isinstance(version, str) and len(version) <= 128, "invalid release version")
    core = r"(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)"
    suffix = {"stable": "", "rc": r"-rc\.([1-9][0-9]*)",
              "nightly": r"-nightly\.(\d{4}-\d{2}-\d{2})\+([0-9a-fA-F]{8})"}[channel]
    match = re.fullmatch(core + suffix, version)
    require(match is not None, "release version is not canonical for channel")
    require(all(int(part) <= (1 << 32) - 1 for part in match.groups()[:3]), "version core overflows u32")
    target = ".".join(match.groups()[:3])
    if channel == "rc":
        require(int(match.group(4)) <= (1 << 32) - 1, "RC ordinal overflows u32")
    if channel == "nightly":
        dt.date.fromisoformat(match.group(4))
        require(match.group(5).lower() == full_commit[:8].lower(), "nightly prefix disagrees with full commit")
    return target


def validate_metadata(actual, expected):
    require(isinstance(actual, dict), "release metadata must be an object")
    for key in ("build_id", "channel", "version", "commit"):
        require(actual.get(key) == expected[key], f"release {key} mismatch or missing")
    commit(actual["commit"])
    positive_u64(actual.get("build_number"), "release build_number")
    require(actual["build_number"] == expected["build_number"], "release build_number mismatch")
    require(timestamp(actual.get("released_at")) == timestamp(expected["released_at"]), "release timestamp mismatch")


def validate_files(actual, expected):
    require(isinstance(actual, list) and len(actual) <= 16, "files must be a bounded list")
    require(len(actual) == len(expected), "release file inventory mismatch")
    expected_by_name = {item["name"]: item for item in expected}
    seen = set()
    for item in actual:
        require(isinstance(item, dict), "file must be an object")
        name = item.get("name")
        require(isinstance(name, str) and name in expected_by_name and name not in seen, "unexpected or duplicate file")
        seen.add(name)
        positive_u64(item.get("size_bytes"), "file size")
        require(item.get("sha256") == expected_by_name[name]["sha256"]
                and item["size_bytes"] == expected_by_name[name]["size_bytes"], "file hash/size mismatch")


def prepare(args):
    positive_u64(args.build_number, "build number")
    timestamp(args.released_at)
    require(args.channel in ("stable", "rc", "nightly"), "invalid channel")
    source = args.artifact_dir
    selected = []
    with os.scandir(source) as entries:
        for count, entry in enumerate(entries, 1):
            require(count <= 1024, "artifact directory exceeds 1024 entries")
            if entry.name.endswith(".zip") or (entry.name.startswith("checksums-")
                    and entry.name.endswith((".txt", ".txt.sig"))):
                require(len(selected) < 16, "excessive release artifacts")
                selected.append(source / entry.name)
    selected.sort()
    require(bool(selected), "missing release artifacts")
    args.snapshot_dir.mkdir()
    for path in selected:
        require(path.is_file() and not path.is_symlink() and path.stat().st_size > 0, "artifact must be a nonempty regular file")
        shutil.copyfile(path, args.snapshot_dir / path.name)
    archives = sorted(args.snapshot_dir.glob("*.zip"))
    require(bool(archives), "no packaged archives")
    with zipfile.ZipFile(archives[0]) as archive:
        names = [info for info in archive.infolist() if info.filename.endswith("update-manifest.json")]
        require(len(names) == 1 and names[0].file_size <= verifier.MAX_UPDATE_MANIFEST_BYTES, "ambiguous or oversized manifest")
        raw = archive.read(names[0])
        manifest = json.loads(raw, object_pairs_hook=verifier.reject_duplicate_manifest_fields)
    require(isinstance(manifest, dict), "manifest must be an object")
    full_commit = commit(manifest.get("commit"))
    target = version_context(args.version, args.channel, full_commit)
    build_date = manifest.get("build_date")
    require(isinstance(build_date, str) and re.fullmatch(r"\d{4}-\d{2}-\d{2}", build_date), "invalid packaged build date")
    dt.date.fromisoformat(build_date)
    contract = verifier.load_release_contract(ROOT / "release_contract.toml")
    assets = verifier.expected_release_assets(contract, args.channel, args.version, target)
    expected_names = {item.name for item in assets.zips} | {assets.checksum}
    actual_names = {p.name for p in selected}
    require(actual_names in (expected_names, expected_names | {assets.signature}), "artifact inventory differs from release contract")
    context = argparse.Namespace(channel=args.channel, version=args.version, target_version=target,
                                 commit=full_commit, build_date=build_date)
    errors = []
    for asset in assets.zips:
        errors.extend(verifier.validate_zip_manifest(args.snapshot_dir / asset.name, asset,
                      verifier.archive_layout_from_contract(contract), context))
    checksum_path = args.snapshot_dir / assets.checksum
    require(checksum_path.stat().st_size <= MAX_JSON, "checksum file is oversized")
    errors.extend(verifier.validate_checksum_file(checksum_path, args.snapshot_dir, assets.zips))
    require(not errors, "; ".join(errors))
    expected_id = f"yield-{args.version}"
    nightly_id = re.fullmatch(r"yield-nightly-b([1-9][0-9]*)-([0-9a-fA-F]{8}|[0-9a-fA-F]{40})", args.build_id)
    require(args.build_id == expected_id or (args.channel == "nightly" and nightly_id
            and int(nightly_id.group(1)) == args.build_number
            and full_commit.startswith(nightly_id.group(2))), "build id differs from packaged identity")
    files = []
    for path in sorted(args.snapshot_dir.iterdir()):
        files.append({"name": path.name, "sha256": verifier.sha256(path),
                      "size_bytes": positive_u64(path.stat().st_size, "artifact size")})
        path.chmod(0o400)
    identity = {"build_id": args.build_id, "build_number": args.build_number,
                "channel": args.channel, "version": args.version, "commit": full_commit,
                "released_at": args.released_at, "files": files}
    args.identity_file.write_text(json.dumps(identity), encoding="utf-8")
    print(full_commit)


def verify_response(args):
    expected = read_json(args.identity_file)
    response = read_json(args.response_file)
    require(isinstance(response, dict), "response must be an object")
    if args.phase == "staging":
        require(response.get("staged") is True, "upload not staged")
        validate_metadata(response, expected)
        selected = [item for item in expected["files"] if item["name"] == args.file_name]
        require(len(selected) == 1, "unknown staged file")
        validate_files([response.get("file")], selected)
    elif args.phase == "commit":
        require(response.get("committed") is True, "release not committed")
        release = response.get("release")
        validate_metadata(release, expected)
        validate_files(release.get("files"), expected["files"])
    else:
        releases = response.get("releases")
        require(isinstance(releases, list) and len(releases) <= 512, "invalid catalog releases")
        require(all(isinstance(item, dict) for item in releases), "invalid catalog release")
        matches = [item for item in releases if item.get("build_id") == expected["build_id"]]
        require(len(matches) == 1, "catalog release missing or duplicated")
        validate_metadata(matches[0], expected)
        validate_files(matches[0].get("files"), expected["files"])


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    sub = parser.add_subparsers(dest="command", required=True)
    p = sub.add_parser("prepare")
    for name in ("artifact-dir", "snapshot-dir", "identity-file"):
        p.add_argument(f"--{name}", type=Path, required=True)
    p.add_argument("--build-id", required=True)
    p.add_argument("--build-number", type=int, required=True)
    p.add_argument("--channel", required=True)
    p.add_argument("--version", required=True)
    p.add_argument("--released-at", required=True)
    p = sub.add_parser("verify")
    p.add_argument("--identity-file", type=Path, required=True)
    p.add_argument("--response-file", type=Path, required=True)
    p.add_argument("--phase", choices=("staging", "commit", "catalog"), required=True)
    p.add_argument("--file-name")
    args = parser.parse_args()
    try:
        prepare(args) if args.command == "prepare" else verify_response(args)
    except (ValueError, OSError, KeyError, TypeError, RecursionError, zipfile.BadZipFile) as error:
        parser.exit(1, f"Publisher identity rejected: {error}\n")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
