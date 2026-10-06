# Original initial attempt record backing (OPT-2416)

Initial `DatabaseAttemptStart::validate_bound` borrows the complete unchanged
stored operation. Previously it cloned every owned record field before calling
the same context validator despite making no changes. Historical validation
continues to clone the record and overlay the stored attempt phase/disposition;
its codec normalization and error ordering are unchanged.

The actual SQLite regression settles a legal record with a 256-byte intent
label, reads that original record, and persists its first attempt. A private
test observer checks equal label bytes and distinct original/copied String
backing at the original clone site, then records the copied String capacity.
This witnesses one actual owned allocation only. It does not measure other
record fields, maps, serializer/command/inventory backing, SQLite allocations,
allocator rounding or whole-process peak. Legal attempt records require empty
opaque payloads; an invalid large payload is not evidence for this path.

Initial insertion/readback bytes and digest, current validation error precedence,
and historical overlays remain covered. Eliminating this initial record copy
does not fund complete attempts or recording startup. Historical record copies,
command JSON, complete audit collections, original SQLite/input funding and
prestart attempt/full-progress zero-row gates remain unresolved. Journal schema
and original ceilings are unchanged. Logical retained observations issue no
permit, retry/refund/reservation-release or genuine settlement authority and
provide no native physical qualification.
