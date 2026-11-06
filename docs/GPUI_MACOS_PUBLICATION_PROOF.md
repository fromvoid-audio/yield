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
| `openat` with `O_CREAT` and `O_EXCL` | Claims an absent leaf and returns the exact new file descriptor. Later writes stay on that object even if the name is replaced. | The final name is visible before the first byte is written. Writing through the descriptor cannot install a complete staged file at the final name or safely remove that name after a failure. |

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
cc -Wall -Wextra -Werror scripts/qa/macos_publication_probe.c -o /private/tmp/yield-publication-probe
/private/tmp/yield-publication-probe
```

The probe creates and removes its own `/private/tmp/yield-publication-probe.*`
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
- An exclusive final create refused a collision and returned the owned
  descriptor. A second open observed the empty file and then its partial
  contents. After name substitution, writes through the held descriptor reached
  the original object, and the final name held unrelated bytes.

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

## Approved scope for the first managed extraction route

The [follow-up review](https://pulse.iocore.dev/#/issues/6022) found no
supported descriptor operation that both atomically publishes a complete
output and supplies the final identity in an arbitrary same-user writable
directory. Darwin's file descriptor guards govern use of an fd. The
experimental vnode guard declarations are private. The public exclusive create
guarantee and the probe above establish ownership at creation and expose the
visibility gap. [Apple's open manual](https://github.com/apple-oss-distributions/xnu/blob/main/bsd/man/man2/open.2)
and [XNU guard definitions](https://github.com/apple-oss-distributions/xnu/blob/main/bsd/sys/guarded.h)
describe those interfaces.

The [approved scope](https://pulse.iocore.dev/#/issues/6023) is one dedicated
managed output source on a qualified local APFS volume. A separately
authorized, unprivileged service identity would own the staging and final
namespaces. Yield and DAWs running as the interactive user could read finished
media. The interactive identity has no staging access or permission to edit
or remove managed outputs. Existing arbitrary destinations and in-place edits
require their own qualification.

The service would bind a configured output root and traverse the protected
ancestors without following links. All path components would need ownership,
permission, ACL and volume checks. The service needs one writer per root
across restarts and must retain directory descriptors through each effect.
The threat includes an ordinary process of the interactive user, including
one that tries to replace a directory by changing its parent. Root, an
administrator who has elevated privileges, and compromise of the authorized
writer fall outside this boundary.

The client would send bounded requests with a destination leaf and a
read-only payload descriptor. The service must validate one leaf, reject
caller-supplied destination paths, copy into its private stage under its own
allocation budget, and validate the exact result before publication. A
mutating call needs both the expected signed app identity and its configured
local account. Apple provides an approval path for a bundled helper and an
XPC peer signing requirement. [Service Management](https://developer.apple.com/documentation/servicemanagement/smappservice)
and [XPC peer signing requirements](https://developer.apple.com/documentation/foundation/nsxpcconnection/setcodesigningrequirement%28_%3A%29)
describe those APIs.

With that boundary established, a candidate can keep the validated staged
descriptor through the same-volume exclusive rename and compare the reopened
final identity before registering the output. The journal would persist
`CopyValidated` before the claim. The service would need a qualified allocation
bound, reserve, and file and directory synchronization evidence. Recovery
would rebind the protected roots and verify identities before advancing. Cleanup
would require its own grant and verify the expected owned object and subsequent
absence within the protected namespace. A collision, unknown outcome, stale
root or unproved absence retains the applicable files, charge and journal
evidence. This candidate has no native helper or qualification evidence yet.

The design proof begins with a disposable APFS fixture and two actual local
identities. The reader must be able to open the finished media while the
filesystem denies changes to both leaf names and the protected directories.
The writer must show exact identity through exclusive publication, collision
preservation and separately checked cleanup. The profile-owned metadata
writer can register the committed destination after the filesystem
checkpoint. [Yield #6031](https://pulse.iocore.dev/#/issues/6031)

This approval covers the design and isolated proof. Service installation,
production enablement and use against a real library require separate evidence
and authorization.

### Run the two-identity fixture

On a local APFS Mac, run from the repository as an ordinary account with
permission to launch a command as the built-in unprivileged `nobody` account.
The probe creates a disposable directory under `/private/tmp` and invokes the
peer with noninteractive sudo. If needed, authenticate in the same terminal
before running it.

```sh
sudo -v
/usr/bin/python3 -I scripts/qa/macos_managed_output_probe.py
```

The writer line confirms exact staged/final identity, no-replace collision,
owned staging cleanup and readable WAV data. A separate peer line must confirm
that the WAV was readable and filesystem mutations were refused. Add
`--writer-only` to exercise the writer in a shell without peer access.
Only the two-identity run can establish peer refusal. The probe uses ordinary
file sync and a temporary sticky parent. The installed service, its production
ancestor permissions, crash recovery and directory durability require
separate qualification. [Yield #6031](https://pulse.iocore.dev/#/issues/6031)
