# Original journal queue metrics

`OperationJournalClient::queue_metrics_snapshot` returns fixed per-owner ordinary/control counters and five dispatch residence buckets. A contended accounting mutex returns `None` immediately. Snapshots contain no operation IDs, paths, SQL, payloads or authority.

The existing 28 ordinary and four control command slots and priority/FIFO scheduling remain unchanged. Each real envelope has one private tracking ticket. Successful `try_send` records accepted backlog and its peak; accounted receive records dequeue and residence; discard/receiver teardown records abandonment. Dropped replies do not abandon accepted work. Failed sends do not count as accepted. Dispatch is observed before command validation/execution, so counts do not prove storage success or durability.

Backlog ends at completed receive accounting: it can include one envelope already removed from the channel and waiting for the short accounting mutex. Peak can therefore reach that channel's capacity plus one. It does not report instantaneous channel length. Residence starts immediately before the nonblocking enqueue attempt and ends at accounted dequeue, including that short bookkeeping window, but excluding admission/build and command execution. Buckets are <=1, <=10, <=100, <=1000 and >1000 ms.

Only nonblocking send and numeric bookkeeping occur under the accounting mutex. Rejected command and original resource destruction occur after unlocking. The ticket and collector fit fixed bounded backing; no input/result/publication pool ceiling or execution policy changes. Counters saturate; backlog is independently maintained for live tickets, never cleared by a bulk reset.

Real channel race/full/disconnect/priority/drop tests and actual profile-owned worker lost-reply/reopen/panic tests exercise these sites. Observations issue no permit, retry, refund, settlement, physical qualification or SLO acceptance. OPT-1673 and full startup/lifecycle acceptance remain open.
