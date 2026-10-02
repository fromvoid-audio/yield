#!/usr/bin/env python3
"""Controlled Linux metadata fixtures, NOT native binaries or release qualification.

Exercise the actual release ZIP packer with temporary placeholder binary bytes and
its real contract/validator. Never install, publish, sign or access a live profile.
"""
from __future__ import annotations

import argparse
import copy
import json
import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import unittest
import zipfile

import verify_published_release as verifier

ROOT = Path(__file__).resolve().parents[3]
MANIFEST = "yield/update-manifest.json"


class ControlledReleaseMetadata(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.temp = tempfile.TemporaryDirectory(prefix="yield-metadata-fixture-")
        cls.addClassCleanup(cls.temp.cleanup)
        cls.workspace = Path(cls.temp.name)
        cls.contract = verifier.load_release_contract(ROOT / "release_contract.toml")
        cls.layout = verifier.archive_layout_from_contract(cls.contract)
        cls.fixtures = []
        target_version = "0.19.1"
        versions = {"stable": target_version, "rc": f"{target_version}-rc.2",
                    "nightly": f"{target_version}-nightly.20261002"}
        (cls.workspace / "assets").mkdir()
        shutil.copyfile(ROOT / "assets/logo3.icns", cls.workspace / "assets/logo3.icns")
        # The production script's existing skip-build mode packages finite fixture bytes.
        # No fake compiler, host support assertion, or real native binary is involved.
        for target in cls.contract["targets"]:
            binary = cls.workspace / "target" / target / "release" / (
                "yield.exe" if "windows" in target else "yield")
            binary.parent.mkdir(parents=True)
            binary.write_bytes(b"CONTROLLED METADATA FIXTURE - NOT A NATIVE BINARY\n")
        for channel in cls.contract["channels"]:
            args = argparse.Namespace(channel=channel, version=versions[channel],
                target_version=target_version, commit="1" * 40, build_date="2026-10-02")
            expected = verifier.expected_release_assets(
                cls.contract, channel, args.version, target_version)
            for asset in expected.zips:
                subprocess.run(["bash", str(ROOT / "scripts/internal/release/build_release_zip.sh"),
                    "--target", asset.target, "--platform", asset.platform, "--arch", asset.arch,
                    "--channel", channel, "--version", args.version,
                    "--target-version", target_version, "--git-sha", args.commit,
                    "--build-date", args.build_date, "--out-dir", str(cls.workspace / channel)],
                    cwd=cls.workspace, env={**os.environ, "YIELD_SKIP_BUILD": "1",
                        "YIELD_MACOS_SIGNING": "0"}, check=True,
                    stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True, timeout=20)
                cls.fixtures.append((asset, args, cls.workspace / channel / asset.name))

    def errors(self, path, asset, args):
        return verifier.validate_zip_manifest(path, asset, self.layout, args)

    def rewrite(self, source, manifest):
        output = self.workspace / "changed.zip"
        with zipfile.ZipFile(source) as original, zipfile.ZipFile(output, "w") as changed:
            for entry in original.infolist():
                data = manifest if entry.filename == MANIFEST else original.read(entry)
                changed.writestr(entry, data)
        return output

    def test_actual_packer_matches_every_declared_channel_and_target(self):
        self.assertEqual(len(self.fixtures), len(self.contract["channels"]) * len(self.contract["targets"]))
        for asset, args, path in self.fixtures:
            with self.subTest(channel=args.channel, target=asset.target):
                self.assertEqual(self.errors(path, asset, args), [])

    def test_other_channel_version_or_target_never_substitutes_the_intended_artifact(self):
        for asset, args, intended in self.fixtures:
            for other_asset, other_args, other in self.fixtures:
                if intended == other:
                    continue
                with self.subTest(channel=args.channel, target=asset.target,
                                  other_channel=other_args.channel, other_target=other_asset.target):
                    self.assertTrue(self.errors(other, asset, args))

    def test_each_identity_field_mismatch_or_absence_is_rejected(self):
        for asset, args, path in self.fixtures:
            with zipfile.ZipFile(path) as archive:
                manifest = json.loads(archive.read(MANIFEST))
            for field in ("app", "channel", "target", "platform", "arch", "version",
                          "target_version", "commit", "build_date"):
                for absent in (False, True):
                    changed = copy.deepcopy(manifest)
                    if absent:
                        del changed[field]
                    else:
                        changed[field] = "wrong-fixture-identity"
                    with self.subTest(channel=args.channel, target=asset.target, field=field, absent=absent):
                        errors = self.errors(self.rewrite(path, json.dumps(changed).encode()), asset, args)
                        self.assertTrue(any(f"manifest {field} " in error for error in errors), errors)

    def test_malformed_metadata_is_a_rejection_not_an_exception_or_ambiguous_success(self):
        asset, args, path = self.fixtures[0]
        with zipfile.ZipFile(path) as archive:
            original = archive.read(MANIFEST)
        malformed = [b"[]", b"null", b"true", b"17", b'"string"', b"{", b"\xff",
                     original.rstrip()[:-1] + b', "channel": ' + json.dumps(args.channel).encode() + b'}',
                     b" " * (1024 * 1024) + original,
                     b'{"nested":' * 1500 + b'null' + b'}' * 1500]
        for manifest in malformed:
            with self.subTest(shape=manifest[:40]):
                self.assertTrue(self.errors(self.rewrite(path, manifest), asset, args))

    def test_missing_or_duplicate_archive_manifest_is_rejected(self):
        asset, args, path = self.fixtures[0]
        for mode in ("missing", "duplicate", "other-location"):
            output = self.workspace / "ambiguous.zip"
            with zipfile.ZipFile(path) as original, zipfile.ZipFile(output, "w") as changed:
                for entry in original.infolist():
                    if entry.filename != MANIFEST:
                        changed.writestr(entry, original.read(entry))
                if mode != "missing":
                    changed.writestr(MANIFEST, original.read(MANIFEST))
                    second = MANIFEST if mode == "duplicate" else "other/update-manifest.json"
                    # ZIP duplicate-name warnings are expected for this deliberate negative.
                    import warnings
                    with warnings.catch_warnings():
                        warnings.simplefilter("ignore", UserWarning)
                        changed.writestr(second, original.read(MANIFEST))
            with self.subTest(mode=mode):
                self.assertTrue(self.errors(output, asset, args))


if __name__ == "__main__":
    unittest.main(verbosity=2)
