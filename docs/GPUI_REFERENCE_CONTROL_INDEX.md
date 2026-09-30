# Reference control source index (OPT-1867)

Generate a local, reproducible navigation index before reviewing the macOS
action matrix:

```sh
python3 scripts/internal/check/index_gpui_reference_controls.py --self-test
python3 scripts/internal/check/index_gpui_reference_controls.py > /tmp/yield-reference-controls.json
```

The index records exact checkout identity, SHA-256 hashes of every input, and
one-based source witnesses for each business-message reference in archived
native chrome. It separately lists production GPUI function declarations.
At baseline `c04e28c37a7ce3396d259764ce21b58f6f3fe117`, the reference index
contains 175 distinct messages: 37 FolderBrowser, 105 Gui, 15 Metadata, and
18 Settings. These are **source references**, including forwarding and match
arms, rather than a count of reachable user controls. Internal widget messages
and the broad controller `UiAction` enum are outside this finite index.

Comments, literals, and structural `cfg(test)` modules are excluded using the
same lexical scanner as the GPUI owner guard. Platform conditions, aliases,
macro-generated routes, keyboard bindings outside native chrome, and runtime
visibility still require source review. File hashes make local edits visible;
the checkout SHA alone does not establish the identity of uncommitted inputs.

For each reviewed control, OPT-1867 still requires its actual GPUI handler,
meaningful automated test, native/manual evidence, backend owner, and concrete
open blocker. Similar function names do not satisfy that requirement. Keep
forwarding messages and dormant proposals distinct from active controls, and
check keyboard/controller routes as well as chrome before declaring coverage
complete. The index does not close OPT-1867 or release acceptance OPT-1863.

The destructive-edit decision remains in
[GPUI_REFERENCE_EDIT_ACTIONS.md](GPUI_REFERENCE_EDIT_ACTIONS.md) under
OPT-1734. Unsupported publication and cleanup remain unmet under OPT-1868;
the earlier unsupported decision OPT-1474 does not establish parity. Current
implementation and historical native evidence live in
[GPUI_MIGRATION.md](GPUI_MIGRATION.md). Remaining native checks are tracked in
the Linear Mac queue OPT-1953; do not infer fresh native acceptance from this
index or Linux test results.
