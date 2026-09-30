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

Validation on the Linux development host uses normal lint settings through
`scripts/gpui-development-checks.sh test`; the app check, all core tests and
focused GPUI family/layout fixtures pass. The full headless UI suite also passes.

The lifecycle fixtures drive the real source projection retirement and scan
installation routes. They verify that suspended choices survive retirement,
inactive-source actions do nothing, and re-enable projects only replacement
identities with current metadata. A recreated Browser receives a preference
snapshot with persisted text scaling while its session-only Name/Type/Rating
switches and choices return to defaults. This is constructor/preference-reload
evidence, not an operating-system process restart or durable browser-preference
claim. Native restart/AX acceptance remains in OPT-1954.

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
