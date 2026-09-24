# macOS accessibility bridge proof for GPUI 0.2.2

OPT-1870 proves one native accessibility route on the pinned `gpui =0.2.2`
`macos-blade` backend. The native host was macOS 26.6.2 (25G83), arm64, with
Rust 1.93.0. This is an isolated control proof, not browser, waveform, Settings,
or release accessibility acceptance.

## Route

GPUI's public `Window: HasWindowHandle` returns an AppKit handle containing its
real `NSView` on macOS (`gpui-0.2.2/src/platform/mac/window.rs`). The proof in
`crates/yield-platform/examples/ax_bridge_probe.rs` retains that view for
the root GPUI entity's lifetime and attaches one `NSAccessibilityElement`
subclass through `NSView.setAccessibilityChildren`. Pinned GPUI has no exposed
semantic-tree API, and its `PlatformWindow` is crate-private; neither a GPUI
fork nor a parallel native widget is needed for this route.

The visible button remains a GPUI `div`. Both its mouse handler and native
`accessibilityPerformPress` event call the same `Probe::toggle` state mutation.
The native callback uses a bounded asynchronous channel to enter GPUI's window
context. Its callbacks hold only a channel sender and atomic focus state, so
they do not touch GPUI or AppKit off the UI thread. AXValue is updated by the
same GPUI mutation; AX focus requests activate the window and focus GPUI's
`FocusHandle`. The frame comes from the button's real GPUI prepaint bounds and
is converted to the parent view's coordinates. `Probe::drop` detaches the AX
child and parent before the GPUI entity releases its view.

## Native evidence

From this worktree, build and run:

```sh
cargo run -p yield-platform --example ax_bridge_probe
```

From another terminal, get the process id with `pgrep -fl ax_bridge_probe` and
run the companion native AX inspector:

```sh
swift crates/yield-platform/examples/ax_bridge_inspect.swift <pid> press
swift crates/yield-platform/examples/ax_bridge_inspect.swift <pid> focus
swift crates/yield-platform/examples/ax_bridge_inspect.swift <pid> click
swift crates/yield-platform/examples/ax_bridge_inspect.swift <pid> close
```

On 2026-09-24, `AXUIElement` enumerated `AXWindow > AXButton` with label
`Probe toggle`, position `(790.0, 585.5)`, and size `(220.0, 64.0)`. `AXPress`
returned success (`0`) and changed AXValue from `Off` to `On`. Setting
AXFocused returned success and reported focused `1`. A CGEvent click at the
reported frame center changed the same AXValue back to `Off`. Pressing the
native close button returned success; the process exited and logged
`OPT-1870 AX child detached`. The exact position depends on display layout;
the size and physical click validate the frame on this host. `cargo check`
and `cargo build` of the example passed.

## Production contract and remaining acceptance

Per-surface adapters must maintain one owned child registry per GPUI window,
with stable control identity and precise attach, update, and detach rules.
Each adapter must derive role, label, value, focus, action availability, and
geometry from the same GPUI control model and dispatch native actions through
the corresponding GPUI handler. A single `setAccessibilityChildren` call for
each control would overwrite siblings. Layout changes, scroll offsets,
multidisplay coordinates, backing-scale changes, window activation, dynamic
values, AX notifications, and window teardown need native verification. The
example demonstrates one button only; it does not settle text input semantics
or any per-surface issue. Keep OPT-1749, OPT-1750, OPT-1871, OPT-1494, and
OPT-1863 open until their own implementations and native acceptance pass.
