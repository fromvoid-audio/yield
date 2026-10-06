# Original database claim scalar projection

OPT-2403 retains the original SQLite queries, inventory versions, relation ceilings
and semantic audits. Valid UTF-8 TEXT is borrowed from its row while exact binding
and slice fields are compared. UUID pages retain a fixed-size parsed result and
canonical flag, rather than an owned input string. SQLite `length(TEXT)` ends at
NUL; a valid-looking 36-character prefix can otherwise conceal an unbounded owned
suffix, even when the original SQL shape checks succeed.

UUID parse errors remain deferred until the original validation point after page
collection. Later driver type or UTF-8 failures therefore retain their original
precedence. NULL, wrong SQL types and invalid UTF-8 use the pinned nullable String
conversion, preserving its original column/type/error diagnostics. Canonical,
non-nil checks remain only where originally required. No accepted UUID form,
inventory version or physical-key rule changes.

The complete optional UUID page cell is compile-time bounded to 72 bytes. Its
maximum three simultaneous vector capacities fit the original 216-byte per-row
SQL page allowance; input TEXT payload is never retained by the projection.
This bounds application scalar projection, not SQLite's own query workspace,
complete collection backing, whole-process memory or physical storage.

Real profile-owned SQLite regressions preserve complete retained claim rows,
exercise megabyte NUL suffixes and earlier malformed UUID/later UTF-8 error order,
and retain valid inventory and nonce readback. The fixtures contain synthetic
storage observations and issue no live permits, reservations, retries, refunds,
Start admission, native qualification or genuine settlement. OPT-2168, OPT-2145
and the complete WAL/recording acceptance remain open.
