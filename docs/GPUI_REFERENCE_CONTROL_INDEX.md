# Reference control source index (OPT-1867)

Generate a local, reproducible navigation index before reviewing the macOS
action matrix:

```sh
python3 scripts/internal/check/index_gpui_reference_controls.py --self-test
python3 scripts/internal/check/index_gpui_reference_controls.py > /tmp/yield-reference-controls.json
```

The index records exact checkout identity, SHA-256 hashes of every input, and
one-based source witnesses for each business-message reference in archived
native chrome, native keyboard routes and waveform interactions. It separately
lists production GPUI function declarations. The corrected source review at
baseline `006e92fd58437fbe0d3e6951d0c47becfa6fa707` contains 242 distinct
references. The earlier 175-message chrome-only index included standalone test
files and omitted keyboard/waveform routes; it must not be used as a coverage
denominator. These are **source references**, including forwarding and match
arms, rather than a count of reachable user controls. The broad controller
`UiAction` enum remains outside this finite native-app scope.

Comments, literals, and structural inline `cfg(test)` modules are excluded using
the same lexical scanner as the GPUI owner guard. The index also excludes
external `#[cfg(test)] mod name;` files and their module subtrees, recording
excluded paths explicitly. Platform conditions, custom module path attributes,
aliases, macro-generated routes and runtime visibility still require review.
File hashes make local edits visible;
the checkout SHA alone does not establish the identity of uncommitted inputs.

The source review is recorded per reference in
[GPUI_REFERENCE_ACTION_REGISTER.md](GPUI_REFERENCE_ACTION_REGISTER.md), with
GPUI control/model anchors, scoped automated fixtures, explicitly missing
evidence and open owners/blockers. Its machine-readable register is validated
against this index; duplicate, missing or retired references and broken
function/test anchors fail validation. Similar function names alone are not
semantic proof. Native reachability, aliases/macros and behavior remain open
in OPT-2009 under the Mac queue OPT-1953. Neither source artifact certifies
native parity or release acceptance OPT-1863.

The destructive-edit decision remains in
[GPUI_REFERENCE_EDIT_ACTIONS.md](GPUI_REFERENCE_EDIT_ACTIONS.md) under
OPT-1734. Unsupported publication and cleanup remain unmet under OPT-1868;
the earlier unsupported decision OPT-1474 does not establish parity. Current
implementation and historical native evidence live in
[GPUI_MIGRATION.md](GPUI_MIGRATION.md). Remaining native checks are tracked in
the Linear Mac queue OPT-1953; do not infer fresh native acceptance from this
index or Linux test results.
