# GPUI macOS adjustable slider contract

`AxRegistry` reconciles a slider as an `AxChild` with role `Slider` and no string value. The owner then calls `set_slider` with a finite numeric value and inclusive bounds after each reconciliation. The registry publishes native numeric `AXValue`, `AXMinValue`, and `AXMaxValue`; it advertises value setting and increment/decrement only while the child is live and enabled. Native callbacks validate numeric requests, queue bounded `AxEvent`s, and leave application state changes to the GPUI owner. The owner must recheck its current authority and apply its own step for increment/decrement.

## Native fixture

Build and launch `cargo build --locked -p yield-platform --example ax_slider_probe`, then run `swift crates/yield-platform/examples/ax_slider_inspect.swift <pid>` from the repository root. The isolated probe has one slider, an enabled-state toggle, and a removal button.

On macOS 26.6.2, the inspector found `AXSlider` with numeric value `0.5`, numeric bounds `0` and `1`, `AXValue` settable, and `AXIncrement`/`AXDecrement` actions. Setting the value to `0.3`, incrementing to `0.4`, and decrementing to `0.3` updated GPUI-owned state. An out-of-range `1.2` request left the accepted value unchanged. Disabling the slider removed its actions and value setter; removing it cleared it from the window AX tree and retired the previously held native child. This is a platform contract fixture, not yet the browser volume control; OPT-1750 owns that integration.
