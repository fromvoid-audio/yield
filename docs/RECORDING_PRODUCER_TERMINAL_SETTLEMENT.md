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
