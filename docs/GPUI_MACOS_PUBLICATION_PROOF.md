# macOS publication and cleanup primitive proof (OPT-1869)

Status: no reviewed production route qualifies under the current ownership
contract. This is a bounded primitive result, not a claim that macOS lacks
ordinary atomic saves. Keep OPT-1868 and the macOS release gate open.

## Contract obligations

`IO_ARCHITECTURE_TARGET.md` invariants 19–22 require independent proof of:

1. **Owned source object:** final publication consumes the already opened,
   validated transaction-owned staged object, even if its directory entry is
   replaced by another same-user process. Matching contents or a UUID name do
   not prove ownership.
2. **Root authority:** source and target directory operations stay relative to
   verified no-follow directory capabilities and check live filesystem identity.
   A replaced root path must not redirect a held capability.
3. **Final claim:** an atomic no-replace claim must preserve both the owned
   source and an unrelated final on collision. The result must identify the
   transaction's published object so a later name substitution cannot be
   mistaken for successful publication or adoption.
4. **Cleanup and compensation:** removing staging or a published object must
   target the verified transaction-owned object, not whatever currently has
   its former name. Cleanup is independently qualified; publication support
   cannot imply cleanup support.
5. **Ordering and durability:** validated complete staging must be durably
   recorded as `CopyValidated` before claim or recovery continuation. File and
   directory synchronization, reopened identity, crash replay, and capacity
   settlement need separate qualification. No syscall probe establishes these.

Same-user namespace substitution is in scope. A mode-0700 staging directory or
process-local lock does not remove this adversary from the contract.

## Candidate API result

| Primitive | Documented useful guarantee | Missing contract property |
| --- | --- | --- |
| `renameatx_np` with `RENAME_EXCL` | Directory-relative source and target; existing final returns `EEXIST` on a supporting filesystem. | Source operand is a mutable name, not the held staged file. The call returns no object handle. Substitution before the call can publish an unrelated object. `RENAME_SWAP` intentionally replaces/exchanges the final and cannot claim an absent name. |
| `fclonefileat` | Held source fd, destination directory fd, absent destination, and an expected atomic clone on supporting same-volume filesystems. | Success returns an integer status, no destination fd or identity token. A later reopen by name can see a substituted entry; source and final are distinct objects, and successful clone leaves staging needing independently safe cleanup. Its availability is filesystem dependent. |
| `unlinkat` | Held parent directory fd and relative entry name; no-follow options can reject traversal through links. | Target is still the current name entry. There is no expected file identity operand; a same-user substitution between check and unlink can delete an unrelated object. `AT_UNIQUE` limits hard links, and `AT_NODELETEBUSY` rejects open files; neither binds deletion to the held staged fd. |
| Path-based `rename`, `unlink`, `FileManager` replace | Common macOS save operations. | They do not provide the required held source identity and no-replace/cleanup authority under this threat boundary. |

Apple documents the signatures and scope in the macOS
[`rename(2)`](https://github.com/apple-oss-distributions/xnu/blob/main/bsd/man/man2/rename.2),
[`clonefile(2)`](https://github.com/apple-oss-distributions/xnu/blob/main/bsd/man/man2/clonefile.2),
and [`unlink(2)`](https://github.com/apple-oss-distributions/xnu/blob/main/bsd/man/man2/unlink.2)
manuals. Apple's [APFS API guide](https://developer.apple.com/library/archive/documentation/FileManagement/Conceptual/APFS_Guide/ToolsandAPIs/ToolsandAPIs.html)
lists the descriptor variants. These describe supported operations, not a
composed ownership or power-loss guarantee. The current SDK's local manual also
documents `AT_NODELETEBUSY` and `AT_UNIQUE` for `unlinkat`.

## Isolated host probe

On macOS 26.6.2 (25G83), APFS Data volume `/dev/disk3s5`, run:

```sh
cc -Wall -Wextra -Werror scripts/qa/macos_publication_probe.c -o /private/tmp/wavecrate-publication-probe
/private/tmp/wavecrate-publication-probe
```

The probe creates and removes its own `/private/tmp/wavecrate-publication-probe.*`
fixture. On this host it observed:

- A held directory fd stayed on the original directory after its path was
  renamed and replaced. This is useful root anchoring, but live identity and
  expected location still need verification for production admission.
- Replacing the staged name after opening the owned file made `renameatx_np`
  publish the unrelated replacement. A final collision returned `EEXIST` and
  preserved both entries.
- `fclonefileat` cloned the held source and preserved an unrelated collision
  target, but replacing the final name before reopen made reopen return the
  unrelated replacement. The syscall gives no resulting handle to disambiguate.
- Replacing the cleanup name after opening the owned file made `unlinkat`
  remove the unrelated replacement while preserving the held owned object.

These are deterministic, orchestrated substitution fixtures; they show API
operand semantics on this host. They are not a scheduler race stress test, a
cross-filesystem qualification, or a power-failure test. The exact fixture
cannot prove that every possible macOS API is inadequate. No documented
candidate reviewed here satisfies both publication and cleanup obligations.

## Decision and follow-up

Keep `StagedFileFinalizationCapabilities` publication and cleanup unsupported
on macOS. Do not add a pathname fallback or interpret successful clone/reopen
as transaction ownership. Production recovery must continue retaining staged
data and journal evidence on unsupported finalization. No production behavior
changes are authorized by this proof.

OPT-1868 remains a release blocker. A future bounded design task needs a
documented primitive that accepts the held staged object and returns or
otherwise retains verifiable authority over the resulting final object, plus
an independently qualified identity-bound cleanup/disposition primitive. Only
then should implementation, APFS/filesystem-version qualification, collision
and substitution tests, sync/crash replay, and production admission be split
into separate tasks. If those primitives are unavailable, the macOS release
scope requires an explicit contract/product decision; this proof does not
silently weaken invariants 19–22.
