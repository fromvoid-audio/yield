# Collection history catalog transport (OPT-2160)

The existing catalog worker accepts a privately constructed `CollectionHistoryRequest`:
source, lifecycle, exact numeric revision, nonzero caller generation, one collection slot,
explicit desired membership and 1–128 distinct supported exact relative paths. Pathnames
are limited to 4,096 bytes. Validation checks the combined request/result estimate before
cloning paths. Accepted source IDs are copied after validation so a short ID cannot
retain an oversized caller allocation. A conservative 32 KiB identity/struct allowance and twice each pathname
plus 256 bytes per row fit within 1 MiB. No request serde or public field mutation exists.

The worker qualifies a fresh complete SourceRevision on its actual retained SourceWriter,
then calls the merged OPT-2138 capture. It moves the private selection into a Debug-only
event with the original request and cancellation token. The event contains deterministic
confirmed changed bits, not an intent, capacity grant, replay record or history stack.

Existing FIFO command, input and output accounting applies. The output reservation charges
1 MiB for the combined retained request and result until event consumption/discard; the
additional queued-input reservation releases after worker dispatch. Failed admission,
cancelled work, dropped publication and shutdown release the existing reservations.

Source attempt custody cancels replaced/retired/remapped/shutdown requests. A weak reference
to the exact history token lets the worker advance existing attempt custody before later
curation, compound curation or rating-decay writes, without changing ordinary commands
that have no current capture. Audit, replay and feature attempts already advance custody.
Physical publication qualification stays on the worker. Delivery checks only atomic tokens,
the retained publication fence's in-memory liveness, and owner-issued clocks. An expired
successful observation becomes `ObservationExpired` for its still-live caller, with no
publication fence or stale selection. Retired/replaced/cancelled callers are discarded.
Future history admission must revalidate under OPT-2136/2137.

## Executed evidence

`CARGO_TARGET_DIR=/workspace/yield/target cargo test --locked -p yield-catalog --lib`
initially reports 193 passed, two existing protected-reserve bootstrap failures and one
isolated child driver ignored. The parent actually executes that driver in 12 bounded
child processes: mixed add/remove bits, no-op, stale revision/expired lease, cancellation,
queued expiry, replacement, retirement, remap, actual curation commit, missing path/stale
lifecycle, output pressure/closed submission, and constructor bounds. DB/WAL bytes remain
unchanged for read-only paths. The paired OPT-2138 actual sole-owner tests inspect SQL
`total_changes` and authorizer guards; the catalog harness does not expose that connection.

A separate isolated session test installs an actual CatalogService in the existing session
CatalogSlot, submits registration/audit/capture through real forwarding, consumes the owner
selection, and verifies NotReady/Closed admission. Eleven browser catalog tests pass.
The broader normal session catalog run has one pass and five protected-reserve bootstrap
failures; exact native commands and requirements remain recorded in OPT-2143. The initial
local test linker gap was repaired by linking the already installed host runtime libraries
into the writable development sysroot; this changes no repository or platform capability.

Constructor regression rejects ambiguous raw dot/repeated-separator keys before queueing;
Windows native separators are compared using canonical slash spelling. It reproduces
accepted `a/./b.wav` with the original Path equality check, then passes with exact spelling.

Disabling only the pre-write invalidation hook reproduces “old revision projected after
actual commit”. Restoring it passes and a fresh capture confirms the actual new revision.
The two full-suite failures occur before authority assertions because Linux lacks native
protected-reserve namespace bootstrap; OPT-2026 tracks actual macOS qualification.

Final rebased commands, results, merge identity and complete tested/merged tree comparison
are recorded on OPT-2160 and its PR. Normal qualified session bootstrap, native physical
ownership and persisted history acceptance stay open in OPT-2143 and tracking parents.
