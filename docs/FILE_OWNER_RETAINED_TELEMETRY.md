# Original file-owner retained-result observations

`FileOperationOwner::retained_telemetry_snapshot` returns fixed numeric observations
from the original worker's receipt and tombstone state. The snapshot returns
`None` when its short update lock is busy. Its separate 80-byte payload contains
current receipt/tombstone counts and eight saturating original retained primary
outcome counters; the complete original queue plus retained collector fits 256
bytes. It stores no paths, IDs, hashes, payloads, error text or event history.
The existing queue snapshot retains its original API and meanings; these two
snapshots do not form one atomic combined observation.

The worker counts a primary result after inserting its original receipt, including
lost replies. Exact duplicates and changed-request conflict replies do not count
as second primary outcomes. Acknowledgement moves the actual receipt to a
tombstone; the counter remains. Original shutdown clears the gauges only after
its existing confirmed custody decision clears those inventories. Unavailable,
panic and unresolved shutdown retain the original state and its observations.
Queued requests abandoned before a result create no retained primary outcome.
Inventory updates and result classification do not clone payloads or destroy
commands, grants, inputs or receipts under the telemetry lock.

The gauges describe these in-memory inventories only. `Applied` can mean read-only
staging validation and cannot establish publication or physical qualification.
`RetryableBeforeEffect` records the existing typed result and authorizes no retry.
Acknowledgement, tombstone retention, metric saturation or a zero gauge supplies
no durable custody, physical quiescence, refund, safe shutdown or genuine
settlement. The original owner result and lifecycle remain authoritative.
Production publication and the full owner lifecycle/telemetry acceptance remain
open under OPT-2215/2216/2217/2218/1632 and OPT-1673/1670.

Portable tests use real original queues and receipt transitions, genuine profile
staging observations and explicit pre-effect scalar-boundary fault injection.
They preserve original results, exact input custody, lost replies, duplicate and
conflict behavior, cancellation, panic and unknown shutdown. Logical fixtures
and failure injection create no executable filesystem grant or qualification.
