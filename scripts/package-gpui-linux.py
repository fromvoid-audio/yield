#!/usr/bin/env python3
"""Build and verify one private x86_64 Linux Yield archive."""

import argparse
import hashlib
import json
import os
from pathlib import Path
import platform
import re
import shutil
import stat
import subprocess
import tempfile
import zipfile


TARGET = "x86_64-unknown-linux-gnu"
PROFILE = "release-local"
ARCHIVE_NAME = "yield-linux-x86_64.zip"
MANIFEST_NAME = "package-manifest.json"
FILES = {
    "README.md": ("docs/LINUX_TEST_BUILD.md", 0o644),
    "assets/yield-brand-icon.png": ("assets/yield-brand-icon.png", 0o644),
    "assets/IoskeleyMono-LICENSE.txt": ("assets/fonts/IoskeleyMono-LICENSE.txt", 0o644),
    "scripts/doctor.sh": ("scripts/doctor.sh", 0o755),
    "scripts/run.sh": ("scripts/run.sh", 0o755),
    "scripts/internal/run/bug_bundle.sh": ("scripts/internal/run/bug_bundle.sh", 0o755),
    "scripts/internal/run/latest_log.sh": ("scripts/internal/run/latest_log.sh", 0o755),
    "scripts/internal/run/linux_diagnostics_paths.sh": (
        "scripts/internal/run/linux_diagnostics_paths.sh",
        0o644,
    ),
}


def command(*args: str, cwd: Path) -> str:
    return subprocess.check_output(args, cwd=cwd, text=True).strip()


def sha256(path: Path) -> str:
    with path.open("rb") as source:
        return hashlib.file_digest(source, "sha256").hexdigest()


def require_x86_64_elf(binary: Path) -> None:
    if binary.is_symlink() or not binary.is_file() or not os.access(binary, os.X_OK):
        raise ValueError("expected an executable regular Yield binary")
    with binary.open("rb") as source:
        header = source.read(64)
    if (
        len(header) != 64
        or header[:7] != b"\x7fELF\x02\x01\x01"
        or int.from_bytes(header[16:18], "little") not in (2, 3)
        or int.from_bytes(header[18:20], "little") != 62
    ):
        raise ValueError("expected an x86_64 ELF executable")


def verify_archive(path: Path, manifest: dict) -> None:
    expected = {f"yield/{name}" for name in [*manifest["files"], MANIFEST_NAME]}
    with zipfile.ZipFile(path) as archive:
        names = archive.namelist()
        if len(names) != len(expected) or set(names) != expected:
            raise ValueError("Linux archive layout does not match its manifest")
        if json.loads(archive.read(f"yield/{MANIFEST_NAME}")) != manifest:
            raise ValueError("Linux archive manifest does not match the build")
        for name, metadata in manifest["files"].items():
            entry = archive.getinfo(f"yield/{name}")
            if entry.create_system != 3 or entry.external_attr >> 16 != (
                stat.S_IFREG | metadata["mode"]
            ):
                raise ValueError(f"Linux archive mode mismatch for {name}")
            with archive.open(entry) as contents:
                if hashlib.file_digest(contents, "sha256").hexdigest() != metadata["sha256"]:
                    raise ValueError(f"Linux archive digest mismatch for {name}")


def assemble_archive(
    root: Path, binary: Path, archive_path: Path, source_sha: str, version: str
) -> None:
    require_x86_64_elf(binary)
    members = {"yield": (binary, 0o755)}
    members.update({name: (root / source, mode) for name, (source, mode) in FILES.items()})
    manifest = {
        "schema_version": 1,
        "source_sha": source_sha,
        "version": version,
        "target": TARGET,
        "build_profile": PROFILE,
        "installation": "manual",
        "files": {},
    }
    with zipfile.ZipFile(archive_path, "x") as archive:
        for name, (source, mode) in members.items():
            if source.is_symlink() or not source.is_file():
                raise ValueError(f"missing regular package input for {name}")
            entry = zipfile.ZipInfo(f"yield/{name}")
            entry.create_system = 3
            entry.external_attr = (stat.S_IFREG | mode) << 16
            entry.compress_type = zipfile.ZIP_DEFLATED
            entry.file_size = source.stat().st_size
            with source.open("rb") as contents, archive.open(entry, "w") as destination:
                shutil.copyfileobj(contents, destination)
                contents.seek(0)
                digest = hashlib.file_digest(contents, "sha256").hexdigest()
            manifest["files"][name] = {"sha256": digest, "mode": mode}
        archive.writestr(f"yield/{MANIFEST_NAME}", json.dumps(manifest, indent=2) + "\n")
    verify_archive(archive_path, manifest)


def require_checkout(root: Path, source_sha: str) -> None:
    if command("git", "rev-parse", "HEAD", cwd=root) != source_sha:
        raise ValueError("checkout HEAD does not match --sha")
    if command("git", "status", "--porcelain", "--untracked-files=normal", cwd=root):
        raise ValueError("source checkout must be clean")


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--sha", required=True, help="exact 40-character source commit")
    parser.add_argument("--out-dir", required=True, type=Path, help="new private artifact directory")
    args = parser.parse_args()
    if platform.system() != "Linux" or platform.machine() != "x86_64":
        parser.error("packaging requires an x86_64 Linux host")
    if not re.fullmatch(r"[0-9a-f]{40}", args.sha):
        parser.error("--sha must be a full lowercase commit ID")
    root = Path(__file__).resolve().parents[1]
    output = args.out_dir.expanduser().absolute()
    if output.exists() or output.is_symlink():
        parser.error("artifact directory must be new")
    try:
        require_checkout(root, args.sha)
        metadata = json.loads(command(
            "cargo", "metadata", "--locked", "--no-deps", "--format-version", "1", cwd=root
        ))
        version = next(
            package["version"] for package in metadata["packages"] if package["name"] == "yield"
        )
        subprocess.run(
            [
                "cargo", "build", "--locked", "--target", TARGET, "--profile", PROFILE,
                "-p", "yield", "--bin", "yield",
            ],
            cwd=root,
            check=True,
        )
        require_checkout(root, args.sha)
        binary = Path(metadata["target_directory"]) / TARGET / PROFILE / "yield"
        output.parent.mkdir(parents=True, exist_ok=True)
        with tempfile.TemporaryDirectory(prefix=".yield-linux-", dir=output.parent) as temp:
            archive = Path(temp) / ARCHIVE_NAME
            assemble_archive(root, binary, archive, args.sha, version)
            checksum = Path(temp) / "SHA256SUMS"
            checksum.write_text(f"{sha256(archive)}  {ARCHIVE_NAME}\n", encoding="utf-8")
            output.mkdir(mode=0o700)
            archive.rename(output / ARCHIVE_NAME)
            checksum.rename(output / checksum.name)
    except (ValueError, OSError, subprocess.CalledProcessError) as error:
        parser.exit(1, f"Linux package failed: {error}\n")
    print(output / ARCHIVE_NAME)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
