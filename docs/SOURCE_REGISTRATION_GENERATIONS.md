# Original Source registration generations (OPT-2383)

`SourceDatabase::prepare_source_authority` issues a checked process-local serial
after validating the original profile, lease and native database binding and
reserving its singleton registration UUID. The immutable serial belongs to that
original request. Request aliases and genuine Registered custody retain it
unchanged. It is separate from the lease epoch, Source lifecycle and CurrentRound
revision; those numbers may coincide without identifying the same registration.

The issuing counter starts at one and only issues positive values within SQLite's
signed i64 range. Concurrent issuance uses an atomic checked update. After the last
valid value, exhaustion is sticky: no wrap, zero issuance or restart is possible.
Failed request construction may burn a serial. Exhaustion clears only the exact
attempted UUID reservation before returning an error, without issuing a guard or
changing Source SQLite rows. Existing descriptor-construction cleanup remains in
place. A successful or abandoned registration retains the existing singleton
lifetime; exhaustion cannot clear that marker or provide a replacement handshake.

The number is meaningful only alongside the original registration UUID, profile
acquisition, root/native binding and retained lease ownership. It is not durable
or monotonic across processes; a different process can reuse a numeric value with
a different UUID. Historical observations cannot reconstruct current ownership,
an ACK, a decision latch or permission to mutate a database.

The original lease preparation captures the serial separately from its stored
fence. Original binding validation compares it with the actual registration and,
when retained, genuine Registered custody. Transport observation equality includes
it. No Source registry DTO, stored fence encoding, schema or wire tag changes.
The original capture scratch remains prepaid; concrete binding/LeaseReport size
accounting includes the fixed field before scratch shrinks and report publication.
No additional payload, pool, worker or owned identity string is introduced.
AuthorityState and the RegisteredDecisionBoundary backing layout are unchanged;
the original runtime boundary ceiling still applies.

## Acceptance and remaining work

The regression exercises the actual Native Source request and Registered custody
before checking its generation. It fails on the missing producer using an explicit
test-only zero observation adapter. Implementation tests additionally cover last
storage-range issuance, sticky invalid/exhausted counters, concurrent uniqueness,
actual producer exhaustion cleanup and subsequent public same-lease preparation,
successful/abandoned singleton retention, request aliases and genuine custody.
The two-Source preparation test keeps equal lease epochs and lifecycle values
while observing distinct genuine registration UUIDs/serials, refuses altered,
foreign, zero and oversized generations, and verifies original settings, lease,
writer reuse and counted-custody cleanup.

Run `cargo test --locked -p yield-library --lib registration_generation -- --nocapture`
for these six cases, then affected Source preparation/original Owner acceptance,
full Library, Linux and Windows all-tests compilation, docs and stable committed
canonical preflight. Exact macOS and Windows/MSVC runtime remains
[OPT-2384](https://linear.app/boostnlvp/issue/OPT-2384); GNU compilation is not
native runtime evidence.

This completes only the original checked Source serial producer. The actual
bounded original inventory factory in OPT-2382 remains necessary. It does not
issue an inventory/header authority, qualify physical allocation, latch Start,
persist initial intent/claims, issue attempts/permits, retry, refund, release or
complete settlement. Full OPT-2194/2136/2168 and native/physical requirements remain
open.
