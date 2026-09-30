# Browser filter-family controls

This is the independent Name, Playback Type and Rating implementation portion of
[OPT-1907](https://linear.app/boostnlvp/issue/OPT-1907). It does not complete the
six-family acceptance ticket. Harvest facts/filter integration remains dependent
on OPT-1900 and OPT-1898; existing Tags and Curation behavior is retained.

The core projection stores each family enable flag independently from its query
or chip selection. Disabling affects only that family's predicate, including
collection counts. Editing a disabled family's choices keeps it disabled.
Re-enabling uses current entries, including replaced source data. Flags retain
the existing browser-session lifetime; no new preference or restart persistence
is claimed. Temporary reveals are cleared when a family switch changes.

Pointer and macOS AX actions share the same handler, which rejects actions
without an active source, invalidates prepared file handoffs and reconciles
selection and focus. Each new AX family control is a checkbox with an explicit
On/Off value. Filled/empty circles and text colors show enabled/suspended states.

Validation on the Linux development host: all 184 yield-core tests passed,
including independent family intersections, collection counts, disabled-choice
edits, and replacement-data restoration. Native macOS AX, keyboard/pointer,
constrained-layout and exact-head local CI acceptance must still run on macOS.
The ordinary Linux UI check is blocked by existing recovery-reserve dead-code
lints under deny(warnings). A diagnostic UI check with `RUSTFLAGS='--cap-lints warn'`
passed after correcting an unguarded macOS-only reserve check and completing
existing Curation AX identifiers in the non-macOS stub. Those corrections
preserve unsupported-platform behavior and do not provide native AX support.
The diagnostic is type-check evidence only, not the required local CI gate.
