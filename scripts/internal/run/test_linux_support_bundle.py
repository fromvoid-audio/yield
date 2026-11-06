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
    def test_archive_contains_only_allowlisted_status(self):
        scripts = Path(__file__).resolve().parents[2]
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            install = root / "installed Yield"
            entry = install / "scripts" / "run.sh"
            internal = entry.parent / "internal" / "run"
            internal.mkdir(parents=True)
            shutil.copy2(scripts / "run.sh", entry)
            shutil.copy2(scripts / "internal" / "run" / "bug_bundle.sh", internal)

            tools = root / "tools"
            tools.mkdir()
            uname = tools / "uname"
            uname.write_text(
                '#!/bin/sh\ncase "$1" in\n'
                '  -s) echo Linux ;;\n  -m) echo x86_64 ;;\n'
                '  *) echo PRIVATE_HOSTNAME ;;\nesac\n',
                encoding="utf-8",
            )
            uname.chmod(0o700)
            self.assertIsNotNone(shutil.which("zip"))

            config_base = root / "PRIVATE_ACCOUNT"
            app = config_base / ".yield"
            logs = app / "logs"
            logs.mkdir(parents=True)
            (app / "config.toml").write_text('token = "PRIVATE_TOKEN"\n', encoding="utf-8")
            for suffix in ("00000001", "00000002"):
                (logs / f"yield_0000000000000000_00000000_00000000_{suffix}.log").write_text(
                    "Authorization: Bearer PRIVATE_AUTH_HEADER\n", encoding="utf-8"
                )
            (logs / "yield_PRIVATE_FILENAME.log").write_text("PRIVATE_LOG", encoding="utf-8")

            caller = root / "support caller"
            caller.mkdir()
            result = subprocess.run(
                [entry, "bug-bundle", "--out-dir", "chosen output", "--logs", "1"],
                cwd=caller,
                env={
                    "PATH": f"{tools}{os.pathsep}{os.defpath}",
                    "YIELD_CONFIG_HOME": str(config_base),
                    "ZIPOPT": "-m",
                },
                capture_output=True,
                text=True,
                check=True,
                timeout=5,
            )
            (archive,) = (caller / "chosen output").glob("yield-bug-bundle-*/support.zip")
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


if __name__ == "__main__":
    unittest.main()
