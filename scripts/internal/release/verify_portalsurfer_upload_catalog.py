#!/usr/bin/env python3
"""Verify an exact packaged-identity PortalSurfer catalog round trip."""
from __future__ import annotations

import argparse
from pathlib import Path

from publisher_identity import read_json, verify_response


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--catalog-file", type=Path, required=True)
    parser.add_argument("--identity-file", type=Path, required=True)
    args = parser.parse_args()
    args.phase = "catalog"
    args.response_file = args.catalog_file
    try:
        verify_response(args)
        expected = read_json(args.identity_file)
        catalog = read_json(args.catalog_file)
        release = next(item for item in catalog["releases"] if item["build_id"] == expected["build_id"])
        changelog = release.get("changelog")
        if not isinstance(changelog, dict) or changelog.get("format") != "markdown" or not changelog.get("url"):
            raise ValueError("catalog missing markdown changelog link")
    except (ValueError, OSError, KeyError, TypeError, RecursionError) as error:
        parser.exit(1, f"Publisher catalog rejected: {error}\n")
    print(f"Verified {len(expected['files'])} release files and full packaged identity for {expected['build_id']}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
