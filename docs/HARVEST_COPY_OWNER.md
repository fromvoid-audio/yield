# Actual copy metadata owner (OPT-2065)

`GlobalLibraryWriter::apply_harvest_copy` runs one bounded copy metadata command
through the existing admitted queue and retained `library.db` connection. It uses
the existing `harvest_files` and `harvest_derivations`, plus an immutable receipt
keyed by the operation and original frozen Harvest obligation. It introduces no
source/session/UI producer and no second graph store.

The request needs genuine private observations: persisted copy-family metadata
from OPT-2076, full-content source observations from OPT-2063, destination metadata
enrollment from OPT-2064, and an Applied global registry participant receipt for
the exact frozen global obligation. It also captures a privately issued bounded
global reference snapshot. Copy/DuplicateSame endpoints, participant storage,
original source expectation, destination enrollment's single exact audio entry,
source/global/Harvest keys and profile identity must all agree. No caller boolean
can select no-rekey behavior. This copy command never transfers existing edges or
removes the origin.

An enrollment advances the generic source revision even if the exact entry was
already present. A fresh complete audit must precede obtaining the destination's
reference/file observation. Enrollment metadata is not audit coverage, publication
authority or a private family completion. Tests use the actual audit transaction
with explicitly seeded metadata; they do not qualify physical file effects.

Historical proposal bytes retain exact source storage/lifecycle/lease/revisions,
family evidence, source enrollment and registry observations, graph precondition,
inherited metadata and tool identity. The family must retain these exact bytes
before dispatch. Restoring them requires real current owners for the same storage;
fresh acquisition contexts never rewrite historical payload fields. New effects
require exact historical/current source facts; existing receipt replay can observe
fresh later file state while returning the original result without touching it.

SQLite reads the receipt first. Exact replay precedes capacity and current graph
checks. New outcomes require the actual matching Applied registry ledger entry,
bounded graph CAS and exact file identity checks. Missing/unverified or different
existing content records IdentityConflict; changed graph records GraphChanged.
Neither rejection advances a required journal checkpoint. Applied changes at most
two file rows and inserts one edge. Direct SQL preserves authored Done/Ignored,
notes and timestamps; no compatibility merge/reparse is used. Edge/file and receipt
readback occur before commit. SQL failure rolls back all changes and the receipt.

The canonical proposal and result budgets are 64 KiB. Rating is -3 through 3;
playback type is absent, one-shot or loop; at most 64 sorted unique nonempty tags
are allowed, each at most 256 bytes and 16 KiB total. Full copy has no invented
selection range or duration. Text identities/tool are bounded and reject NUL.
SQLite type/length/duplicate-key/sequence predicates precede receipt allocation;
canonical encodings and a domain-separated result hash protect readback. Ledger
retention is 8192 rows and 16 MiB of all fields/scalar overhead, with no eviction.
Existing verified receipts remain replayable when new admission exhausts capacity.

Journal validation runs outside SQLite transactions. Source ownership/facts are
rechecked before effect, before commit and at delivery; the existing global owner
checks its actual profile/database bindings around dispatch. A postcommit source
or family change reports CommitUncertain, retaining the original operation and
proposal for replay. Receiver disconnection does not cancel admitted work. The
queue's monotonic Closing and terminal rejection/drain behavior remain intact.

These are actual metadata transactions and immutable observations. Source/global
transactions remain independent; point-in-time read fences are not atomicity.
The admitted family producer, qualified publication/absence/retirement, rendered
or move branches and private journal checkpoint adapters remain OPT-1645/1787/1634.
Contract snapshots/capacity-free journal fixtures and seeded content hashes are
not production physical qualification. Native profile/SQLite/lease regression
remains scoped to OPT-2042 (macOS) and OPT-2043 (Windows).
