# Original all-owner WAL preparation

OPT-2263 adds asynchronous **pre-intent preparation and confirmed empty abort**
on the existing journal, global-library and source-catalog owner routes.
`OperationJournalClient::prepare_wal_owners` returns the sole cancellation handle.
The journal loop polls original replies without moving SQLite connections or
creating another production worker. It acquires global first, then source owners
in ascending physical database-identity order. The minimum original deadline
includes queue time and cannot exceed two seconds. Coverage is bounded to 64
distinct databases; that count does not guarantee that every payload fits.

Factories validate immutable command/participant/claim coverage. Universal
commands use the actual derived work plan. Tag commands retain the original
selection and compare every path without another selection buffer. Layout
commands compare their existing hashed participant domain to the original raw
layout scope, including the profile acquisition. The explicit lease factory
supports an original pre-audit lease scope without inventing an audited revision.
Unsupported command topology, missing or duplicate owners, conflicting families,
claim classes/devices and aliases refuse. Original native replies are checked
against command correlation, physical MAIN device and journal profile acquisition.

## Memory custody

One private 64 KiB budget covers coordinator backing, queued original inputs,
native capture and validation scratch, descriptor backing and retained history.
Original source/global local limits remain additional restrictions. Allocation
credits are charged before allocation, use checked arithmetic, and follow the
actual last data holder. They only account for memory; releasing one does not
refund an operation's capacity reservation.

The original journal ordinary-input pool debits a full 64 KiB before enqueueing
the handshake. Its sole debit is shared with the historical inventory and remains
charged after cleanup or journal shutdown until the last inventory copy drops.
There is no detached second journal allowance. Original source and global scope
copies also retain their original local and combined debits.

Universal factory validation funds complete topology vectors, growth/sort backing,
cloned fence strings and bounded encodings before deriving the work plan. Layout's
binary-generation validation/base64 scratch is funded before serialization. Tag
validation funds its canonical encoding and original passive SQL capture. Lease
validation funds the stored bounded fence encoding and future vector backing.
Large original contexts can legitimately return `Full`.

Source capture phases are sequential: audit decoding holds at most 24 KiB SQL
text and 16 KiB decoded backing; registered capture holds bounded scalar subjects
and one 16 KiB encoding. Revision copies are charged separately. Current schema,
runtime and tag membership comparisons borrow SQLite values. Parked source checks
reserve 12 KiB for bounded CONTROL lease fields, native identities and SQL headers.
Global native capture initially reserves its existing 16 KiB ceiling and then
retains the actual bounded descriptor/history and parked-validation backing.

Linux SHM mapping verification scans the complete bounded `/proc/self/maps` stream
with fixed 512-byte input and 256-byte identity-prefix backing. It preserves
duplicate mapping, permissions, range, offset, inode/device, deadline and total
input checks without a two-megabyte temporary vector. Pathnames do not determine
mapping identity.

Lease cleanup funds its original full validation capture. If a destructor cannot
fund it, the private consuming close closes the actual connection before its lease
and extension gate. This path returns no writer and publishes no rollback or
retirement acknowledgement.

## Outcomes and limits

`Ready` contains historical scopes only. The handle's `abort` and its destructor
request actual reverse-order cleanup. `Aborting` remains visible while original
owners are cleaning up. `Aborted` or `Rejected` requires confirmed empty cleanup
from every acquired owner. Unconfirmed owner loss publishes `Unavailable`, fences
another handshake and prevents clean journal handoff/shutdown. Pending global
cancellation continues observing the original worker's actual cleanup result.

Preparation creates no journal operation identity or intent row. It does not issue
required permits, qualify physical capacity, renew a deadline or lease, authorize
retry, refund capacity, release an admitted source, or settle an operation.
Prepared source registrations and copied history cannot become current Registered
acknowledgements or a common decision latch. The pre-intent cleanup rules must not
be reused after genuine settlement starts.

OPT-2136 remains open for qualified per-direction claims, current Registered
all-owner acknowledgements, the sealed decision, atomic nonce/intent/claim
installation, post-settlement cleanup fencing, uncertainty and restart/readback.
OPT-2194 and required issuer issues OPT-2168/2169/2170/2171 remain open. See
[WAL_ATTEMPT_LEDGER.md](WAL_ATTEMPT_LEDGER.md) for durable-attempt evidence limits.

## Verification boundary

Portable tests use actual SQLite files, original source/global producers and the
real journal store/actor. Journal fixtures use the explicit maintenance-only
no-capacity bootstrap; they do not fabricate native physical qualification.
Tests exercise two sources plus global, ordering, external writer exclusion,
reverse cleanup, busy and unknown schema, actual hard WAL size, cancellation,
caller drop, queued and active expiry, owner loss, copied-history exhaustion,
omitted/duplicate/foreign claims, native device mismatch and no identity/intent.
Normal-build public factory and lease cleanup tests use genuine owners without
test-support positive producers. Privacy checks reject invented input,
observations and handles.

Positive production native journal bootstrap and physical qualification still
require the exact platform resources tracked by OPT-2026/2262. Missing portable
parent implementation remains implementation work, not a platform verification
waiver.
