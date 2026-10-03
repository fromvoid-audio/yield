# Browser filter-family controls

This is the independent Name, Playback Type and Rating implementation portion of
[OPT-1907](https://linear.app/boostnlvp/issue/OPT-1907). It does not complete the
six-family native acceptance ticket. Harvest facts/filter integration is implemented
by OPT-1898 / PR #333 through the bounded owner API from OPT-1900. Existing Tags
and Curation behavior is retained.

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
toolbar actions reachable. Record actual results and the exact build; include the
implemented Harvest enable/dropdown and its source-scoped choices. Restart
fixtures must explicitly distinguish the existing session lifetime from durable
preferences rather than infer persistence from a successful in-session toggle.

## Six-family portable integration (OPT-2238)

Ordinary Tags, Curation and other read attempts previously canceled the token
retained by a Harvest mask, so a combined request silently lost its Harvest
delivery and timed out. The catalog now separately retains at most one Harvest
mask token per source (4,096 sources maximum). Unrelated reads preserve that exact
source/generation/token; a new Harvest request supersedes it. Writes, audits,
source replacement/removal and shutdown still invalidate it. These tokens only
fence read freshness; they confer no mutation or settlement authority. Exhausting
the retained-source bound returns the existing query-budget failure without
evicting another source's mask.

Clearing the active Browser projection now invokes Harvest's existing reinstall
hook, canceling pending requests and dropping completed mask custody while
preserving the old source session's choices.

The cross-family GPUI fixtures use the actual profile-owned global writer and
source/catalog service to produce Harvest and Tags masks and Curation tag facts.
Local hydrated rating/playback values are explicit test data, not mutation receipts.
They configure all six families together, suspend each through its production
handler, and verify the other five enable states and retained choices. The genuine
absent-tag mask must continue excluding rows when another family is suspended;
suspending Tags exposes only the intersection of the other predicates.

After suspending every family, changed local metadata tests that Rating, Playback
Type and Curation re-enable against the current projection. Name, the retained
absent-tag requirement, and the actual Touched Harvest mask each independently
restore their predicate. Replacing the catalog mapping invalidates the Harvest
mask and hides rows until a fresh coherent owner result arrives, even with other
families active.

A separate fixture reconstructs Browser, reloads text-scale preferences and
installs the same audited source into the fresh projection. All six families'
session state returns to defaults: Name/Type/Rating switches enabled with empty
choices, Tags/Curation/Harvest disabled. This is Browser reconstruction and
preference projection evidence, not OS process restart or durable filter storage.
Native six-family pointer/keyboard/AX and process restart remain OPT-1954 and
OPT-2224. OPT-1907 and its tracking parents remain open.

Reproduce with `cargo test --locked -p yield-ui --lib six_family`, the catalog
Harvest-filter and retained-mask tests, the full portable catalog/core/UI library
suites, and `bash scripts/agent.sh preflight` on the committed head. The existing
native finalization tests remain tracked by OPT-2026.
