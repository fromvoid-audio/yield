# Signed update payload custody (OPT-2177)

This decision supplies the missing portable contract for
[OPT-1776](https://linear.app/boostnlvp/issue/OPT-1776). It does not implement a
downloader, qualify an artifact, or complete update/install acceptance.

## Decision and current source

The first production staging owner retains one verified ZIP in private process
memory. It never creates or extracts a staging directory, receives an installed
app/profile/source pathname, or invokes an install, swap, cleanup or relaunch
operation. Successful staging preserves the exact verified bytes while the
owner remains live. Restart loses this volatile state and starts unstaged.
Extraction and durable staging require separate admitted filesystem ownership;
they cannot be inferred from an archive handle.

The existing [catalog](../crates/yield-platform/src/update_catalog.rs) reports
identity only. Its `Candidate` is not a download or signature-verification grant.
The [production adapter](../crates/yield-ui/src/update_catalog_service.rs) checks
complete versions before projecting the current core-only UI result. That
projection must not be used to reconstruct an RC/nightly selection. The actual
[browser worker](../crates/yield-ui/src/browser/update_check.rs) already supplies
cancellation, service replacement, lane, generation and panel-lifetime fences.

[ReleaseBuildIdentity](../crates/yield-core/src/release_identity.rs),
[release_contract.toml](../release_contract.toml), the
[strict release verifier](../scripts/internal/release/verify_published_release.py)
and [publisher contract](PUBLISHER_COMMIT_IDENTITY.md) supply the authoritative
identity, asset layout and verification vocabulary. The archived
[checksum](../reference/radiant/src/updater/archive/checksums.rs),
[download](../reference/radiant/src/updater/archive/download.rs) and
[extraction](../reference/radiant/src/updater/archive/unzip.rs) helpers are
reference evidence: first-match checksum parsing and silently skipping unsafe
members must not enter the new implementation. Their pathname extraction and
temporary directory are not the new owner.

## Selected identity and authority

Introduce a private payload-selection snapshot, constructed by the production
payload owner from bounded catalog bytes and immutable compiled installed
context. Its fields include the full release version, canonical lane, contract
target/platform/architecture, positive u64 build number, nonzero full 20-byte
commit, bounded build ID, and the exact declared file inventory with names,
positive bounded sizes, 32-byte SHA-256 hashes and validated download URLs.

Use the same installed-line policy as
[OPT-2201](https://linear.app/boostnlvp/issue/OPT-2201); preserve its ordinary
rollback checks and narrowly scoped renumbering. A request cannot substitute
installed version, build, commit or target to obtain download admission.
Scalar `UpdateCheckOutcome::Candidate` remains an observation, not this private
snapshot. The future consumer must receive the actual full selection, either
through a private retained bridge or a fresh selection that is presented and
bound before download. It must never manufacture a full version from the UI core.

The snapshot has a private issuer/service lifetime, request generation and
cancellation identity. It cannot be deserialized, constructed by a UI caller,
or reused after selection/lane/service retirement. Metadata changing after
selection rejects the attempt; no fallback to another asset or release.

Every selected asset name comes from the current release contract. Download only
the chosen target ZIP plus its release-wide checksum and checksum signature.
Require the complete contract checksum inventory, including the other target ZIP
names, without claiming those other archives were downloaded or verified here.
Duplicate, unexpected, missing or conflicting file identity rejects selection.
The signature is mandatory for staging, even though publisher identity preflight
can upload an inventory without independently qualifying signing.

## Trust and network protocol

The compiled Ed25519 public key is
`8Z7dQJBRMbxCFkFMeBYa1FMSWOUm6nePFgoK5c43jT4=`. Its provenance is the existing
[verification commands](ENV_VARS.md), disabled stable/RC/nightly workflow commands
and [archived pin](../reference/radiant/src/updater/mod.rs). It is a nonsecret
source-controlled trust anchor, not a missing resource. Rotation requires a
reviewed code change and genuine artifact qualification. No catalog key,
environment key, trust-on-first-use or private upload credential is accepted.

Production uses HTTPS `portalsurfer.org:443`. Fetch the existing catalog at
`/yield/api/v1/releases`; obtain its existing anonymous build-specific gate at
`/yield/api/v1/releases/{encoded-build-id}/gate?donation_amount=0.00`. Require the
gate's exact build ID and a bounded nonempty download token. This follows the
actual release verifier; it is not an invented endpoint or account flow.

Use only URLs from the selected file records (`url`, `download_url` or `href`);
conflicting alternatives reject. Resolve relative URLs against the fixed catalog
origin. Require the exact HTTPS origin, no userinfo/fragment, and a `/yield/`
path after validated resolution. Reject dot segments, encoded separators,
backslashes and decoding/normalization escapes from that namespace before adding
the gate token. Never accept a runtime base URL or caller-supplied URL. Replace any incoming
`download_token` query field with the gate token, encode it once, and retain it
only for this attempt. Never log tokens or token-bearing URLs. The verifier's
private verification/upload proof must not become an application credential.

Allow no redirects, retries, cookies, range/resume or alternate-host fallback.
Require HTTP 200. Request identity content encoding and reject other encodings
before reading bodies so actual transfer bounds do not depend on decompression.
Use normal TLS validation. A default-off loopback fixture seam may substitute
transport in tests; production has no URL/TLS/key override. The existing Python
verifier's unbounded network reads are not a production transport implementation.

## Finite resource policy

All arithmetic is checked. These are v1 ceilings, not runtime configuration or
proof that every native artifact fits. Enforce actual stream bytes and actual
owned capacities, not only declared lengths. Reject before allocating beyond a
ceiling. Use fallible allocation, bounded parsing/visitors and fixed error codes.

| Resource | Ceiling |
| --- | --- |
| Catalog and gate bodies | 512 KiB each; depth 16; individual text 16 KiB |
| Catalog rows / release files | 512 / 16; build ID and asset name 160 bytes |
| Selected full version / gate token / asset URL | 100 bytes / 4 KiB / 4 KiB |
| Final token-bearing request URL | 20 KiB, including percent-encoding expansion |
| Response status/header/trailer metadata | 32 KiB combined; 100 fields; 8 KiB per field |
| Chunk/framing metadata | 16 MiB per response; 8 KiB per line |
| Checksum bytes / encoded signature | 1 MiB / 8 KiB |
| Selected compressed archive | 1 GiB; exact selected size and hash |
| Aggregate network body bytes | 1 GiB + 2 MiB + 8 KiB |
| Aggregate inbound HTTP plaintext | Body ceiling + five framing/metadata ceilings: 1 GiB + 82 MiB + 168 KiB |
| Archive metadata before parser allocation | 16 MiB; 10,000 entries |
| Member path / component depth | 1,024 UTF-8 bytes / 32 |
| Manifest bytes | 1 MiB, depth 16, bounded strings and file list |
| Expanded entry / total expanded bytes | 512 MiB / 2 GiB |
| Expansion ratio | 200; nonempty output cannot have zero compressed size |
| Verification read/decompression chunk | 64 KiB |
| Retained archive allocation | 1 GiB maximum actual capacity, one buffer |
| Additional owned working allocations | 64 MiB, including parser/index/metadata |
| Updater-created files / extracted output | 0 / 0 |
| Whole attempt deadline / connect / body-read wait | 300 s / 10 s / 10 s |
| Concurrent active or staged payloads | 1 per application, across service facades |

Enforce response metadata limits before an allocating HTTP parser accepts input;
body limits alone cannot bound headers, trailers or chunk extensions. Credit
actual framing bytes as well as body bytes, and reject conflicting length/framing
or content-encoding metadata. The plaintext ceiling excludes TLS/TCP framing;
their bounded implementation buffers remain within the working-allocation ledger.

The archive/expanded/count/ratio ceilings retain the established reference
limits. The new metadata/working-memory ceilings close parser and custody gaps.
The whole deadline also bounds CPU verification; check it and cancellation
between bounded operations. Do not materialize expanded contents: verify into a
discard sink, retaining only the bounded manifest/index and original ZIP bytes.
Validate central-directory/ZIP64 lengths, counts and offsets within these limits
before constructing an allocating ZIP parser. Preserve supported ZIP semantics;
this decision does not rewrite the payload format.

Normalize or reject overallocated inputs before retaining them, and credit actual
capacity. Avoid a second archive-sized copy while freezing verified bytes. The
buffer remains privately owned and becomes immutable by API encapsulation; UI
handles do not clone its bytes. Working buffers are retired before Staged, with
bounded snapshot/proof overhead credited to the same owner. There is no updater
filesystem output; this does not claim control over OS paging or networking.

## Signature, checksum and archive verification

Verify the Ed25519 signature against the exact raw checksum bytes with the compiled
pin and strict signature validation. Decode a 32-byte key and 64-byte signature;
reject malformed encoding, noncanonical/malleable signatures and wrong keys.
Then parse the checksum grammar against the exact contract ZIP inventory. Reject
duplicate/conflicting/unexpected names, missing entries and malformed digests;
do not select the first matching line or normalize bytes before signature checks.

Hash the actual retained target ZIP and require agreement with both the signed
checksum entry and selected catalog hash/size. Verify every archive member and
actual decompressed length/CRC within the limits, including data descriptors and
central/local header agreement. Reject encrypted or unsupported compression,
duplicate or overlapping member ranges, path traversal/absolute paths/NULs/
backslashes/ambiguous separators, symlinks and special/inconsistent member types,
file/directory collisions, and target filesystem case/Unicode/name collisions.
Unsafe members reject the entire archive; they are never skipped or extracted.

Require exactly `yield/update-manifest.json`, the current contract root and all
target-specific required files. The manifest's canonical file list must match the
actual archive entries without duplicate or normalized aliases. Require `app`
`yield`, exact channel/target/platform/architecture/full version/target version,
and complete commit agreement with the private selection. The existing
`target_version` is the selected full version's `major.minor.patch` core, as the
[packer](../scripts/internal/release/build_release_zip.sh) requires. It is not the
installed version or a substitute for the selected full RC/nightly version.
Validate its build date
as a calendar date and retain that authenticated claim; do not invent an expected
date from publication time or the installed app. The current manifest lacks a
compiled build-number attestation: matching catalog number and archive hash does
not prove the native binary's compiled number. Native artifact qualification
retains compiled number/date/signature/notarization reconciliation.

Only after all checks may the actual owner construct a private verified-payload
witness tied to the same immutable byte buffer and full selection. A standalone
checksum, manifest, catalog candidate or boolean cannot construct that witness.
Ed25519 payload authenticity does not establish OS code signing or notarization.

## Worker, cancellation and lifecycle

The payload owner holds one admission slot from selection through all five bounded
requests (catalog, gate, checksum, signature, ZIP), verification and any retained
Staged payload. It owns network state, buffers, full selection and verified witness.
The GPUI executor performs none of this work and does not deallocate a large buffer.
Retain application-scoped admission across UI/checker facade replacements; a
replacement cannot create an independent gate or worker while old custody remains.

Cancellation/deadline can return promptly to the caller while an OS resolver or
network operation still runs. Keep the slot and all resources until the actual
worker has stopped, discarded unaccepted bytes and confirmed retirement. No timer,
channel closure, caller drop or copied cancellation flag unlocks an unknown worker.
Repeated requests cannot accumulate detached workers or duplicate archives.
Check cancellation before I/O, allocation and verification, and immediately before
publishing Staged. Failed delivery/stale context retires the bytes, not a result.

Staged retains the private bytes and reservation until an explicit bounded retire
control is acknowledged on the worker, service retirement or process exit. A new
download cannot overlap it. Channel/service replacement and closing its UI retire
the old attempt and payload through that same owner. Application restart presents
Unstaged; no pathname discovery or synthetic recovery of volatile bytes. Future
extraction must separately revalidate its selection, receive newly admitted effect
authority and establish its own custody/recovery protocol.

## UI truth and acceptance

The active consumer retains request generation, strong service identity, immutable
installed context, selected lane/target/full release and cancellation identity.
Publish only if all still match and the panel is live. Preserve existing close,
replacement, retry and draining fences. Display the actual full RC/nightly version.

| Observation | Honest result |
| --- | --- |
| Missing compiled context, unsupported target, no signed matching selection | Unavailable, no bytes staged |
| Connection unavailable | Offline, no bytes staged |
| Corruption, identity substitution, malformed input or budget violation | Failed, no accepted payload |
| Caller cancelled/timed out while worker still holds custody | Stopping/Unconfirmed; admission remains held |
| Verified immutable bytes retained by live owner | Staged; installation and relaunch unavailable |
| Confirmed retirement or restart | Unstaged |

Staged is not Installed, Committed, native-qualified or ready for relaunch. The
[target updater policy](TARGET.md#updater-policy) commits an install only after all
destination swaps; Windows installation remains manual. No Stage state invokes
that boundary. Do not expose budget/transport implementation details in product
controls; explain retained lifetime and unavailable installation where meaningful.

## Production leaves and qualification

Implement these bounded leaves after this decision; retain OPT-1776/1498 and
native/private-artifact parents until their original acceptance is satisfied:

1. [OPT-2225](https://linear.app/boostnlvp/issue/OPT-2225) — actual Rust verification: strict signed checksum/ZIP/manifest verifier with the
   compiled pin, private immutable byte custody and real tampering/bounds tests.
2. [OPT-2226](https://linear.app/boostnlvp/issue/OPT-2226) — actual complete selection: bounded private catalog/asset descriptor parsing,
   compiled identity/OPT-2201 policy and exact fixed-origin/gate contract.
3. [OPT-2227](https://linear.app/boostnlvp/issue/OPT-2227) — actual transport/staging owner: finite HTTP and allocation ledger, single slot,
   genuine verification, retained bytes and acknowledged cancellation/retirement.
4. [OPT-2228](https://linear.app/boostnlvp/issue/OPT-2228) — actual GPUI consumer: full-selection bridge, background dispatch/progress,
   generation/service/lane/close fences and honest Staged/Stopping/Unavailable.
5. [OPT-2229](https://linear.app/boostnlvp/issue/OPT-2229) (macOS) and
   [OPT-2230](https://linear.app/boostnlvp/issue/OPT-2230) (Windows) — native preservation evidence: genuine qualified macOS and Windows archives,
   real trust pin and matching compiled identity, controlled native app/installed
   bundle preservation. This is separate from install/relaunch acceptance.

Tests must use the real packer grammar, real Ed25519/hash/CRC algorithms and actual
HTTP/owner/UI paths, including corrupt and signed-but-mismatched archives, unsafe
members, unknown-length/oversized bodies, short bodies, blocked I/O, late completion,
overallocated input, all lanes/targets and retained-byte equality. Sentinel installed
app/profile/source data must remain unchanged by staging. Test signing keys and
loopback transport belong only to explicit default-off/private test seams; valid
fixture signatures cannot qualify the production key or native artifacts.

The exact nonsecret key and executable verification/owner/UI work are available
here. Genuine signed artifacts and native runtime acceptance remain
[OPT-1769](https://linear.app/boostnlvp/issue/OPT-1769) and its platform children.
Actual deployed catalog/gate/download support and public roundtrip remain
[OPT-2123](https://linear.app/boostnlvp/issue/OPT-2123): this executor received enforced
CONNECT 403 for `portalsurfer.org` and has no deployment/upload resource. Those
resources do not block portable implementation or its controlled HTTP fixtures.
The [filesystem boundary](FILESYSTEM_PRODUCER_BOUNDARY.md) remains mandatory;
an inventory exemption cannot supply extraction, installation or cleanup authority.
