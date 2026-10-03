# Complete update payload selection (OPT-2226)

The production `yield-platform::payload_selection::PayloadSelectionService`
retains a complete selection from the fixed release catalog and its anonymous,
build-specific gate. It consumes `ReleaseBuildIdentity::compiled()` directly.
Development builds without that complete supported context remain unavailable;
no UI candidate, runtime environment, installed version or endpoint argument can
fill it. The selected channel may change; installed target/build/commit cannot.

This implements the selection component of the
[staging contract](UPDATE_PAYLOAD_STAGING_CONTRACT.md). It retains catalog
declarations and private request metadata. It does not download a ZIP, verify a
signature, retain verified archive bytes, stage a payload, or install/relaunch.
Those production consumers remain OPT-2225/2227/2228. Native artifact and deployed
service acceptance remain OPT-2229/2230/2123; metadata fixtures do not qualify them.

## Identity and inventory

The snapshot retains full stable/RC/nightly version, channel, positive build
number, complete nonzero commit, exact build ID, compiled target, and every
current-contract asset's name/hash/positive bounded size/validated URL. The
complete inventory includes all three target ZIPs plus checksum and mandatory
checksum signature; only the installed target ZIP and its two checksum files
receive private token requests. Other target archives are not downloaded here.

The installed-line policy preserves the specific legacy 19.1.x to pre-1.0
transition and filters historical 19.1.x before selecting a newer-line build.
Current requires the exact full version/number/commit. Newer requires a higher
number and different full commit, ordinary full-version precedence, and the
existing narrow renumbering exception. Incomplete highest candidates, duplicate
identities, conflicting assets/URL aliases and ordinary rollbacks cannot fall
back to a different release. Legacy identity-less nightly metadata cannot become
Current or mask a higher complete build by supplying the installed version.

Publisher nightly dates use YYYY-MM-DD; compiled identity now accepts that
existing publisher spelling only when it equals the validated compiled build
date and the exact commit prefix. Existing compact date identities retain their
original full version. No artifact version is relabeled.

## Network and lifetime

Production metadata requests use ordinary validated rustls roots at
`https://portalsurfer.org:443`, with no redirects, retries, cookies, caller origin,
TLS/key override or private publisher credential. The gate request uses the exact
encoded build ID and `donation_amount=0.00`. Its response must name that build and
provide a nonempty token of at most 4 KiB. Request URLs contain exactly that token,
encoded once after removing incoming token fields, and are limited to 20 KiB.
Tokens/URLs have no public accessor, Debug or Display implementation.

The raw URL spelling is checked before URL normalization: reject namespace
escapes, dot segments, encoded separators/backslashes/recursive decoding,
userinfo, fragments, alternate origins and ambiguous token query names. Both
declared URL alternatives and distinct-file aliases are checked. Metadata changes
cannot update the original private snapshot; `matches_catalog` only compares
fresh observations and cannot issue, revive or replace it.

A preallocation guard wraps the validated plaintext stream before the HTTP
client's parser. It enforces 32 KiB combined status/header/trailer metadata,
100 fields, 8 KiB lines, 16 MiB framing, and actual 512 KiB decoded metadata
bodies. Conflicting framing/encoding, incomplete bodies/chunks and unsafe trailers
fail closed. JSON parsing bounds depth/text and rejects a 513th catalog row or
17th file before growing those collections. Retained text/list allocations use
field limits and fallible reservations. The fixed small parser/guard/TLS and
selection buffers are separate from the future archive allocation ledger.

The existing ureq decoder supports chunk extensions but rejects response trailer
fields. The guard bounds trailer bytes before that rejection; this selector does
not relax the decoder or silently accept incomplete final chunk framing.

Selections are non-Clone, non-serializable and privately constructed. Their weak
original factory identity, monotonic request generation and non-resettable
cancellation backing must remain live. A new accepted request, cancellation,
retirement or factory Drop invalidates prior issuance. Copied identity/inventory
observations confer no request or staging authority.

The metadata worker holds its per-factory admission until actual I/O stops.
Caller cancellation or its ten-second deadline cannot release a blocked worker's
slot. This is metadata admission only: OPT-2227 must still implement the single
application-wide slot spanning selection, verification, retained ZIP and confirmed
off-UI retirement across facade replacements. Closing this source scope does not
complete that owner or any updater parent.

## Verification

Private unit fixtures exercise the actual loopback HTTP reader, production
selector and gate construction for every lane/target. They cover complete and
changed inventories, legacy policy, retired/foreign issuers, cancellation while
I/O is blocked, actual metadata/framing limits, token/path substitution and
malformed bodies. Loopback transport/context substitution is absent from normal
builds; test records are declarations, not genuine signed release artifacts.

Run full locked `yield-platform` and `yield-core` unit suites, selection privacy
doctests, affected UI updater checks and `bash scripts/agent.sh preflight`.
Exact tested/merged SHAs, commands, counts and limitations are recorded on
[OPT-2226](https://linear.app/boostnlvp/issue/OPT-2226).
