# Original Windows staging read/share custody (OPT-2396)

The original FileOperationOwner target preparation consumes its readonly
staging observation handle through `ReOpenFile`, requesting `FILE_GENERIC_READ`
and `FILE_SHARE_READ` only. Original and returned native disk/regular/nonreparse
singleton identities must agree before hashing. The returned handle occupies
`BindingData.staged_file`, so sharing restrictions remain through hashing,
original worker revalidation and the final live prepared-target alias. No locator
is used by this lease acquisition. The original observation producer requests
readonly access; no held writable producer must be closed or transferred.

Windows sharing checks are symmetric. An existing handle with write or delete
access refuses lease acquisition; while the lease lives, a new such handle
cannot open. Other compatible readonly handles remain possible. Failure closes
only the newly attempted observation handles, preserves original staging bytes
and existing claims, and enables no namespace operation. The bounded original
input debit, deadline, root/source/volume validation and truthful Unsupported
worker result remain in force.

This is a file-handle sharing guarantee. A writable mapping can outlive its
creating handle and is not excluded by this lease. Trusted created-stage
provenance, mapped-alias exclusion, native effect rights, bounded namespace
allocation, synchronization and qualified physical policy remain implementation
requirements of OPT-2218. This lease is no qualification receipt and issues no
publication, retry, refund or settlement authority.

Two actual Windows tests exercise original readonly-handle acquisition, existing
write/delete conflicts, new conflicts, read compatibility, identity/byte
preservation and closure after the last retained Arc alias. Linux does not execute
these Windows APIs. GNU all-test compilation checks the actual native bodies;
Windows execution requirements are recorded separately in Linear.

The new producer owns no String, Vec, map or diagnostic allocation. Identity is
three scalar native fields (volume serial, file index, creation time), read into
a fixed BY_HANDLE_FILE_INFORMATION record. At most the captured identity and
one current identity plus one native record and two File handle values coexist;
these fixed values fit 256 bytes on the checked Windows ABI. Refusals use inline
io::ErrorKind values, never copied native error messages. The existing 64 KiB
request debit precedes acquisition; native Windows object/kernel allocations are
outside this Rust backing claim. Existing bounded canonical/proof construction
is unchanged. The two new original-owner tests additionally use actual original
preparation queues and BindingData aliases, root retirement, a lost duplicate
reply and genuine owner teardown. Helper tests alone prove no owner integration.
