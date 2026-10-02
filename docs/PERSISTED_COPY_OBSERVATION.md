# Persisted copy-family observation (OPT-2076)

`OperationJournalClient::observe_copy_intent` reads one exact initial copy contract
through the actual retained journal owner. It accepts a stable operation and the
original frozen Harvest key. The store supports strict initial-intent records and
full-progress envelopes, validates typed Copy/DuplicateSame arguments, participant
source mappings and their derived plan, and requires exactly one applicable Harvest
obligation and one global obligation. Other families, ambiguous journal-family keys,
invalid/canceled/audit dispositions or unsupported storage fail closed.

The privately issued `FrozenCopyObservation` carries the exact persisted family,
participant inventory, plan/key and actual profile-root/database/binding identity.
There is no public constructor or decoder. Its command/plan getters and canonical
bytes are historical observations; copying those DTOs cannot mint this result.
Source/global consumers still need their genuine current owners and receipts.

One SQLite statement reads both families with `LIMIT 2`, guarded type/length CASE
expressions, and a fixed 64 KiB record budget before BLOB allocation. Existing strict
codecs, canonical readback and sequence checks are reused. A capped writer applies
the same 64 KiB ceiling to the canonical result. The queue retains its existing finite
input/result slots; this read allocates no operation ID or capacity claim.

The observation holds only a weak journal-owner reference. `validate()` submits a
fresh bounded read to that same owner and compares the immutable family/storage
facts; this rechecks the live profile/database namespace. Caller waiting is limited
to two seconds. Timeout neither cancels the queued read nor moves ownership. Call
validation outside any SQLite transaction. Shutdown/drop invalidates the old result;
fresh reopen must read a new observation. The value retains no profile lock.

This proves privately issued provenance of **persisted family metadata**. It does not
prove physical admission, live capacity/claim/permit, publication/absence/retirement,
qualified power-loss durability or private journal completion. Public full-progress
observations retain their existing non-authoritative meaning. OPT-2065 consumes this
family evidence to decide copy-only/no-reference-rekey semantics; OPT-1787/1634/1645
retain their complete admitted-family/qualified predecessor/checkpoint acceptance.

Linux verification uses the actual journal actor and profile-owned SQLite. Test-only
stored intents/full snapshots and the capacity-free journal fixture are explicitly
metadata test inputs. Native filesystem/lease semantics require the scoped macOS and
Windows lanes; Unix replacement tests are not Windows execution.
