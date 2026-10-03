# Bounded retained payload owner (OPT-2227)

`yield_platform::payload_selection::PayloadStagingService::compiled()` composes
full private catalog/gate selection and strict compiled-pin verification. Missing
compiled identity, including unsupported Linux development targets, stays
unavailable. There is no public transport, installed identity or key override.
The Linux tests use private unit-only loopback transport and signing fixtures;
they do not qualify genuine production artifacts or native installed binaries.

## Admission and custody

All production facades share one process-global engine and one standing serial
background thread. Admission covers selecting, downloading, verifying, Staged
and Stopping. A bounded one-job channel carries immutable compiled context and
private original lifetime/generation control, never archive bytes. Independent
facade replacement cannot obtain a second admission while the old operation
has unresolved I/O or a retained archive. Service clones share their lifetime.

The same worker performs HTTP, invokes the actual strict verifier and retains its
original immutable `VerifiedPayload` allocation. That type remains non-Send and
non-Sync. UI-facing handles contain only a small observation/control object;
cloning or dropping them cannot copy or deallocate the archive on the UI thread.
Staged observations report full identity, authenticated hash, length and manifest
date. They grant no filesystem, install, signing, binary-attestation or relaunch
authority. An observation authenticates against the original facade/generation;
the future GPUI consumer must also preserve its live panel and requested lane.

Cancellation, original whole-attempt deadline, last caller Drop and service
retirement request stopping. They never clear engine admission. Observations
recheck that original lifetime while holding the small state lock, so copied
Staged data cannot survive confirmed retirement. Only the serial worker, after
actual I/O and every working/retained buffer are dropped, records retirement and
releases admission. This is a resource-retirement acknowledgement, not a claim
that the standing worker thread exits after every attempt. A timer does not
manufacture acknowledgement of a blocked OS operation. A dead/inconsistent
engine fails closed rather than releasing uncertain custody.

## Transport and allocation bounds

A valid attempt makes exactly five requests: catalog, exact anonymous gate,
selected checksum, selected signature and selected target ZIP. Fresh non-pooled
agents prevent recycled-connection retries; redirects are disabled. The origin
is fixed HTTPS portalsurfer.org:443 with ordinary web PKI validation, no client
certificate, credentials, range/resume request or forwarded cookie. The strict
plaintext guard runs before the client's allocating HTTP/chunk parser, including
when other workspace crates enable the client's gzip feature.

The existing complete selection parser and guard enforce these contract bounds:

| Quantity | Ceiling |
| --- | --- |
| Each catalog/gate decoded body | 512 KiB |
| Checksum/signature/target archive actual length and capacity | 1 MiB / 8 KiB / 1 GiB, and the exact positive selected size |
| Combined response status/header/trailer metadata | 32 KiB, 100 fields, 8 KiB per line |
| Each response chunk framing | 16 MiB, 8 KiB per line |
| Actual aggregate decoded bodies | 1 GiB + 2 MiB + 8 KiB |
| Actual aggregate HTTP plaintext reads | 1 GiB + 82 MiB + 168 KiB |
| Original whole-attempt / connect / read/write wait | 300 s / 10 s / 10 s |
| Additional working allocations | 64 MiB |

There is no content decompression. Conflicting framing/length/encoding and
short, oversized or nonexact assets fail. Unknown-length and supported chunked
bodies use the same exact selected asset bounds. The pinned client rejects
trailer fields; the guard bounds them before rejection rather than claiming
support for a grammar the client cannot consume.

The phase ledger reserves a conservative 8 MiB credit for bounded selection,
HTTP/parser/TLS/URI/read work, then charges actual metadata/checksum/signature
capacities. Rustls's pinned implementation limits handshake messages to 64 KiB,
send buffers to 64 KiB and received plaintext to 16 KiB; the guarded metadata,
request-line rewrite and read chunk are independently bounded. Only one HTTP
reader/agent exists at a time. The catalog schema caps rows, file lists and
strings within a 512 KiB input. HTTP readers/agents and raw metadata are dropped
before verification, whose existing 64 MiB ledger uses the same conservative
phase credit and charges actual checksum/signature/index/name capacities.
The sole archive allocation is accounted separately and is not copied.

The client debug-logs URLs and request preludes. A private bounded bridge keeps
the token out of its URL: the agent sees the redacted URL, and the validated TLS
writer checks the complete expected GET line before writing the exact private
tokenized line once. No logging switch, endpoint change, TLS exception or public
secret accessor is needed. Tests capture actual client debug records and verify
the real wire has the once-encoded gate token while logs never receive it.

## Verification and remaining acceptance

Run `cargo test --locked -p yield-platform --lib` and
`cargo test --locked -p yield-platform --doc`, then repository
`bash scripts/agent.sh preflight`. Real HTTP tests compose the actual complete
selection, real Ed25519/SHA-256/CRC/ZIP verifier and retained owner. They cover all
nine lane/target combinations, exact original bytes, corrupt and signed-wrong
payloads, short/unknown/chunked bodies, actual metadata/framing/encoding/URL
attacks, aggregate accounting, blocked reads across cancellation/drop/facade
replacement, actual socket deadlines, off-UI retirement, real client logging
and installed/profile/source sentinel preservation. Aggregate ceiling tests
seed only a private test counter while a real socket read is blocked, then
prove the next actual network bytes are denied before parsing; they do not
allocate or transfer a gigabyte to test integer accounting.

OPT-2228 still owns the actual GPUI consumer. OPT-2229 and OPT-2230 remain open
for genuine compiled release identities, production-pinned signatures, macOS
signing/notarization and Windows signing, and native installed preservation.
OPT-1769 retains genuine artifact/signing-resource qualification. Tracking
OPT-1776 and OPT-1498 remain open. This source owner performs no staging-path
creation, extraction, install, cleanup or relaunch.
