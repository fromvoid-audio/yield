# Packaged commit identity in PortalSurfer publication

`publish_portalsurfer_release.sh` derives the authoritative full nonzero 40-hex
commit from actual ZIP manifests. `GITHUB_SHA` and abbreviated build identifiers
cannot supply or extend that identity. All release-contract ZIPs and the combined
checksum file must be present; a declared checksum signature may also be present.
Per-target `checksums-entry.txt` intermediates must be collected into the combined
release checksum file before publication.

Before any HTTP PUT, the publisher copies selected files into its private temporary
snapshot, validates every archive with the existing release manifest/layout validator,
checks exact target/platform/architecture/channel/full version/target version/commit/
build date, and verifies the combined SHA-256 inventory against those snapshot bytes.
Every target must agree. Invalid, missing, unexpected, duplicate or substituted
identity fails before staging. Stable/RC/nightly versions follow immutable build
identity rules, including valid nightly calendar date and matching eight-hex prefix.
The supplied catalog build number must be a positive u64; it remains publication
metadata, not a new attestation of the native binary's compiled build number.

Uploads and summaries use the validated snapshot. Original input files changing
later cannot change the bytes uploaded or the recorded hashes. The snapshot is
removed on exit. No signing validation or native artifact qualification is added
by this identity preflight; those existing release gates remain independent.

## Server extension required by OPT-2123

Every staging and commit PUT includes `X-Yield-Commit` containing the exact packaged
40-hex commit, alongside existing build-number/channel/version/released-at headers.
The commit JSON additionally includes `commit`. A staging response must contain
`staged: true`, the exact `build_id`, positive integer `build_number`, `channel`,
`version`, `commit`, `released_at`, and its `file` name/SHA-256/size. A commit response
must contain `committed: true` and a `release` object with those same identity fields
and the complete `files` inventory. The public catalog must contain exactly one
matching release with identical identity and file names/hashes/sizes. Existing
markdown changelog response/link/content checks also remain required.

Missing, shortened, substituted, duplicate, malformed or overflowing fields reject
publication success. Metadata responses are limited to 512 KiB; the identity-response
parser limits depth to 16, strings to 16 KiB, catalog releases to 512, and file inventories
at most 16. Every publisher metadata GET/PUT, including pre-commit catalog and historical
changelog downloads, uses the existing finite curl connect/total deadlines and retry
policy. The assembler receives bounded local files in this path; it performs no
additional network requests. Its standalone URL/local metadata reader also caps bytes.
A failing response after PUT may mean staged or committed server data exists; the
publisher reports failure and does not assert rollback or remove remote evidence.
An old server that omits these fields cannot produce successful publication.

## Linux verification and remaining gates

```sh
python3 scripts/internal/release/test_publisher_identity.py
python3 scripts/internal/release/test_release_metadata.py
python3 scripts/internal/release/test_release_integrity.py
bash -n scripts/internal/release/publish_portalsurfer_release.sh
bash scripts/agent.sh preflight
```

The publisher suite runs the actual packer with controlled placeholder payloads
under its existing skip-build mode, then the actual shell publisher and curl/Python
request paths against a loopback HTTP server using a fake local token. All declared
lanes/targets, pre-PUT mismatch/integrity/overflow rejection, malformed and substituted
server metadata, complete file identity, and changed-original/snapshot behavior are
covered. Unknown-length oversized staging and pre-commit HTTP responses must be
rejected by curl during transfer, before JSON parsing or a remote commit request.
These fixtures are not native binaries, signing, deployment or physical
qualification. Disabled workflows are not activated or edited.

OPT-2149 implements this portable client contract only. OPT-2123 retains actual server
support/deployment and public HTTPS roundtrip against genuine packaged artifacts;
OPT-2104/1498 and native tracking parents remain open. This executor's actual public
GET was rejected by enforced egress with CONNECT 403 for `portalsurfer.org`; no live
catalog contents, deployment credentials or publication success are inferred.
