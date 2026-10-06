# Recording producer terminal settlement (OPT-2205)

`RecordingSinkBinding::bind` moves one retained byte transport into a private
serializer binding and returns its sole `RecordingSettlements` receiver. The
adapter retains that receiver before passing the binding to
`AudioRecorder::start_into_bound_sink`. The normal paused CPAL construction,
header initialization and stream play ordering is unchanged. Each binding has
only two one-element channels and a private instance identity. Binding an
arbitrary codec transport supplies no admitted journal or filesystem authority.

Handle the receiver on the owner thread, separately from synchronous Stop/join.
The worker closes accepted callback submission, waits for any submission that
already passed Running, drains Stop samples or fixes Cancel abandonment, and
captures complete nonsaturated sample accounting. Only then does it issue one
`RecordingTerminalBarrier`, before terminal WAV tail/header finalization. Its
format, frames and accepted/encoded/abandoned/unconfirmed partition are actual
producer observations; encoding is not retained-byte acknowledgement. Input-loss
counters are observations at this exact barrier; rejected later callbacks cannot
change its accepted-sample partition.

The admitted adapter must validate its current source/profile/effect/capacity
and exact sink binding, and persist its own immutable terminal inventory before
consuming `acknowledge`. Producer instance matching is available through the
receiver's `owns_barrier` and `owns_completion` methods. There is no public token
constructor, Clone, serialization or reconstruction from path, health or UUID.
Dropping/denying the barrier, missing or lost delivery, a changed disposition or
the fixed two-second acknowledgement deadline closes the serializer transport.
Capture callbacks never wait on this channel or perform owner/journal I/O.

Before dropping the codec, the worker arms a sticky fence on the same bounded
transport hound uses. Denied terminal writes, seeks and flushes are rejected,
including hound and BufWriter destructor attempts. Transport errors also arm
this fence immediately; unwinding cannot forward compensating I/O. Real partial
writes preceding a transport failure remain retained and unconfirmed. The codec
cannot undo those bytes or claim that failure was atomic. Cancellation after a
Stop barrier invalidates that barrier rather than issuing a replacement grant.
Already selected cancellation never becomes Stop.

Only successful actual finalization **and successful actual worker join** can
send a separate one-shot `RecordingJoinedCompletion`. Worker completion alone,
panic, join failure, repeated join, recorder Drop and lost joined delivery cannot
supply it. Stop returns an error when joined delivery is lost, even if real WAV
bytes were already finalized. Neither evidence value publishes, verifies retained
bytes, releases charges, reopens staging or qualifies a physical reserve.

The existing `start_into_sink` observation API remains available without these
proofs. This change does not activate GPUI recording. Actual owner capability,
Finalizing/Frozen inventory/seal integration remains OPT-2145; pre-capture terminal
row/byte reservation OPT-2162 is already implemented. Retained transport, qualified
physical validation/publication and retirement remain OPT-2058 and its parents.

Twelve real hound/worker/AudioRecorder regressions use owned in-memory transports:
Stop/Cancel ordering; successful join timing; denial/drop/timeout; independent
instance issuance; callback-inflight settlement; winning cancellation; error,
panic and lost delivery; no late acceptance; startup failure; unknown accounting;
and exact prefix retention with no destructor I/O after fault. Four compile-fail
doctests enforce private construction and absence of Clone. Run:

```
cargo test --locked -p reson recording --lib
cargo test --locked -p reson --lib
cargo test --locked -p reson --doc recording::settlement
bash scripts/agent.sh preflight
```

Actual macOS/Windows CPAL devices and native callback/Stop/Drop lifecycle are
pending in OPT-2066/2067. These Linux transport tests qualify no device, physical
capacity, journal inventory, publication or power-loss behavior.

## Device-independent producer and owner-facing types (OPT-2294)

Reson's default `native-audio` feature keeps existing input, monitoring and
playback behavior enabled. With `default-features = false`, the actual bounded
WAV serializer, sink binding, sample accounting, pending-tail/final-header
inventory and private terminal proofs are available without CPAL, ALSA or Windows
ASIO. This lets the journal owner retain the genuine nominal producer types;
it does not replace them with a parallel model or data-only receipt.

`recording::RecordingProducer` consumes the original `RecordingSinkBinding` and
runs the same worker as native capture. CPU sample submission uses its bounded
ring and accounting. Stop/Cancel close submission and join through the original
pre-finalize owner acknowledgement; owner denial cannot emit successful joined
evidence. The owner must handle the paired settlements separately from the
blocking Stop/join caller. Drop uses the existing protected cancellation/join
path and cannot manufacture a joined proof.

Producer-only checks include `cargo test --locked -p reson --no-default-features
--lib`, its proof privacy doctests, and a real Windows GNU all-test compile.
The dependency tree in that configuration excludes native device SDKs. Default
native-audio tests still cover the original recorder/monitor/playback routes.
Neither configuration qualifies an arbitrary transport as an admitted physical
sink or activates recording in GPUI. OPT-2145 still requires the actual private
sink/source/profile/claim/capacity association and Finalizing/Frozen/seal owner
integration; OPT-2058 retains physical transport/publication/retirement scope.

## Original bounded serializer channel (OPT-2295)

`RecordingTransport::bind(limits)` constructs the original private sink and returns its
single byte-request receiver together with the original sink binding and settlement receiver.
The receiving owner must be running before `RecordingProducer::start`: startup writes the
initial WAV header synchronously. The receiver also continues running while another thread
performs Stop/Cancel and joins the serializer. No capture callback waits on this channel.

Each request retains exact immutable bytes and its serializer offset. At most one write
(up to 16384 bytes) or flush awaits acknowledgement. Original finite initialization first
buffers at most 68 header bytes; those Write returns mean byte custody only. Its first Flush
waits for one complete header request at offset zero and a separate Flush acknowledgement.
After initialization, a write advances the sink cursor only after acknowledgement;
seeking updates a checked local cursor and cannot exceed the finite
allocation or use end-relative seeks. Refusal, request/receiver loss, queue failure or the
fixed two-second acknowledgement deadline permanently fails the sink. The timed-out request
can remain retained for audit; late acknowledgement cannot resume encoding. A lost response
is unconfirmed even if the receiving owner already performed an effect. There is no automatic
retry, fallback file I/O, publication, refund or genuine settlement.

This is production byte plumbing, not an admitted file-operation adapter. Consumers must
bind actual current source/profile/output/chunk/capacity authority before retaining or
executing requests. Neither `acknowledge` nor the original producer proof constructs that
authority. OPT-2145 still owns immutable Finalizing/Frozen/seal storage and authentic owner
binding; OPT-2058 still owns retained physical transport, verified physical results and
publication/retirement. Cursor-backed transport tests verify original serialization and
proof routing, not native qualification or journal acceptance.
