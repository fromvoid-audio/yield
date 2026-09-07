#!/bin/sh
# Build an isolated, ad-hoc-signed macOS development application.
set -eu

if [ "$(uname -s)" != Darwin ]; then
    echo "This development bundle requires macOS." >&2
    exit 1
fi

repo_dir=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
manifest="$repo_dir/Cargo.toml"
cargo build --locked --manifest-path "$manifest" --bin wavecrate-gpui

python3 - "$manifest" <<'PY'
import json
from pathlib import Path
import plistlib
import shutil
import subprocess
import sys

metadata = json.loads(subprocess.check_output([
    'cargo', 'metadata', '--locked', '--no-deps', '--format-version', '1',
    '--manifest-path', sys.argv[1],
]))
target = Path(metadata['target_directory'])
bundle = target / 'bundle' / 'Wavecrate GPUI.app'
contents = bundle / 'Contents'
executable = contents / 'MacOS' / 'wavecrate-gpui'
executable.parent.mkdir(parents=True, exist_ok=True)
shutil.copy2(target / 'debug' / 'wavecrate-gpui', executable)
(contents / 'Info.plist').write_bytes(plistlib.dumps({
    'CFBundleExecutable': 'wavecrate-gpui',
    'CFBundleIdentifier': 'org.portalsurfer.wavecrate.gpui.dev',
    'CFBundleName': 'Wavecrate GPUI',
    'CFBundleDisplayName': 'Wavecrate GPUI',
    'CFBundlePackageType': 'APPL',
    'CFBundleShortVersionString': '0.1.0',
    'CFBundleVersion': '1',
    'NSHighResolutionCapable': True,
}))
subprocess.run(['codesign', '--force', '--sign', '-', str(bundle)], check=True)
subprocess.run(['codesign', '--verify', '--strict', str(bundle)], check=True)
print(bundle)
PY
