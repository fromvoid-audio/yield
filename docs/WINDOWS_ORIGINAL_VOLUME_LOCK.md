# Original Windows volume lock validation (OPT-2363)

The existing volume owner validates its original held lock before using it and during retained revalidation. Windows now reads native handle information and refuses non-disk, directory/device/reparse, zero volume/object identity and non-singleton links. It then compares the original handle's owner SID with the current process account SID. Both native queries use fixed aligned 512-byte stack buffers. Returned lengths, complete headers, relative owner offset, token SID pointer span, SID revision and subauthority count/body are checked before interpretation. No pathname security query, dynamically sized native allocation or security setter participates. A final original-handle shape/identity check follows owner comparison.

This is original-object/account observation. OWNER equality does not prove a private DACL or secure ancestors, and a volume serial is not a persistent volume UUID. No canonical registry/root, protected reserve, staging creation, physical publication, permit, refund or settlement authority is enabled.

The validator itself is read-only. `open_lock` already performs `open(create=true)` before validation; a refusal can therefore follow creation of an entry. Existing pathname/error evidence and descriptor drop behavior are preserved. This is not a clean no-effect result or implemented rollback/failure capsule. OPT-2352/2349 still own exact post-effect backing/custody. The existing Windows `sync_directory` no-op remains an unresolved namespace durability contract; it has not been changed into a new post-effect refusal.

Portable parser/record tests exercise actual helper boundaries, without native account or physical qualification. The compiled Windows body invokes the original production validator on genuine current-user-owned files, held-handle rename/foreign replacement, actual hardlinks and NUL refusal. Native execution remains OPT-2364, on actual Windows/NTFS with an ordinary unelevated account, SDK/toolchain and real hardlink/rename support. Record nonzero counts, exact merged SHA and preserved original/foreign bytes. An elevated process whose files default to Administrators ownership must not weaken exact account comparison.

Native command:

```sh
cargo test --locked -p yield-library --lib original_windows_lock -- --nocapture
cargo test --locked -p yield-library --lib windows_original_lock_validator_retains_exact_handle_and_refuses_hardlinks -- --nocapture
```

The compiled nonignored foreign-account test requires `YIELD_WINDOWS_FOREIGN_LOCK_FIXTURE` to name a controlled disposable regular singleton owned by a second Windows account, readable by this account, and at most 128 bytes. Run `cargo test --locked -p yield-library --lib windows_original_lock_validator_refuses_genuine_foreign_account_fixture -- --nocapture` with that explicit native fixture. It reads the original no-follow handle, requires `LockNotOwned`, and checks unchanged bytes/length; it never changes ownership or silently skips a missing fixture. Full OPT-2215/2216/2218/2219 acceptance and canonical root/DACL/persistent identity/reserve/creation contracts remain open.
