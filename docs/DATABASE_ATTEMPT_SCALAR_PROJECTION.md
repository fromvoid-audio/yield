# Original database attempt scalar projection

OPT-2411 preserves the original schema-12 start/outcome, audit-page and nonce SQL,
image ceilings, UUID acceptance and error order. SQLite `length(TEXT)` stops at NUL;
a 36-character prefix can hide a megabyte suffix copied by the old String getter.
Valid UTF-8 TEXT now produces a fixed UUID result and canonical flag directly from
the borrowed row. No input string payload is retained.

Required columns preserve the pinned required String NULL/type/UTF-8 errors before
image decoding. UUID parse results in these columns are comparison data: the original
image decode and outcome validation still precede the original column/image mismatch.
Audit pages emit parse errors only after the complete page's SQL projections succeed,
and preserve canonical/non-nil checks. Nonce readback preserves its original parser
without adding a canonical check. Accepted canonical cursor text is reconstructed
only after validation. Optional page cells are compile-time bounded to 72 bytes.

Real profile-owned SQLite fixtures retain complete start/outcome images, corrupt all
seven copied column fields and both attempt-key tables with megabyte NUL tails, and
compare durable evidence before and after refused reads and actual reopen. Helper
fixtures exercise required/nullable driver diagnostics, accepted UUID forms, deferred
page error precedence and image-before-column semantic precedence. The copy counter
measures only returned owned scalar String bytes; it does not measure image decoding,
SQLite workspace, collection backing or whole-process allocation.

These are synthetic durable observations, not genuine participant receipts or native
qualification. Original attempt history/conservation, funding, issuance, consumption,
Start, retry/refund and settlement acceptance remains open under OPT-2168 and the
other tracking parents. No schema, pool, quota or semantic envelope changes.
