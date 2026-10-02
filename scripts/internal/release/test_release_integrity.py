#!/usr/bin/env python3
"""Exercise the production read-only verifier with ephemeral signed release fixtures.

Placeholder payloads and test keys establish verifier behavior, not native release,
installation, staging, or production signing qualification.
"""

from __future__ import annotations

import argparse
import base64
import json
import shutil
import subprocess
import sys
import tempfile
import unittest
import zipfile
from pathlib import Path

import verify_published_release as verifier


class ReleaseIntegrityTests(unittest.TestCase):
    def setUp(self) -> None:
        if not shutil.which("openssl"):
            self.fail("openssl is required for real Ed25519 integrity checks")
        self.temp = tempfile.TemporaryDirectory(prefix="yield-integrity-test-")
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.assets = self.root / "downloaded"
        self.assets.mkdir()
        self.installed = self.root / "installed-app"
        self.installed.write_bytes(b"installed application sentinel")
        self.key = self.root / "test-key.pem"
        self.public_key = self.generate_key(self.key)
        self.contract = verifier.load_release_contract(verifier.REPO_ROOT / "release_contract.toml")
        self.layout = verifier.archive_layout_from_contract(self.contract)
        self.prepare("stable", "0.19.1")

    def run_openssl(self, *args: str) -> bytes:
        return subprocess.run(
            ["openssl", *args], check=True, stdout=subprocess.PIPE, stderr=subprocess.PIPE
        ).stdout

    def generate_key(self, path: Path) -> str:
        self.run_openssl("genpkey", "-algorithm", "ED25519", "-out", str(path))
        public = self.run_openssl("pkey", "-in", str(path), "-pubout", "-outform", "DER")
        self.assertTrue(public.startswith(verifier.ED25519_SPKI_PREFIX))
        return base64.b64encode(public[-32:]).decode("ascii")

    def prepare(self, channel: str, version: str) -> None:
        self.args = argparse.Namespace(
            channel=channel, version=version, target_version="0.19.1",
            commit="0123456789abcdef0123456789abcdef01234567", build_date="2026-10-02",
            surface="github", source="controlled signed fixture",
            checksum_public_key=self.public_key,
        )
        self.expected = verifier.expected_release_assets(
            self.contract, channel, version, self.args.target_version
        )
        for asset in self.expected.zips:
            files = self.layout.expected_files_for(asset.platform) + ["update-manifest.json"]
            manifest = dict(
                app="yield", channel=channel, target=asset.target, platform=asset.platform,
                arch=asset.arch, version=version, target_version=self.args.target_version,
                commit=self.args.commit, build_date=self.args.build_date, files=files,
            )
            with zipfile.ZipFile(self.assets / asset.name, "w") as archive:
                for name in files:
                    payload = json.dumps(manifest).encode() if name == "update-manifest.json" else b"placeholder"
                    archive.writestr(f"{self.layout.root_dir}/{name}", payload)
        self.checksum = self.assets / self.expected.checksum
        self.signature = self.assets / self.expected.signature
        self.checksum.write_text("".join(
            f"{verifier.sha256(self.assets / asset.name)}  {asset.name}\n"
            for asset in self.expected.zips
        ), encoding="utf-8")
        self.sign()

    def sign(self) -> None:
        signature = self.run_openssl(
            "pkeyutl", "-sign", "-inkey", str(self.key), "-rawin", "-in", str(self.checksum)
        )
        self.signature.write_bytes(base64.b64encode(signature))

    def verify(self) -> list[str]:
        before = {path.name: path.read_bytes() for path in self.assets.iterdir()}
        installed = self.installed.read_bytes()
        errors = verifier.validate_downloaded_assets(
            self.assets, {}, self.expected, self.layout, self.args
        )
        self.assertEqual(before, {path.name: path.read_bytes() for path in self.assets.iterdir()})
        self.assertEqual(installed, self.installed.read_bytes())
        return errors

    def assert_rejected(self, message: str) -> None:
        errors = self.verify()
        self.assertTrue(any(message in error for error in errors), errors)

    def test_valid_signed_channel_matrix(self) -> None:
        for channel, version in (
            ("stable", "0.19.1"), ("rc", "0.19.1-rc.1"),
            ("nightly", "0.19.1-nightly.20261002+01234567"),
        ):
            with self.subTest(channel=channel):
                self.prepare(channel, version)
                self.assertEqual([], self.verify())

    def test_tampered_payload(self) -> None:
        path = self.assets / self.expected.zips[0].name
        path.write_bytes(path.read_bytes() + b"tampered")
        self.assert_rejected("Checksum mismatch")

    def test_tampered_checksums_without_resigning(self) -> None:
        self.checksum.write_bytes(self.checksum.read_bytes() + b"\n")
        self.assert_rejected("signature verification failed")

    def test_wrong_key(self) -> None:
        self.args.checksum_public_key = self.generate_key(self.root / "wrong-key.pem")
        self.assert_rejected("signature verification failed")

    def test_signature_negatives(self) -> None:
        for signature, message in (
            (b"not-base64!", "not valid base64"),
            (b"\xff", "not valid base64"),
            (base64.b64encode(b"x" * 64), "signature verification failed"),
        ):
            with self.subTest(signature=message):
                self.signature.write_bytes(signature)
                self.assert_rejected(message)

    def test_missing_each_required_asset(self) -> None:
        for name in self.expected.required_assets:
            with self.subTest(asset=name):
                path = self.assets / name
                data = path.read_bytes()
                path.unlink()
                self.assert_rejected("asset is missing")
                path.write_bytes(data)

    def test_signed_ambiguous_or_malformed_checksums(self) -> None:
        original = self.checksum.read_bytes()
        first = original.splitlines(keepends=True)[0]
        name = self.expected.zips[0].name
        for data, message in (
            (original + first, "duplicates checksum entry"),
            (f"{'0' * 64}  {name}\n".encode() + original, "duplicates checksum entry"),
            (original + f"{'0' * 64}  unexpected.zip\n".encode(), "unexpected asset"),
            (b"bad-hash  " + name.encode() + b"\n", "not a SHA-256 checksum entry"),
            (b"\xff", "not valid UTF-8"),
            (b"", "missing entry"),
        ):
            with self.subTest(message=message):
                self.checksum.write_bytes(data)
                # OpenSSL cannot sign an empty input on every supported version.
                if data:
                    self.sign()
                self.assert_rejected(message)

    def test_invalid_public_key(self) -> None:
        for key in ("not-base64!", base64.b64encode(b"short").decode()):
            with self.subTest(key=key):
                self.args.checksum_public_key = key
                self.assert_rejected("public key")

    def test_command_exit_status_and_preservation(self) -> None:
        release = self.root / "release.json"
        release.write_text(json.dumps(dict(
            tagName="v0.19.1", isPrerelease=False, targetCommitish=self.args.commit,
            body="Controlled fixture for Yield 0.19.1",
            assets=[dict(name=name) for name in self.expected.required_assets],
        )), encoding="utf-8")
        command = [
            sys.executable, str(Path(verifier.__file__)),
            "--channel", "stable", "--version", self.args.version,
            "--target-version", self.args.target_version, "--commit", self.args.commit,
            "--build-date", self.args.build_date, "--tag", "v0.19.1",
            "--release-json", str(release), "--asset-dir", str(self.assets),
            "--checksum-public-key", self.public_key,
        ]
        for tampered in (False, True):
            with self.subTest(tampered=tampered):
                if tampered:
                    self.checksum.write_bytes(self.checksum.read_bytes() + self.checksum.read_bytes())
                    self.sign()
                before = {path.name: path.read_bytes() for path in self.assets.iterdir()}
                installed = self.installed.read_bytes()
                result = subprocess.run(command, capture_output=True, text=True, check=False)
                self.assertEqual(int(tampered), result.returncode, result.stderr)
                if tampered:
                    self.assertIn("duplicates checksum entry", result.stderr)
                self.assertEqual(before, {path.name: path.read_bytes() for path in self.assets.iterdir()})
                self.assertEqual(installed, self.installed.read_bytes())


if __name__ == "__main__":
    unittest.main(verbosity=2)
