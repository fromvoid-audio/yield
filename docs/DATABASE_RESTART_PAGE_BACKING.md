# Database binding restart page backing

The optional-input binding/slice restart scan prepays its original count,
key-shape and DISTINCT UUID page producers on the same input pool. The original
two 8192-row relation ceilings run before key/page interpretation. Original
64-row pages complete all scalar conversions before UUID validation, so later
UTF-8/driver errors keep precedence over an earlier malformed UUID.

Static original SQL uses the existing private borrowed SQLite statement bridge.
The pinned Row scalar conversion helper is shared here and by the attempt
readers. This only shares conversion/diagnostic behavior: callers remain
responsible for funding owned conversions. UUID projections borrow valid TEXT;
NULL/type/UTF-8 fallback preserves the original driver's diagnostics. Native
SQLite messages are separately funded before Rust ownership.

Working headers, fixed errors, cursor strings and each page's geometric Vec
capacity are prepaid before their producers. Existing funded immutable inventory,
complete operation visitor, recording control, command digest and claim
comparison readers run beneath that same page. No owned record or binary image
escapes into a replacement producer. Page storage dies before its credit refunds.
Selected failure transfers working/page/native loans into the same input error
carrier, preserving nested producer loans already retained there.

The unfunded compatibility and recording-prestart routes retain their original
query and decoding order. Some historical inner-reader error labels already
differ between the funded and compatibility routes; nested-error tests compare
the funded original producer to preserve that established label.

Real owned SQLite tests cover empty and complete scans, no owned point-record or
binary-image copy, whole-page UTF-8 precedence, refusal before actual page growth,
native errors and nested inventory error lifetime after caller destruction. The
full library/docs, agent preflight, GNU Windows compilation and existing System
historical allocation regression are required. That regression does not measure
these new pages; compilation is not native positive capacity qualification.

Recording restart pages/chunks, transaction settlement, post-intent provider
growth and the full Legacy admission worker remain separate work under OPT-2373.
No stored UUID, inventory, receipt or page supplies live capacity, execution or
settlement authority.
