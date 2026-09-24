# GPUI macOS accessibility child registry

`yield_platform::accessibility::AxRegistry` extends the single-control native bridge proved in `GPUI_MACOS_AX_BRIDGE_PROOF.md`. One registry owns the AX children installed on the real GPUI `NSView` for one window. The owning GPUI entity keeps the registry and its bounded event receiver alive for that window's lifetime.

## Owner contract

1. Create the registry from the window handle on the AppKit main thread. Keep the receiver in a `cx.spawn_in(window, ...)` task and route each `AxEvent` back through `update_in`. Never mutate GPUI state in an AX callback.
2. Reconcile the complete, ordered list of visible controls using stable window-local IDs. Each child supplies a native role, a nonempty label, a current value when applicable, and an enabled state. Reconciliation preserves native AX objects for unchanged IDs, reorders them, and retires removed children.
3. In a control's layout/prepaint callback, call `set_frame` using its actual GPUI window bounds. The registry converts the top-left GPUI coordinates into AppKit parent coordinates. Update `set_focused` from the actual GPUI focus handle and window activity.
4. On `Press` or `Focus`, recheck that the control still exists and is enabled in the GPUI model before dispatch. Activate the app/window for keyboard focus and synchronize AX focus after the GPUI focus change. The queue is bounded to 64 events; a full queue rejects a press or drops a focus request.
5. Drop the registry with the owning window entity. Its receiver closes, children are retired, and the NSView child list is cleared. Do not share one registry across windows or keep it after the owning window closes.

The bridge is macOS-only and contains no browser, waveform, transport, or Settings policy. Those surfaces own their own semantic labels, order, frame updates, and action routing.

## Native fixture

On macOS, build and run:

```sh
cargo build -p yield-platform --example ax_registry_probe
target/debug/examples/ax_registry_probe
```

From another terminal, use `pgrep -fl ax_registry_probe` for the process ID, then run `swift crates/yield-platform/examples/ax_registry_inspect.swift <pid> <action> <control-id>`. The fixture starts with `first-toggle` enabled and `second-control` disabled. `focus first-toggle` reports focused=1 when the fixture remains the active app. `press first-toggle` changes Off to On and enables the second control. `press second-control` moves it before the first in both GPUI and AX order and changes its value to `Remove next`; both native AX frames follow their new GPUI positions. Pressing it again removes it and leaves only the first child. `close` closes the window; the fixture logs child detachment and exits.

On September 24, 2026, this sequence passed on macOS 26.6.2 using the pinned GPUI 0.2.2 dependency. Native focus and frame updates require activating the GPUI application when an AX action arrives. The fixture's `cx.activate(true)` plus `window.activate_window()` makes the window active before changing its GPUI focus or model.
