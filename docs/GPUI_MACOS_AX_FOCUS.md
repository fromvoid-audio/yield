# GPUI virtual-child focus on macOS

`AxRegistry::set_focused` keeps the native child, the application's focused
accessibility element, and the focus-change notification in sync. Disabled or
retired children clear application focus if they owned it. The browser passes
both its GPUI focus handle and window activation state when synchronizing.

Run the two-control probe from this checkout, then inspect it from another
terminal:

```sh
cargo run --locked -p yield-platform --example ax_registry_probe
swift crates/yield-platform/examples/ax_focus_inspect.swift <probe-pid>
```

The inspector checks disabled focus refusal, live and reordered child focus,
retirement of a held AX element, application `AXFocusedUIElement`, and native
focus-change notifications. It exits with `PASS` only when every check succeeds.
