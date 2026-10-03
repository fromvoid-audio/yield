# Verified retained update bytes (OPT-2225)

`SelectedPayload::verify_retained` implements actual signed-checksum, ZIP and
manifest verification in `yield-platform`. Call it on the background worker,
with the original live selection and the three downloaded bodies. It uses the
source-proven Ed25519 pin from
[the staging contract](UPDATE_PAYLOAD_STAGING_CONTRACT.md), never a caller,
catalog or environment key. Private test-key construction is compiled out of
normal builds. The internal owner entry also accepts the already established
whole-attempt deadline so HTTP and CPU work cannot reset it (OPT-2227).

The result is a privately constructed, non-Clone, non-serializable, non-Send and
non-Sync `VerifiedPayload`. It retains the exact original ZIP allocation without
copying or boxing it into another archive-sized buffer. Tokens, mutable bytes,
paths, extraction and installation are not exposed. Observed hash, length, full
identity and authenticated build date remain conditional on the original
selection's issuer, generation and cancellation. Worker ownership prevents moving
the retained allocation to the UI for disposal; the actual application-wide
staging/retirement owner remains OPT-2227.

## Authentication

Check actual length/hash of checksum and encoded signature bodies against the
original selected catalog declarations. Decode strict canonical base64 and
verify Ed25519 strictly against the **raw** checksum bytes; no whitespace or line
normalization precedes signature verification. Require exactly the three contract
ZIP entries with 64 hex digits, two spaces and the exact name, unique and complete.
LF and CRLF line endings preserve their signed spelling. Every digest must match
the complete catalog inventory; other target ZIPs are not downloaded here.

Hash the actual original target ZIP in bounded chunks; require its exact declared
size and agreement with both signed checksum and catalog. No source field or
private verified witness can be assembled from a standalone boolean/hash/manifest.

## Strict archive and manifest

Validate the raw ZIP/ZIP64 end records, central range, minimum record bytes and
metadata/count limits before allocating a bounded index. Read central/local
headers and optional descriptors with checked slices/arithmetic. Enforce
single-disk records, supported stored/raw
deflate compression, central/local version/time/flags/name/CRC/size agreement and
every local range. Explicitly account for each byte of the member area once;
reject overlap, hidden/unindexed members, unexplained gaps, self-extracting
prefixes and trailing data. Preserve ZIP64 locals/end records and both descriptor
spellings, including an unsigned descriptor whose CRC equals the optional marker.

Every member must have an unambiguous UTF-8 name/type beneath the exact `yield`
root. Reject absolute/traversal/dot/empty/backslash/colon paths, special members,
symlinks, file/directory collisions and duplicate names. The portable filename
profile also rejects reserved device names, trailing dots/spaces, forbidden or
default-ignorable characters, and components over 255 UTF-8 bytes. Compare
decomposed full case-folded names, including implicit parent directories; distinct
case/Unicode spellings cannot merge a tree. Ordinary Unicode names remain valid.
This is source validation; platform artifact qualification still verifies the
actual native producer output, including resource-fork/code-signing conventions.

Declare-check entry/aggregate expansion and ratio limits before inflating. Read
in 64 KiB chunks into a discard sink, enforcing actual expanded length and CRC for
every file/directory. Raw deflate must consume the exact compressed range and
terminate cleanly. Only the bounded manifest bytes are retained while checking.

Require exactly `yield/update-manifest.json`, current contract target-specific
required files, exact canonical file inventory, app/channel/target/platform/arch,
full stable/RC/nightly version, selected version core and complete commit. Reject
duplicate known JSON fields and bounded-list/text/depth violations. Authenticate
the manifest's calendar build date; do not derive an expected date from the
installed app or publication time. The manifest does not attest the native
binary's compiled build number/date, OS signature or notarization.

## Resource and qualification boundaries

Reject overallocated input capacities before retaining them: archive 1 GiB,
checksum 1 MiB, encoded signature 8 KiB. Archive metadata is limited to 16 MiB and
10,000 entries; paths 1,024 bytes/32 components; manifest 1 MiB/depth 16; expanded
entry 512 MiB, total 2 GiB, ratio 200. Hash/decompression operations check original
lease/cancellation/deadline between bounded steps and before acceptance.

The 64 MiB working ledger reserves a conservative 8 MiB for bounded selection,
manifest raw/decoded/list/sort references, crypto/inflater/normalizer/proof work,
then charges actual checksum/signature capacities and index/name capacities before
growth. Normalized-name growth is fallible and separately bounded. Borrow central
names from the original ZIP rather than allocating duplicate paths. Expanded
contents and working index/manifest state are discarded before returning the
verified witness. No updater-created files or extracted output exist.

The real packer tests run in disposable Linux fixture directories for all three
lanes and targets, with controlled non-native binaries/icons and private test
keys. Tests exercise actual cryptography/CRC/ZIP bytes, tampered and signed-but-wrong
metadata, ZIP64/descriptors, allocation/metadata/manifest limits, target name
collisions, retained allocation equality and untouched installed/profile/source
sentinels. Production rejects those fixture signatures using its actual pin.

These tests complete the verifier component, not HTTP staging, application-wide
reservation/retirement (OPT-2227), GPUI integration (OPT-2228), deployed routes
(OPT-2123) or genuine production artifact/native preservation (OPT-1769/2229/2230).
Tracking parents OPT-1776/1498 remain open. Run locked full platform tests,
selection/verified privacy doctests, affected updater UI tests and canonical
`bash scripts/agent.sh preflight`; exact results/SHAs are recorded on
[OPT-2225](https://linear.app/boostnlvp/issue/OPT-2225).
