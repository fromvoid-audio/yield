#!/usr/bin/env python3
"""Build one private macOS GPUI bundle from an exact clean commit."""

import argparse
import hashlib
import json
import os
from pathlib import Path
import platform
import plistlib
import re
import shutil
import subprocess
import sys
import tempfile


BUNDLE_NAME = "Yield.app"
BUNDLE_ID = "org.portalsurfer.yield.dev"


def command(*args: str, cwd: Path) -> str:
    return subprocess.check_output(args, cwd=cwd, text=True).strip()


def sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as source:
        for chunk in iter(lambda: source.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--sha", required=True, help="exact 40-character source commit")
    parser.add_argument("--out-dir", required=True, type=Path, help="new private artifact directory")
    args = parser.parse_args()
    root = Path(__file__).resolve().parent.parent
    if platform.system() != "Darwin":
        parser.error("macOS packaging requires a macOS host")
    if not re.fullmatch(r"[0-9a-f]{40}", args.sha):
        parser.error("--sha must be a full lowercase commit ID")
    if command("git", "rev-parse", "HEAD", cwd=root) != args.sha:
        parser.error("checkout HEAD does not match --sha")
    if command("git", "rev-parse", "--is-shallow-repository", cwd=root) == "true":
        parser.error("full Git history is required for the bundle build number")
    if command("git", "status", "--porcelain", "--untracked-files=normal", cwd=root):
        parser.error("source checkout must be clean")
    output = args.out_dir.expanduser().resolve()
    if output.exists():
        parser.error(f"artifact directory already exists: {output}")

    metadata = json.loads(command(
        "cargo", "metadata", "--locked", "--no-deps", "--format-version", "1", cwd=root
    ))
    package = next(
        (package for package in metadata["packages"] if package["name"] == "yield"),
        None,
    )
    if package is None:
        parser.error("cargo metadata has no yield package")
    version = package["version"]
    build_number = command("git", "rev-list", "--count", "HEAD", cwd=root)
    target = Path(metadata["target_directory"])
    binary = target / "release-local" / "yield"
    icon = root / "assets" / "logo3.icns"
    font_license = root / "assets" / "fonts" / "IoskeleyMono-LICENSE.txt"
    for required in (icon, font_license):
        if not required.is_file():
            parser.error(f"missing bundle asset: {required}")

    subprocess.run(
        ["cargo", "build", "--locked", "--profile", "release-local", "-p", "yield"],
        cwd=root,
        check=True,
    )
    if not binary.is_file():
        raise RuntimeError(f"build did not produce {binary}")

    output.parent.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(prefix=".yield-package-", dir=output.parent) as temp:
        staged = Path(temp) / "artifact"
        contents = staged / BUNDLE_NAME / "Contents"
        executable = contents / "MacOS" / "yield"
        resources = contents / "Resources"
        resources.mkdir(parents=True)
        executable.parent.mkdir(parents=True)
        shutil.copy2(binary, executable)
        shutil.copy2(icon, resources / "Yield.icns")
        shutil.copy2(font_license, resources / "IoskeleyMono-LICENSE.txt")
        (contents / "Info.plist").write_bytes(plistlib.dumps({
            "CFBundleExecutable": "yield",
            "CFBundleIdentifier": BUNDLE_ID,
            "CFBundleName": "Yield",
            "CFBundleDisplayName": "Yield",
            "CFBundleIconFile": "Yield.icns",
            "CFBundlePackageType": "APPL",
            "CFBundleShortVersionString": version,
            "CFBundleVersion": build_number,
            "NSHighResolutionCapable": True,
            "YieldSourceCommit": args.sha,
        }))
        files = sorted(path for path in staged.rglob("*") if path.is_file())
        manifest = {
            "schema_version": 1,
            "source_sha": args.sha,
            "package_version": version,
            "bundle_version": build_number,
            "bundle_identifier": BUNDLE_ID,
            "host_arch": platform.machine(),
            "bundle": BUNDLE_NAME,
            "files": [
                {"path": str(path.relative_to(staged)), "size": path.stat().st_size, "sha256": sha256(path)}
                for path in files
            ],
        }
        (staged / "manifest.json").write_text(json.dumps(manifest, indent=2) + "\n")
        if output.exists():
            raise RuntimeError(f"artifact directory appeared during packaging: {output}")
        os.rename(staged, output)
    print(output)
    return 0


if __name__ == "__main__":
    try:
        sys.exit(main())
    except (OSError, subprocess.CalledProcessError, RuntimeError) as error:
        print(f"macOS package failed: {error}", file=sys.stderr)
        sys.exit(1)
