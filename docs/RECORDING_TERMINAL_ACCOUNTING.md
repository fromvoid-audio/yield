# Recording terminal sample accounting (OPT-2141)

The actual `reson` recorder now records accepted ring samples, successful complete
codec batches, unattempted ring samples abandoned on Cancel/error, and samples in
a failed batch whose partial encoding is unconfirmed. Counts saturate and set an
overflow flag. Successful encoding is a codec observation, not a journal, flush,
filesystem durability or publication acknowledgement.

`RecorderCapture::submit` marks its one exclusively borrowed producer as in flight
before checking Running. The terminal worker establishes Stop/Cancel in the same
sequentially consistent atomic order, then waits for that submission to finish.
The callback never waits, locks or performs I/O. A callback that already accepted
its batch finishes before terminal accounting; later callbacks are rejected.
The submission guard also clears the marker during unwinding.

Stop drains the settled accepted ring. Cancel discards and counts its remaining
unattempted samples, preserving the unpublished prefix. A failed batch is counted
as unconfirmed, separately from unattempted backlog; finalization failure remains
an error even when codec sample accounting is complete. Worker panic marks failure,
leaves accounting incomplete, and preserves the existing join error.

After join, when `writer_accounting_complete` is true and
`writer_accounting_overflowed` is false, the stable sample counts satisfy:

```text
accepted = encoded + abandoned + unconfirmed
```

Live snapshots are independent observations, not an atomic partition. The counters
use samples, not frames: a consumer must bind the configured channel count before
converting them. Rejected callbacks remain in the separate dropped-sample counter;
they were never accepted into the ring. Completion describes sample accounting
only, including accounting after a reported worker error. It cannot prove that
CPAL input stopped, that a WAV header is valid, or that a durable seal exists.

## Verification

```sh
cargo test --locked -p reson --lib recording::
cargo test --locked -p reson --lib
bash scripts/agent.sh preflight
git diff --check
```

Actual memory transports and the writer thread cover queued Stop/Cancel, an
accepted callback deliberately held across each terminal request, late rejection,
failed batch plus remaining backlog, finalization failure, panic/join failure and
counter overflow. Existing WAV serialization, non-power-of-two channel frames,
sink faults and pathname-preservation fixtures remain required.

## Remaining acceptance

[OPT-2131](https://linear.app/boostnlvp/issue/OPT-2131) owns the durable prefix-seal
contract. Its implementation must bind genuine owner-issued evidence, final header,
retained bytes/hash/length and immutable loss observations; these counters grant
none of that authority. [OPT-2058](https://linear.app/boostnlvp/issue/OPT-2058),
OPT-2057 and OPT-1339 remain open for retained transport, validation/publication,
charge retirement and recording activation.

[OPT-2066](https://linear.app/boostnlvp/issue/OPT-2066) and
[OPT-2067](https://linear.app/boostnlvp/issue/OPT-2067) require actual macOS/Windows
CPAL devices and stream/callback lifecycle verification, including Stop/Cancel
races and input loss. Linux memory-sink tests do not qualify those platforms or
physical storage. No recording UI or live sink is activated by this change.
