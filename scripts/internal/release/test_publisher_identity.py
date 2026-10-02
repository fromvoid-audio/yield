#!/usr/bin/env python3
"""Actual packer/publisher HTTP fixtures; placeholder payloads are not native qualification."""
from __future__ import annotations

import copy
import hashlib
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import threading
import unittest
import urllib.parse
import zipfile

import publisher_identity as identity
import verify_published_release as verifier

ROOT = Path(__file__).resolve().parents[3]
SCRIPT = ROOT / "scripts/internal/release/publish_portalsurfer_release.sh"
MANIFEST = "yield/update-manifest.json"
COMMIT = "12345678" + "a" * 32


class Server:
    def __init__(self, phase=None, mutation=None, content_length=True):
        self.requests = []
        self.files = []
        self.release = None
        self.body = None
        self.phase, self.mutation = phase, mutation
        owner = self
        class Handler(BaseHTTPRequestHandler):
            def log_message(self, *args):
                pass

            def reply(self, value, phase=None):
                if phase is not None and phase == owner.phase:
                    value = owner.mutation(copy.deepcopy(value))
                raw = value if isinstance(value, bytes) else json.dumps(value).encode()
                self.send_response(200)
                self.send_header("Content-Type", "application/json")
                if content_length:
                    self.send_header("Content-Length", str(len(raw)))
                self.end_headers()
                try:
                    self.wfile.write(raw)
                except (BrokenPipeError, ConnectionResetError):
                    pass

            def do_PUT(self):
                raw = self.rfile.read(int(self.headers["Content-Length"]))
                owner.requests.append((self.path, dict(self.headers), raw))
                pieces = urllib.parse.urlsplit(self.path).path.split("/")
                metadata = {"build_id": pieces[5], "build_number": int(self.headers["X-Yield-Build-Number"]),
                            "channel": self.headers["X-Yield-Release-Channel"],
                            "version": self.headers["X-Yield-Release-Version"],
                            "commit": self.headers.get("X-Yield-Commit"),
                            "released_at": self.headers["X-Yield-Released-At"]}
                if pieces[-1] == "commit":
                    owner.body = json.loads(raw)
                    metadata["files"] = owner.body["files"]
                    metadata["changelog"] = {"format": "markdown", "url": "/releases/log"}
                    owner.release = metadata
                    self.reply({"committed": True, "release": metadata,
                                "changelog": metadata["changelog"],
                                "full_changelog": {"format": "markdown", "url": "/changelog"}}, "commit")
                else:
                    name = urllib.parse.unquote(pieces[-1])
                    file = {"name": name, "sha256": hashlib.sha256(raw).hexdigest(), "size_bytes": len(raw)}
                    owner.files.append((file, raw))
                    self.reply({"staged": True, **metadata, "file": file}, "staging")

            def do_GET(self):
                if self.path.endswith("/releases"):
                    self.reply({"releases": [owner.release] if owner.release else []},
                               "catalog" if owner.release else "pre_catalog")
                elif self.path.endswith("/changelog"):
                    field = "changelog" if "/releases/" in self.path else "full_changelog"
                    body = owner.body[field]["body"] if owner.body else ""
                    self.reply({"build_id": owner.release["build_id"] if owner.release else None,
                                "changelog": {"body": body}}, "pre_changelog" if owner.release is None else None)
                else:
                    self.send_error(404)
        self.http = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        self.thread = threading.Thread(target=self.http.serve_forever, daemon=True)
        self.thread.start()
        self.url = f"http://127.0.0.1:{self.http.server_port}/yield/api/v1/release-uploads"

    def close(self):
        self.http.shutdown()
        self.http.server_close()
        self.thread.join()


class PublisherIdentity(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.temp = tempfile.TemporaryDirectory(prefix="yield-publisher-fixture-")
        cls.addClassCleanup(cls.temp.cleanup)
        cls.workspace = Path(cls.temp.name)
        cls.contract = verifier.load_release_contract(ROOT / "release_contract.toml")
        cls.versions = {"stable": "0.19.1", "rc": "0.19.1-rc.2",
                        "nightly": f"0.19.1-nightly.2026-10-02+{COMMIT[:8]}"}
        (cls.workspace / "assets").mkdir()
        shutil.copyfile(ROOT / "assets/logo3.icns", cls.workspace / "assets/logo3.icns")
        for target in cls.contract["targets"]:
            binary = cls.workspace / "target" / target / "release" / ("yield.exe" if "windows" in target else "yield")
            binary.parent.mkdir(parents=True)
            binary.write_bytes(b"CONTROLLED PUBLISHER FIXTURE - NOT A NATIVE BINARY\n")
        for channel, version in cls.versions.items():
            out = cls.workspace / channel
            assets = verifier.expected_release_assets(cls.contract, channel, version, "0.19.1")
            for asset in assets.zips:
                subprocess.run(["bash", str(ROOT / "scripts/internal/release/build_release_zip.sh"),
                    "--target", asset.target, "--platform", asset.platform, "--arch", asset.arch,
                    "--channel", channel, "--version", version, "--target-version", "0.19.1",
                    "--git-sha", COMMIT, "--build-date", "2026-10-02", "--out-dir", str(out)],
                    cwd=cls.workspace, env={**os.environ, "YIELD_SKIP_BUILD": "1", "YIELD_MACOS_SIGNING": "0"},
                    check=True, stdout=subprocess.PIPE, stderr=subprocess.PIPE, timeout=20)
            # The release workflow collects ZIPs and writes its combined checksum file.
            # The packer per-target intermediate is not a declared release asset.
            (out / "checksums-entry.txt").unlink()
            cls.checksums(out, channel)

    @classmethod
    def checksums(cls, directory, channel):
        assets = verifier.expected_release_assets(cls.contract, channel, cls.versions[channel], "0.19.1")
        (directory / assets.checksum).write_text("".join(
            f"{verifier.sha256(directory / asset.name)}  {asset.name}\n" for asset in assets.zips))

    def artifacts(self, channel="stable"):
        temp = tempfile.TemporaryDirectory(prefix="publisher-case-")
        self.addCleanup(temp.cleanup)
        case = Path(temp.name)
        shutil.copytree(self.workspace / channel, case / "artifacts")
        (case / "release-log.md").write_text(f"# Yield {self.versions[channel]}\n\nControlled local fixture.\n")
        return case

    def run_publisher(self, case, server, channel="stable", number="44", version=None, build_id=None):
        version = version or self.versions[channel]
        result = subprocess.run(["bash", str(SCRIPT), "--channel", channel,
            "--build-id", build_id or f"yield-{version}", "--build-number", number,
            "--release-version", version, "--released-at", "2026-10-02T00:00:00Z",
            "--artifact-dir", str(case / "artifacts"), "--release-log", str(case / "release-log.md"),
            "--full-changelog-out", str(case / "full.md")], cwd=ROOT,
            env={**os.environ, "PORTALSURFER_RELEASE_UPLOAD_TOKEN": "controlled-loopback-fixture",
                 "PORTALSURFER_RELEASE_UPLOAD_URL": server.url, "GITHUB_SHA": "f" * 40,
                 "GITHUB_STEP_SUMMARY": str(case / "summary.md"), "NO_PROXY": "127.0.0.1",
                 "no_proxy": "127.0.0.1", "PORTALSURFER_RELEASE_CONNECT_TIMEOUT_SECONDS": "1",
                 "PORTALSURFER_RELEASE_MAX_TIME_SECONDS": "3"},
            stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True, timeout=20)
        return result

    def server(self, phase=None, mutation=None, content_length=True):
        server = Server(phase, mutation, content_length)
        self.addCleanup(server.close)
        return server

    def rewrite_manifest(self, case, change, channel="stable", raw=None):
        archive = sorted((case / "artifacts").glob("*.zip"))[0]
        altered = archive.with_suffix(".tmp")
        with zipfile.ZipFile(archive) as source, zipfile.ZipFile(altered, "w") as target:
            for info in source.infolist():
                payload = source.read(info)
                if info.filename == MANIFEST:
                    value = json.loads(payload)
                    change(value)
                    payload = raw if raw is not None else json.dumps(value).encode()
                target.writestr(info, payload)
        altered.replace(archive)
        self.checksums(case / "artifacts", channel)

    def test_actual_packer_and_http_roundtrip_all_channels_and_targets(self):
        for channel in self.contract["channels"]:
            with self.subTest(channel=channel):
                case, server = self.artifacts(channel), self.server()
                result = self.run_publisher(case, server, channel)
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertEqual(server.body["commit"], COMMIT)
                self.assertEqual(len(server.files), len(self.contract["targets"]) + 1)
                for path, headers, raw in server.requests:
                    self.assertEqual(headers["X-Yield-Commit"], COMMIT)
                    self.assertEqual(headers["X-Yield-Release-Channel"], channel)
                for file, raw in server.files:
                    if file["name"].endswith(".zip"):
                        import io
                        with zipfile.ZipFile(io.BytesIO(raw)) as archive:
                            manifest = json.loads(archive.read(MANIFEST))
                        self.assertEqual(manifest["commit"], COMMIT)
                        self.assertEqual(manifest["channel"], channel)
                self.assertIn(COMMIT, (case / "summary.md").read_text())
                self.assertNotIn("f" * 40, (case / "summary.md").read_text())

    def test_every_manifest_identity_and_malformed_input_fail_before_first_put(self):
        changes = [(field, "wrong") for field in ("commit", "channel", "version", "target", "target_version", "build_date")]
        changes += [("commit", COMMIT[:8]), ("commit", "0" * 40), ("commit", "g" * 40), ("commit", "b" * 40)]
        for field, value in changes:
            with self.subTest(field=field, value=value):
                case, server = self.artifacts(), self.server()
                self.rewrite_manifest(case, lambda m: m.update({field: value}))
                result = self.run_publisher(case, server)
                self.assertNotEqual(result.returncode, 0)
                self.assertEqual(server.requests, [])
        for raw in (b"[]", b"{", b'{"commit":"' + COMMIT.encode() + b'","commit":"' + COMMIT.encode() + b'"}'):
            case, server = self.artifacts(), self.server()
            self.rewrite_manifest(case, lambda m: None, raw=raw)
            self.assertNotEqual(self.run_publisher(case, server).returncode, 0)
            self.assertEqual(server.requests, [])

    def test_invalid_publication_context_and_integrity_never_upload(self):
        for kwargs in ({"number": "0"}, {"number": "18446744073709551616"}, {"number": "-1"},
                       {"version": "4294967296.19.1"}, {"build_id": "yield-foreign"}):
            case, server = self.artifacts(), self.server()
            self.assertNotEqual(self.run_publisher(case, server, **kwargs).returncode, 0)
            self.assertEqual(server.requests, [])
        case, server = self.artifacts(), self.server()
        checksum = next((case / "artifacts").glob("checksums-*.txt"))
        checksum.write_text("malformed integrity data\n")
        self.assertNotEqual(self.run_publisher(case, server).returncode, 0)
        self.assertEqual(server.requests, [])

    def test_stage_commit_and_catalog_substitution_are_not_publication_success(self):
        def metadata(value, phase):
            return value if phase == "staging" else value["release"] if phase == "commit" else value["releases"][0]
        for phase in ("staging", "commit", "catalog"):
            for field, value in (("commit", None), ("commit", COMMIT[:8]), ("commit", "b" * 40),
                                 ("channel", "rc"), ("version", "0.19.2"), ("build_number", True),
                                 ("build_number", identity.MAX_U64 + 1)):
                with self.subTest(phase=phase, field=field, value=value):
                    def mutate(response, field=field, value=value, phase=phase):
                        metadata(response, phase)[field] = value
                        return response
                    case, server = self.artifacts(), self.server(phase, mutate)
                    result = self.run_publisher(case, server)
                    self.assertNotEqual(result.returncode, 0, result.stdout)
                    self.assertIn("| Status | failure during", (case / "summary.md").read_text())
                    if phase == "staging":
                        self.assertEqual(len(server.requests), 1)
                        self.assertIsNone(server.body)

    def test_asset_identity_duplicate_catalog_and_bounded_http_response_reject(self):
        for phase in ("staging", "commit", "catalog"):
            def mutate(response, phase=phase):
                file = response["file"] if phase == "staging" else response["release"]["files"][0] if phase == "commit" else response["releases"][0]["files"][0]
                file["sha256"] = "f" * 64
                return response
            case, server = self.artifacts(), self.server(phase, mutate)
            self.assertNotEqual(self.run_publisher(case, server).returncode, 0)
        for mutate in (lambda r: {"releases": r["releases"] * 2}, lambda r: b"[", lambda r: b" " * (identity.MAX_JSON + 1)):
            case, server = self.artifacts(), self.server("catalog", mutate)
            self.assertNotEqual(self.run_publisher(case, server).returncode, 0)

    def test_precommit_catalog_response_is_bounded_before_assembly(self):
        for phase in ("pre_catalog", "pre_changelog"):
            case = self.artifacts()
            server = self.server(phase, lambda r: json.dumps(r).encode() + b" " * identity.MAX_JSON)
            result = self.run_publisher(case, server)
            self.assertNotEqual(result.returncode, 0, "oversized pre-commit metadata was accepted")
            self.assertIsNone(server.body, "must reject before remote commit")

    def test_unknown_length_http_response_is_capped_during_transfer(self):
        for phase in ("staging", "pre_catalog", "pre_changelog"):
            with self.subTest(phase=phase):
                case = self.artifacts()
                server = self.server(phase, lambda r: b" " * (identity.MAX_JSON + 65536),
                                     content_length=False)
                result = self.run_publisher(case, server)
                self.assertNotEqual(result.returncode, 0)
                self.assertIn("curl: (63)", result.stderr,
                              "transport must cap unknown-length bytes before JSON parsing")
                self.assertIsNone(server.body, "oversized transfer must never reach commit")

    def test_nightly_abbreviated_build_id_uses_full_manifest_identity(self):
        case, server = self.artifacts("nightly"), self.server()
        result = self.run_publisher(case, server, "nightly", build_id=f"yield-nightly-b44-{COMMIT[:8]}")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(server.body["commit"], COMMIT)
        for build_id in ("yield-nightly-b44-ffffffff", f"yield-nightly-b45-{COMMIT[:8]}"):
            case, server = self.artifacts("nightly"), self.server()
            self.assertNotEqual(self.run_publisher(case, server, "nightly", build_id=build_id).returncode, 0)
            self.assertEqual(server.requests, [])

    def test_file_name_size_and_complete_inventory_are_exact(self):
        for phase in ("staging", "commit", "catalog"):
            for field, value in (("name", "foreign.zip"), ("size_bytes", True),
                                 ("size_bytes", identity.MAX_U64 + 1)):
                def mutate(response, phase=phase, field=field, value=value):
                    file = response["file"] if phase == "staging" else response["release"]["files"][0] if phase == "commit" else response["releases"][0]["files"][0]
                    file[field] = value
                    return response
                case, server = self.artifacts(), self.server(phase, mutate)
                self.assertNotEqual(self.run_publisher(case, server).returncode, 0)
        for phase in ("commit", "catalog"):
            def duplicate(response, phase=phase):
                release = response["release"] if phase == "commit" else response["releases"][0]
                release["files"][1] = release["files"][0]
                return response
            case, server = self.artifacts(), self.server(phase, duplicate)
            self.assertNotEqual(self.run_publisher(case, server).returncode, 0)

    def test_malformed_and_ambiguous_actual_http_metadata_never_reaches_commit(self):
        malformed = [b"[]", b"null", b"{", b'{"staged":true,"staged":true}',
                     b'{"unused":NaN}', b'{"unused":' * 18 + b'null' + b'}' * 18]
        for raw in malformed:
            case, server = self.artifacts(), self.server("staging", lambda r, raw=raw: raw)
            result = self.run_publisher(case, server)
            self.assertNotEqual(result.returncode, 0)
            self.assertEqual(len(server.requests), 1)
            self.assertIsNone(server.body)

    def test_canonical_prerelease_date_hash_and_overflow_fail_before_upload(self):
        for channel, version in (("rc", "0.19.1-rc.0"), ("rc", "0.19.1-rc.4294967296"),
                                 ("nightly", "0.19.1-nightly.2026-02-30+12345678"),
                                 ("nightly", "0.19.1-nightly.2026-10-02+ffffffff")):
            case, server = self.artifacts(channel), self.server()
            self.assertNotEqual(self.run_publisher(case, server, channel, version=version).returncode, 0)
            self.assertEqual(server.requests, [])

    def test_snapshot_upload_keeps_validated_bytes_after_input_change(self):
        case = self.artifacts()
        original = {p.name: p.read_bytes() for p in (case / "artifacts").iterdir()}
        def mutate(response):
            for path in (case / "artifacts").iterdir():
                path.write_bytes(b"changed original after snapshot")
            return response
        server = self.server("staging", mutate)
        result = self.run_publisher(case, server)
        self.assertEqual(result.returncode, 0, result.stderr)
        for file, raw in server.files:
            self.assertEqual(raw, original[file["name"]])


if __name__ == "__main__":
    unittest.main(verbosity=2)
