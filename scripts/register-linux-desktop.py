#!/usr/bin/env python3
"""Register the extracted private Linux Yield package for the current desktop user."""

import argparse
import os
from pathlib import Path
import platform
import tempfile


APP_ID = "org.portalsurfer.yield"


def quoted_exec(binary: Path) -> str:
    path = str(binary)
    if not path.isascii() or not path.isprintable() or "%" in path or "=" in path:
        raise ValueError("choose an ASCII install path without control characters, % or =")
    # Apply Exec quoting first, then the desktop-entry string escaping layer.
    quoted = "".join(f"\\{character}" if character in '\\"$`' else character for character in path)
    return '"' + quoted.replace("\\", "\\\\") + '"'


def register_desktop(package: Path, data_home: Path) -> Path:
    package = package.resolve(strict=True)
    binary = package / "yield"
    icon = package / "assets" / "yield-brand-icon.png"
    if binary.is_symlink() or not binary.is_file() or not os.access(binary, os.X_OK):
        raise ValueError("expected the executable from the extracted Yield package")
    if icon.is_symlink() or not icon.is_file():
        raise ValueError("expected the extracted Yield package icon")
    executable = quoted_exec(binary)
    icon_value = str(icon).replace("\\", "\\\\")
    entry = (
        "[Desktop Entry]\n"
        "Type=Application\n"
        "Name=Yield\n"
        "Comment=Browse and audition audio samples\n"
        f"Exec={executable}\n"
        f"Icon={icon_value}\n"
        f"StartupWMClass={APP_ID}\n"
        "Terminal=false\n"
        "Categories=AudioVideo;Audio;\n"
    )

    applications = data_home / "applications"
    applications.mkdir(parents=True, exist_ok=True)
    destination = applications / f"{APP_ID}.desktop"
    # Publish a complete entry and preserve any existing registration.
    with tempfile.NamedTemporaryFile(
        mode="w", encoding="utf-8", prefix=".yield-desktop-", dir=applications
    ) as staged:
        staged.write(entry)
        staged.flush()
        os.link(staged.name, destination)
    return destination


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.parse_args()
    if platform.system() != "Linux" or platform.machine() != "x86_64":
        parser.error("desktop registration requires x86_64 Linux")
    data_home = Path(os.environ.get("XDG_DATA_HOME", ""))
    if not data_home.is_absolute():
        data_home = Path.home() / ".local" / "share"
    package = Path(__file__).resolve().parents[1]
    try:
        destination = register_desktop(package, data_home)
    except FileExistsError:
        parser.exit(1, "The Yield desktop entry already exists. Remove it before re-registering.\n")
    except (OSError, ValueError) as error:
        parser.exit(1, f"Yield desktop registration failed: {error}\n")
    print(destination)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
