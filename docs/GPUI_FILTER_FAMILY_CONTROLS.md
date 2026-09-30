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
With enlarged text, the Type/Rating label sits above the chips so retaining
choices does not squeeze the rating markers into undersized hit targets.

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
The focused GPUI handler test also passed with the same diagnostic lint setting;
it verifies inactive-source rejection and retained drafts/choices after switching
all three families off. This is a headless test, not native macOS acceptance.
The enlarged-browser layout regression also passed with that lint setting,
including the NAME control in toolbar viewport bounds.

## Remaining macOS verification

Run `scripts/gpui-local-ci.sh full` at the PR's exact head. In an isolated profile,
check Name/Type/Rating pointer and AX actions with representative WAVs, retained
text/chips while disabled, combinations with Tags/Curation, refreshed metadata,
source replacement, and focus when a selected row disappears. Check Default and
200% text at a normal window and 960×560; the new NAME control must leave critical
toolbar actions reachable. Record actual results and the exact build; then add
Harvest's family control once its bounded facts/filter route exists. Restart
fixtures must explicitly distinguish the existing session lifetime from durable
preferences rather than infer persistence from a successful in-session toggle.
