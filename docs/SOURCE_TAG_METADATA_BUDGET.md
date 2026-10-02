# Source tag metadata allocation observations

OPT2083 supplies the production SourceWriter observation needed before the
OPT2049 capped tag participant can prepare real capacity admission. It does
not install transaction policy or authorize writes. No caller can construct
or deserialize a SourceTagMetadataBudget. Captures and revalidation run on
the existing retained source owner, with a two-second SQL progress budget and
current profile, physical source lease, root/database identity, lifecycle,
revision and complete authoritative audit fences.

A capture records SQLite page size/count and the current max_page_count. The
proposed entered limit is the smaller of that current limit and 64 MiB divided
by page size. Neither logical pages nor the actual main database VFS length
may exceed that limit. Existing smaller limits are never raised by capture.
Main database and WAL file lengths come from SQLite's retained VFS file objects;
no new SQLite database is opened to observe either file. The existing WAL hard
admission gate also applies. WAL-index length is a no-follow regular-file
namespace observation through the retained database root; missing, truncated
or non-32-KiB-aligned observations reject. This is not an issued file effect
capability or proof that a future SHM allocation has been admitted.

For page size P, proposed limit M, observed resident WAL W and WAL-index S, the
checked plan is:

    P*M + W + (32 + M*(P+24)) + S + (32768 + 16*M)

This deliberately includes existing sidecars and additional sidecar growth.
It requires an enforced single entered SQLite transaction with cache spill,
auto-checkpoint and disk temporary storage disabled. Capture does not enforce
any of those settings. Profile and journal allocation/retention claims must
also be included by the actual capacity owners; this value alone never
satisfies admission. OPT2049 must retain/revalidate the applicable observations
and policy through its admitted transaction before using the plan.

The schema boundary compares a finite object/DDL image to the compiled source
schema plus the production source_lease table. Unknown objects, triggers,
attached databases and temporary objects reject. Bounded function/module/
collation images must match fresh bundled SQLite. A finite set of lazy built-in
pragma modules is prewarmed on both connections; arbitrary prefix exemptions
are forbidden. Attempted dynamic extension initialization permanently makes
this bound unknown in that process, even when loading failed, because callbacks
or auto-extensions may have been registered. The nonblocking process loader
scope prevents a dynamic load during capture. No reset seam clears that fact.
Unknown custom SQLite behavior needs a separately reviewed bound, not a waiver.

Revalidation requires the original SourceWriter incarnation and exact physical
facts as well as current logical authority. An observation does not survive
handoff/reopen as a write grant. Actual source fixtures run in bounded fresh
processes so optional extension tests cannot erase or race the production
process-wide safety fact. Test output is checked to ensure the child executed
one real fixture. These Linux tests do not qualify native physical capacity,
filesystem durability or an implemented tag delta/receipt.
