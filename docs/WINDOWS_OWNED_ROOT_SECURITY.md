# Windows owned-root security snapshot

`ProductionRootCapability::validate` checks the exact retained profile or profile-child directory before original FileOperationOwner registration and at existing root revalidation boundaries. External Source roots retain their separate authority contract. The helper receives only the original directory File; it never looks up security metadata by path and does not set an ACL.

The snapshot requires a local disk, non-reparse, non-device directory with nonzero original identity before and after querying OWNER and DACL. The owner must equal the current process TokenUser SID. A fixed, aligned 512-byte TokenUser buffer and 16-KiB security descriptor buffer reject larger replies. Pointer arithmetic, complete SID bodies, self-relative descriptor offsets, ACL sizes and at most 64 ACEs are checked before interpretation.

The accepted conservative grammar uses revision-1 SIDs, revision-2/4 ACLs, ordinary allow/deny ACEs, the five documented inheritance flags, and SDK-checked directory access/control masks. An applicable foreign-SID allow containing ADD_FILE, ADD_SUBDIRECTORY, WRITE_EA, DELETE_CHILD, WRITE_ATTRIBUTES, DELETE, WRITE_DAC, WRITE_OWNER, GENERIC_WRITE or GENERIC_ALL refuses. SYSTEM and Administrators receive no exemption. Read/execute and synchronization grants are permitted. Inherit-only ACEs do not apply to the current directory; a deny never cancels a foreign allow for this conservative decision. Unknown shapes, rights, controls, malformed spans and null DACLs refuse. An empty DACL is valid but gives no new authority.

Consequently default Windows directories may refuse. This is intentional observation policy, not permission repair. A descriptor snapshot cannot establish secure ancestors, canonical OS per-user bootstrap, persistent volume UUID, immutable ACL lifetime, exclusion of existing writable handles or mappings, created-stage provenance, permits or namespace admission. OPT-2218 and OPT-2363 keep those contracts open.

## Bounded scratch and original backing

The simultaneous Rust payload is 16,384 security bytes plus 512 token bytes, two 24-byte identity values, a 52-byte native file-information record, a 16-byte TOKEN_USER record, native handle/output scalars and parser references. It remains below 18 KiB, with no descriptor-sized allocation, native diagnostic string or token-pointer dereference before span validation. Nested identity calls do not retain a second native record. Kernel token storage is shared native state, not owned Rust bytes.

Root registration backing is the separate OPT-2398 prerequisite: the original root construction/control request must fund this scratch before validation, and existing effect request input backing remains live at revalidation. The security observation cannot create or refund that funding. Final integration must be rebased onto the actual verified registration-backing merge before claiming this budget connection.

## Verification

Portable tests exercise every mutation/read bit, SYSTEM/Administrators parity, exact owner equality, inherit-only and deny behavior, unknown rights/ACE/control grammar, all truncations, finite limits and checked token pointers. The Windows conditional harness uses externally prepared private, foreign-mutable and foreign-owned directories, plus genuine original Owner registration/revalidation and refusal. No harness sets ACLs.

Native qualification requires the linked Windows verification issue: current unelevated token, local NTFS/MSVC, exact fixture owner/DACL evidence and actual test exits. Positive descendants require externally prepared inheritable owner-only ACLs; TEMP/TMP must also point to a private inheritable parent for existing native positive suites. Cross-compilation does not supply native execution evidence.
