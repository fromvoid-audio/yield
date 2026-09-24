# GPUI browser accessibility adapter

The Wavecrate browser attaches one `AxRegistry` to its real macOS GPUI window. The registry follows the browser entity's lifetime. AX callbacks enqueue bounded events; the browser rechecks current identities and dispatches them on its GPUI window context.

This slice exposes Add source, visible source rows and their Forget buttons, visible folder rows and their expand buttons, Folder scope, Refresh source, and visible sample rows. Names, selection, counts, scope, errors, and enablement come from the same browser state as the drawn controls. GPUI `uniform_list` supplies the actual visible row ranges; prepaint canvases supply each control's actual frame. Unchanged AX snapshots are not reconciled again on every browser render.

## Native validation

On macOS 26.6.2, a debug `wavecrate-gpui` build was launched with temporary sources. `AXUIElement` queries found `AXWindow` children with expected button and row roles, names, values, enabled state, and nonzero frames. Native AX focus succeeded for a source row. Native `AXPress` on Folder scope changed `Direct only` to `All descendants`; on a folder row it changed the selected folder and kept focus; on a folder toggle it changed `Collapsed` to `Expanded` and exposed the child row; on sample `b.wav` it changed selection from `a.wav` to `b.wav` and kept focus; on a temporary source's Forget button it removed both source AX children. With 80 sample files, the native AX tree contained 19 sample rows, matching GPUI's visible list range rather than the entire source. The temporary source scan reported a local session persistence warning, so these results validate UI and AX behavior only.

Browser search is not exposed by this adapter yet. A native experiment with an `AXTextField` child showed `AXValue` was not settable, while `AXPress` was advertised but rejected. OPT-1875 tracks that missing editable field backend contract and blocks completion of OPT-1749. The browser/source adapter remains partial until that dependency and remaining semantic controls are accepted.
