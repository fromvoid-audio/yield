# Bounded retained Harvest facts (OPT-2105)

`GlobalLibraryWriter::read_harvest_facts` reads exact workflow state and immediate
stored derivative count for one audited source's unique supported-audio paths.
Request construction and source observation happen off the UI thread.
`harvest_facts_client()` provides a cloneable read-only facade sharing the existing
bounded transport/admission/closing state; client clones keep no profile lock or
worker alive. Inputs are
privately issued `SourceFileObservation`s from the real retained source writer,
plus the receiving current profile guard and a one-way `HarvestFactsCancellation`.
Missing/index-only/unsupported members and stale source contexts cannot be replaced
with path strings or decoded observations. Source and receiving profile acquisitions
must match. Source root/database/lifecycle/lease/audited revision must agree across
all requested observations and remain valid before read and acknowledgement.

State and count come from one retained global SQLite read transaction. An absent
Harvest file is explicitly untracked/New; unknown state spelling is never silently
converted to New. A stored file row must identify the exact observed size, mtime
and full stored content hash. A stored edge endpoint naming that file must likewise
match its observed identity. No source/hash mutation or physical revalidation occurs.
Unverified/conflicting identities and unsupported operations fail closed; other edge
endpoints are historical metadata, not proof of their current source membership.

Limits are 128 paths, 32 KiB aggregate path bytes, 64 KiB complete captured semantic
input and encoded result, and 64 incident graph edges per path. Complete row/edge
shape checks happen before field allocation; each text field is capped at 4096
bytes and the complete per-path observation at 64 KiB. Exact counts exceeding this
budget reject rather than truncate. The global SQLite progress handler checks
cancellation every 1000 VM instructions, stops after 1024 callbacks or two seconds,
and is removed on every exit. Damaged indexes cannot force an unbounded scan.
Source validation remains the existing bounded retained-control read, not a lease
renewal. No receipt, source revision, WAL growth, snapshot table or checkpoint is
created by these reads.

Only the actual global worker holds a strong private mutation-generation token.
Before any existing authored-state, copy or reference-rekey command runs, the
worker replaces that token. Even an aborted, rejected or exactly replayed write
conservatively invalidates prior results. Reads carry only a weak token; result
retention cannot keep the worker, profile lock or source owner alive. Worker exit,
profile invalidation or shutdown ends the generation. A newly opened worker never
revives an old token. UUIDs are diagnostic correlation, not writable authority or
a durable database revision. One-way cancellation cannot revive old results.

`HarvestFactsBatch::is_current_global_generation()` checks only global token lifetime
and cancellation, with no SQLite/filesystem/mutex I/O. Consumers still must compare
the batch source epoch and receiving profile against their current source/browser
root, lifecycle, directory generation and committed revision before publication,
and discard cached decorations when any fence changes. This check is not an atomic
source/global/UI transaction. Catalog/session request, queue and browser publication
integration remain OPT-1900; UI filtering remains OPT-1898. Neither parent is completed
by this backend prerequisite. Unauthorized external global writers bypass the sole
writer contract and do not supply supported generation invalidation.

Actual owner/SQLite tests cover states/counts, absent rows, path/input/output/query
bounds, malformed/ambiguous/uncertain metadata, cancellation, source/profile changes,
real authored/copy/rekey invalidation and restart/shutdown. Metadata hashes/edges
seeded by fixtures do not qualify physical publication, absence, power-loss durability,
a private journal checkpoint or the full Harvest saga. Native owner regressions remain
OPT-2042 (macOS) and OPT-2043 (Windows).
