# Original recording header and frame batches (OPT-2380)

The finite `RecordingTransport` factory retains the original hound initialization
in one fixed 68-byte scratch image. Its concrete scratch structure is at most
128 bytes, with no allocation, PCM buffer or additional worker. Writes must cover
a contiguous initialized prefix; bounded rewrites may seek within that prefix.
Incomplete initialization, holes and overflow fail permanently before dispatch.

Initialization writes acknowledge only buffered byte custody. The first real
Flush requires the complete header and cursor at byte 68, emits one immutable
header request at offset zero, and waits for its original acknowledgement. The
scratch is consumed before a separate Flush request, whose acknowledgement is
also required before producer startup completes. Denial, drop, disconnect and
timeout remain sticky, including late replies; no fallback I/O or retry occurs.

The original finite settlement binding selects the sole existing inspectable
serializer buffer capacity as `floor(16384 / (channels * 4)) * (channels * 4)`.
Generic codec bindings retain their existing 16,384-byte buffer and sink behavior.
The actual pre-finalize snapshot checks that same selected capacity and retains
its genuine pending bytes. There is no rechunking buffer or hidden PCM remainder;
terminal barriers and successful joined evidence retain the original inventory.

The existing original owner route allowance is 64 KiB, separate from proposal
and retained-context backing. This fixed initialization structure adds less than
128 bytes within that allowance; no pool or wire ceiling increases. This is a
component accounting statement, not an allocator/RSS or complete startup bound.

Acceptance uses the actual CPU producer, hound serializer, original request
receiver and settlement route. Channels 1–8 cover zero, short, one full batch,
multiple full batches and a larger recording; initial bytes equal the real hound
header, every PCM batch and pending tail is a whole frame, decoded samples remain
exact, and barrier/join inventories agree. Real producer tests hold header, PCM
and separate Flush replies and refuse dropped/denied startup. Component tests
cover incomplete initialization, bounds, preserved rewrites and sticky failures.

The immutable before-fix checkout and actual failed regression remain separate
from after-fix evidence. Receiver acknowledgements confer no journal chunk grant,
physical write/flush qualification, Start authority, publication, retry/refund or
genuine settlement. Full OPT-2131/2145/2058 terminal owner/store, retained physical
transport, session integration and retirement acceptance remains open. Native
audio/device verification remains OPT-2066/2067.
