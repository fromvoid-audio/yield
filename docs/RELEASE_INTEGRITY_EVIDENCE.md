# Portable release integrity verification (OPT-2093)

Run from the repository root:

```sh
python3 scripts/internal/release/test_release_integrity.py
```

Python 3 and OpenSSL with Ed25519 support are required. Missing OpenSSL fails
the suite; it does not skip signature verification. The nine tests exercise
the production `validate_downloaded_assets` path using the current
`release_contract.toml`, archive layout and manifest checks.

The fixture generates disposable signing keys and placeholder archives for
every contracted target. Stable, RC and nightly valid signatures pass.
Tampered ZIP bytes, changed checksum bytes, wrong keys, invalid signature
encoding, invalid public keys, missing required assets and signed malformed
checksum manifests fail. Duplicate names (even identical hashes) and unexpected
asset names fail instead of silently accepting the last entry. Non-UTF-8
checksum/signature input returns a verification error.
The full offline CLI returns zero for valid assets and nonzero for signed
duplicate entries without changing the files or installed-app sentinel.

Each validation snapshots the downloaded files and installed-app sentinel and
asserts they remain unchanged. Fixtures perform no network request, extraction,
installation or relaunch. Temporary keys and fixture directories are removed
after each test. The production checksum assembler emits one entry per
contracted ZIP with SHA-256 and its exact basename; that format is preserved.

This is production verifier implementation evidence with synthetic signed
inputs. It does not qualify native binaries, a production signing identity,
GPUI download/staging wiring, installed-app preservation by an installer or
physical update/relaunch behavior. OPT-1776, OPT-1498, OPT-1769 and native
qualification tracking retain their original acceptance.
