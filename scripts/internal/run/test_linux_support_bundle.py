"""Exercise the installed-layout Linux support command with disposable inputs."""

import os
from pathlib import Path
import re
import shutil
import stat
import subprocess
import tempfile
import unittest
import zipfile


class LinuxSupportBundleTest(unittest.TestCase):
    def setUp(self):
        scripts = Path(__file__).resolve().parents[2]
        self.root = Path(self.enterContext(tempfile.TemporaryDirectory())).resolve()
        self.install = self.root / "installed Yield"
        self.entry = self.install / "scripts" / "run.sh"
        self.doctor = self.entry.with_name("doctor.sh")
        internal = self.entry.parent / "internal" / "run"
        internal.mkdir(parents=True)
        for name in ("run.sh", "doctor.sh"):
            shutil.copy2(scripts / name, self.entry.parent)
        for name in ("bug_bundle.sh", "latest_log.sh", "linux_diagnostics_paths.sh"):
            shutil.copy2(scripts / "internal" / "run" / name, internal)

        tools = self.root / "tools"
        tools.mkdir()
        uname = tools / "uname"
        uname.write_text(
            '#!/bin/sh\ncase "$1" in\n'
            '  -s) echo Linux ;;\n  -m) echo x86_64 ;;\n'
            '  *) echo PRIVATE_HOSTNAME ;;\nesac\n',
            encoding="utf-8",
        )
        uname.chmod(0o700)
        self.path = f"{tools}{os.pathsep}{os.defpath}"
        self.caller = self.root / "support caller"
        self.caller.mkdir()

    def run_command(self, command, *arguments, **environment):
        return subprocess.run(
            [command, *arguments],
            cwd=self.caller,
            env={"PATH": self.path, **environment},
            capture_output=True,
            text=True,
            check=True,
            timeout=5,
        )

    def test_archive_contains_only_allowlisted_status(self):
        config_base = self.root / "PRIVATE_ACCOUNT"
        app = config_base / ".yield"
        logs = app / "logs"
        logs.mkdir(parents=True)
        (app / "config.toml").write_text('token = "PRIVATE_TOKEN"\n', encoding="utf-8")
        for suffix in ("00000001", "00000002"):
            (logs / f"yield_0000000000000000_00000000_00000000_{suffix}.log").write_text(
                "Authorization: Bearer PRIVATE_AUTH_HEADER\n", encoding="utf-8"
            )
        (logs / "yield_PRIVATE_FILENAME.log").write_text("PRIVATE_LOG", encoding="utf-8")

        self.assertIsNotNone(shutil.which("zip", path=os.defpath))
        result = self.run_command(
            self.entry,
            "bug-bundle", "--out-dir", "chosen output", "--logs", "1",
            YIELD_CONFIG_HOME=str(config_base), ZIPOPT="-m",
        )
        (archive,) = (self.caller / "chosen output").glob("yield-bug-bundle-*/support.zip")
        self.assertIn(str(archive), result.stdout)
        self.assertEqual(stat.S_IMODE(archive.parent.stat().st_mode), 0o700)
        self.assertEqual(stat.S_IMODE(archive.stat().st_mode), 0o600)
        with zipfile.ZipFile(archive) as bundle:
            self.assertEqual(bundle.namelist(), ["info.txt"])
            self.assertEqual(bundle.getinfo("info.txt").extra, b"")
            timestamp, *facts = bundle.read("info.txt").decode().splitlines()
        self.assertTrue(re.fullmatch(r"timestamp_utc=\d{8}T\d{6}Z", timestamp))
        self.assertEqual(
            facts,
            [
                "os=linux",
                "architecture=x86_64",
                "config_present=true",
                "log_count_capped=1",
                "log_count_limit=1",
            ],
        )
        self.assertEqual(list(archive.parent.iterdir()), [archive])

    def test_log_and_doctor_resolve_the_same_installed_profile(self):
        home = self.root / "home"
        xdg = self.root / "XDG configuration"
        sandbox = self.install / ".sandbox" / "yield"
        (sandbox / ".yield" / "profiles" / "sandbox").mkdir(parents=True)
        cases = [
            ({"XDG_CONFIG_HOME": "relative"}, [], home / ".config" / ".yield"),
            ({"XDG_CONFIG_HOME": str(xdg)}, [], xdg / ".yield"),
            (
                {"YIELD_CONFIG_HOME": "explicit", "YIELD_CONFIG_PROFILE": "  AuToMaTeD  "},
                [],
                self.caller / "explicit" / ".yield" / "profiles" / "automated-tests",
            ),
            (
                {"YIELD_CONFIG_PROFILE": "live"},
                ["--sandbox"],
                sandbox / ".yield" / "profiles" / "sandbox",
            ),
        ]
        for overrides, flags, app in cases:
            with self.subTest(app=app):
                environment = {"HOME": str(home), **overrides}
                doctor = self.run_command(self.doctor, *flags, **environment)
                logs = app / "logs"
                self.assertIn(f"Logs directory: {logs}\n", doctor.stdout)
                self.assertFalse(logs.exists())

                logs.mkdir(parents=True)
                (app / "config.toml").write_text('app_data_dir = "/obsolete"\n', encoding="utf-8")
                log = logs / "yield_0000000000000000_00000000_00000000_00000001.log"
                log.write_text("fixture log line\n", encoding="utf-8")
                latest = self.run_command(self.entry, "logs", *flags, "--lines", "1", **environment)
                self.assertIn(f"newest_log={log}\n", latest.stdout)
                self.assertTrue(latest.stdout.endswith("fixture log line\n"))


if __name__ == "__main__":
    unittest.main()
