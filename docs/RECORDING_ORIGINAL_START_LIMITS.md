# Original recording transport startup limits (OPT-2376)

`RecordingTransport::bind` retains the exact validated sample rate, recorded channel
count and maximum frame count privately in its original settlement route. The byte
ceiling alone cannot establish those semantics: mono/8192 frames and stereo/4096
frames have the same encoded byte ceiling. A changed sample rate can also increase
the callback ring without changing the finite encoded allocation.

The real `RecordingProducer::start` compares supplied limits before preparing its
ring. Native `start_into_bound_sink` compares the actual negotiated sample rate and
recorded channel count, with the requested maximum frames, before writer/monitor
rings, paused stream construction or initial header serialization. The private
serializer entry repeats the comparison before creating its terminal issuers or
writing a header. A mismatch returns a fixed recording error and drops the original
binding; it emits no byte request or terminal/joined proof.

Native callers must prepare the transport from the actual resolved input format.
The public input resolver provides that observation; a later negotiation change
causes refusal and requires a fresh preparation. A requested format that falls
back to another device format cannot silently replace the original preparation.
The generic `RecordingSinkBinding::bind` remains arbitrary codec plumbing with
caller-selected limits. It asserts no original finite preparation or permission.

The original CPU regression executed all four mismatches before the fix. Each
started and emitted 76 header bytes; the actual prepared ring sizes were
1,536,000, 768,000, 384,000 and 12,288,000 bytes. The thread-local diagnostic runs
immediately after the real `HeapRb` preparation; it measures sample backing,
not allocator overhead or process RSS. The same real startup test requires zero
ring preparation, header traffic and terminal evidence after refusal. Existing
exact-binding WAV/terminal tests and generic codec producer tests retain their
positive behavior. The native resolved-limit helper is tested without pretending
to enumerate a device or exercise CPAL callbacks.

This fixed private context changes no ring ceiling, input-pool credit or physical
allocation policy. It supplies no journal Start or required-WAL permit, retained
custody extraction, chunk/effect grant, physical staging, publication, refund or
settlement. OPT-2145/2058 and native runtime verification OPT-2066/2067 remain open.

Relevant commands: producer-only and default `cargo test --locked -p reson --lib`
(add `--no-default-features` for producer-only), both configurations' doc tests,
library `recording_custody`/`recording_prestart` tests, Windows GNU producer-only
test compilation, formatting, and `bash scripts/agent.sh preflight` on committed
HEAD. Native verification must additionally exercise a real supported input
device, exact negotiated binding and changed/fallback format refusal, recording
the device/backend/configuration and exact tested commit. Linux helper tests do
not complete that native acceptance.
