# Source-derived layout intent data (OPT-2231)

`operation_journal::source_layout_commands::SourceLayoutIntentArgs::from_prepared`
produces passive input from the same genuine `SourceWriter`, private prepared
canonical generation and original layout metadata budget. It checks the issuer,
full source revision, profile acquisition identity, retained root/database names,
complete generation and current allocation facts before and after copying.
Cancellation and the budget's original deadline remain effective throughout.
It writes no source/control row, renews no lease and allocates no operation ID.

Private fields retain participant and source-command UUIDs, historical profile,
source/root/root-binding/database/lifecycle/lease/revision, exact prior generation
byte length/digest, measured page/database/WAL/SHM observations and the checked
finite peak allocation calculation. These facts are neither capacity claims nor
SQL publication permission. Decoding never reconstructs live ownership.

## Binary version 1

The fixed order is `YSLI`, little-endian u64 version, three 16-byte UUIDs
(participant, command, profile), four u16-length fields (UTF-8 source, raw root,
UTF-8 root binding, UTF-8 database identity), eleven little-endian u64 fields
(lifecycle, lease, revision, page size, page count, resident database bytes,
maximum pages, resident WAL bytes, resident SHM bytes, prior generation bytes,
required peak bytes), a 0/1 prior-digest flag and optional 32-byte digest,
u32-length complete canonical generation, then a 32-byte domain-separated BLAKE3
checksum over all preceding bytes. The domain is `yield.source-layout-intent.v1`
followed by one NUL byte. No optional supported fields or trailing bytes exist.

Field limits are 256/4096/512/512 bytes; generations cap at 524288 bytes.
The largest header is 5601 bytes including the optional prior digest and
checksum, below the reserved 8192 bytes. The total input cap is 532480 bytes.
Length checks precede each allocation; canonical generation decoding is bounded.
The supported decoder verifies checksum, identities, generation scope and exact
checked allocation arithmetic, and requires exact canonical re-encoding.
Temporary validation/re-encoding allocations are each bounded by these limits.
Bounded unknown positive versions remain opaque, preserving their exact bytes;
they cannot authorize an operation or a version-1 interpretation.

A future binary or base64 journal embedding avoids decimal JSON byte-array
amplification. Base64 of the complete 532480-byte maximum needs 709976 bytes;
with an illustrative 65536-byte framing budget this totals 775512 bytes, below
the existing 1048576-byte record cap. This is headroom arithmetic, not a completed
journal transport. Its actual full-progress encoder must bound and validate the
complete final record, including all other participant/progress data.

## Verification and remaining scope

Fresh bounded subprocesses use real profile SourceWriters, SQLite, published
retained features and the actual private canonical generation producer. Tests
cover read-only source/control behavior, original budget expiry/cancellation,
changed prior generation/page policy/revision, foreign/handoff/replaced owners,
256-member generation encoding, malformed/checksummed invalid fields, all
truncations, trailing bytes, bounded opaque future versions and actual source
close/reopen. A test-owned prior-generation row and a persisted binary fixture
provide observations only; they do not exercise admitted publication or journal
recovery. Synthetic computed coordinates do not qualify the layout algorithm.

OPT-2154 remains open for the actual versioned source-only journal family,
planner, full-progress store/scanner, queued owner and recovery route. OPT-2136
and OPT-2155 still require real pre-intent admission and atomic publication.
This input codec does not complete ANN, undo, filesystem effects or physical
reserve/platform qualification.
