//! Original namespace working loan and selected-error transfer.
//! Original startup and admission dispatch retain selected failures on their reply.
use super::*;
use crate::operation_journal::owner::InputPermit;

impl OperationJournalStore {
    pub(in crate::operation_journal) fn validate_namespace_with_input(
        &self,
        input: Option<&InputPermit>,
        precommit: bool,
    ) -> Result<(), OperationJournalStoreError> {
        match input {
            Some(input) => self.validate_namespace_for_input(input, precommit),
            None if precommit => self.validate_namespace_for_precommit(),
            None => self.validate_namespace(),
        }
    }
    /// Fund the same original namespace at the startup/admission boundary.
    /// Existing unfunded callers retain their original entry points.
    pub(super) fn validate_namespace_for_input(
        &self,
        input: &InputPermit,
        precommit: bool,
    ) -> Result<(), OperationJournalStoreError> {
        let bytes = working_bytes(
            &self.database_root,
            &self.profile_guard,
            &self.database_path,
        )
        .ok_or(OperationJournalStoreError::OrdinaryRebuildResourceBound)?;
        let mut loan = NamespaceLoan::new(input, bytes)?;
        let result = (|| {
            if !precommit && !self.connection.is_autocommit() {
                return Err(audit_error(
                    &self.database_path,
                    "operation journal connection has an active transaction",
                ));
            }
            self.database_root
                .validate_profile_guard(&self.profile_guard)?;
            validate_database_file_identity(
                &self.database_root,
                &self.database_path,
                &self.database_identity,
            )?;
            // The first callback is the path already prepaid by working_bytes.
            // The second, when present, identifies the full actual selected reason.
            let mut path_copy = true;
            sqlite_identity::validate_connection_identity_with_funding(
                &self.connection,
                &self.database_path,
                &self.database_identity,
                |bytes| {
                    if path_copy {
                        path_copy = false;
                        if bytes != self.database_path.as_os_str().as_encoded_bytes().len() {
                            return Err(OperationJournalStoreError::OrdinaryRebuildResourceBound);
                        }
                        Ok(())
                    } else {
                        loan.reserve_native_message(bytes)
                    }
                },
            )
            .map_err(|error| match error {
                sqlite_identity::ConnectionIdentityFailure::Observed(error) => {
                    map_identity_error(error)
                }
                sqlite_identity::ConnectionIdentityFailure::Funding(error) => error,
            })?;
            reject_unexpected_sidecars(&self.database_root, &self.database_path, true)
        })();
        loan.finish(result)
    }
}

pub(super) struct NamespaceLoan<'a> {
    input: &'a InputPermit,
    working: Option<InputPermit>,
    native: Option<InputPermit>,
}

impl<'a> NamespaceLoan<'a> {
    pub(super) fn new(
        input: &'a InputPermit,
        bytes: usize,
    ) -> Result<Self, OperationJournalStoreError> {
        // Reject before entering the original namespace when there is nowhere
        // to retain a selected owned failure. Existing observer credits stay intact.
        if !input.can_retain_namespace_failure() {
            return Err(OperationJournalStoreError::OrdinaryRebuildResourceBound);
        }
        let bytes = bytes
            .checked_add(std::mem::size_of::<Self>())
            .ok_or(OperationJournalStoreError::OrdinaryRebuildResourceBound)?;
        let working = input
            .reserve_historical_scratch(bytes)
            .map_err(|_| OperationJournalStoreError::OrdinaryRebuildResourceBound)?;
        Ok(Self {
            input,
            working: Some(working),
            native: None,
        })
    }

    /// The actual captured SDK message is charged before its full Rust copy.
    /// Path and enclosing error/consumer scratch belong to the enclosing loan.
    pub(super) fn reserve_native_message(
        &mut self,
        bytes: usize,
    ) -> Result<(), OperationJournalStoreError> {
        if self.native.is_some() {
            return Err(OperationJournalStoreError::OrdinaryRebuildResourceBound);
        }
        // Captured reason, Store Display image, worker diagnostic and retained
        // owner diagnostic can coexist. Their variable bodies all derive from
        // this same full measured reason; framing/headers belong to working_bytes.
        // Std IO's fixed native buffer bound cannot stand in for an SDK message.
        let display =
            format_bytes(bytes).ok_or(OperationJournalStoreError::OrdinaryRebuildResourceBound)?;
        let charged = bytes
            .checked_add(display)
            .and_then(|bytes| bytes.checked_add(display))
            .and_then(|bytes| bytes.checked_add(display))
            .ok_or(OperationJournalStoreError::OrdinaryRebuildResourceBound)?;
        self.native = Some(
            self.input
                .reserve_historical_scratch(charged)
                .map_err(|_| OperationJournalStoreError::OrdinaryRebuildResourceBound)?,
        );
        Ok(())
    }

    pub(super) fn finish(
        mut self,
        result: Result<(), OperationJournalStoreError>,
    ) -> Result<(), OperationJournalStoreError> {
        match result {
            Ok(()) => Ok(()),
            Err(error) => Err(self.input.retain_namespace_failure(
                error,
                self.working
                    .take()
                    .expect("original namespace working loan"),
                self.native.take(),
            )),
        }
    }
}

// The sum deliberately prepays sequential alternatives as well as live overlap.
// No pathname quota is introduced: derive every variable term from this Store's
// original retained metadata. Checked overflow is a resource refusal.
pub(super) fn working_bytes(
    root: &BoundDatabaseRoot,
    profile: &WritableProfileGuard,
    database_path: &Path,
) -> Option<usize> {
    let paths = root
        .participant_clone_bytes()?
        .checked_add(profile.participant_clone_bytes()?)?
        .checked_add(database_path.as_os_str().as_encoded_bytes().len())?
        .checked_add(JOURNAL_DATABASE_FILE_NAME.len())?
        .checked_add(SQLITE_JOURNAL_SUFFIX.len())?;
    // Every malformed encoded byte can become a three-byte replacement character.
    let displayed = paths.checked_mul(3)?;
    // Rust 1.93 native OS-error buffers; this is formatter scratch, not a cap on
    // the SDK HRESULT message captured separately by reserve_native_message.
    #[cfg(windows)]
    let io_text = 2048usize.checked_mul(3)?;
    #[cfg(unix)]
    let io_text = 128usize.checked_mul(3)?;
    #[cfg(not(any(unix, windows)))]
    let io_text = 0usize;
    // Sum all original namespace diagnostic literal pieces conservatively.
    const TEXT: &str = concat!(
        "root is not a non-symlink directory: ",
        "root has no final component: ",
        "stable filesystem identity is unavailable",
        "failed to revalidate retained database root without following symlinks: ",
        "failed to inspect retained database root during revalidation: ",
        "failed to inspect bound source-root capability: ",
        "source database root binding namespace is unavailable at : ",
        "source database root binding namespace identity is uncertain at : ",
        "source database root binding namespace was replaced (identity mismatch): ",
        "profile ownership filesystem operation failed at : ",
        "profile ownership namespace unavailable at : ",
        "profile ownership identity unavailable at ",
        "profile root was replaced: ",
        "profile owner lock was replaced: ",
        "profile child database namespace unavailable at : ",
        "profile child database namespace identity is uncertain at : ",
        "profile child database namespace was replaced: ",
        "operation journal filesystem operation failed at : ",
        "operation journal at  requires audit: ",
        "operation journal database was replaced: ",
        "operation journal cannot verify SQLite connection identity: ",
        "SQLite connection identity check failed for : ",
        "operation journal connection has an active transaction",
        "journal database is not a regular file",
        "stable identity is unavailable for the open journal file",
        "SQLite sidecar  is a symlink",
        "SQLite sidecar  is not a regular file",
        "unexpected SQLite sidecar  is present",
        " (os error )",
        "OS Error  (FormatMessageW() returned error )",
        "OS Error  (FormatMessageW() returned invalid UTF-16)"
    );
    let custom = format_bytes(displayed.checked_add(TEXT.len())?)?;
    let reason = format_bytes(custom.checked_add(io_text)?.checked_add(TEXT.len())?)?;
    let binding = format_bytes(displayed.checked_add(reason)?.checked_add(TEXT.len())?)?;
    let consumer = format_bytes(displayed.checked_add(binding)?.checked_add(TEXT.len())?)?;
    let formatting = format_bytes(io_text)?
        .checked_add(custom)?
        .checked_add(reason)?
        .checked_add(binding)?
        .checked_add(consumer)?;
    // Single-normal-component cap-primitives fallback: original relative Vec,
    // four-slot Cow component Vec, one retained parent-directory slot. Neither
    // original no-follow traversal nor a normal direct child expands symlinks.
    let components = 4usize
        .checked_mul(std::mem::size_of::<&std::ffi::OsStr>())?
        .checked_add(4 * std::mem::size_of::<[usize; 4]>())?
        .checked_add(std::mem::size_of::<(usize, usize, File)>())?;
    // Prepay CString conversion, rebuilt PathBuf, wide encoding plus terminator,
    // and Windows Unicode uppercase device-name classification. Alternatives
    // remain prepaid on Unix too. Original native queries are unchanged.
    let path_work = paths
        .checked_add(1)?
        .checked_add(paths.checked_mul(2)?)?
        .checked_add(paths.checked_add(1)?.checked_mul(4)?)?
        .checked_add(format_bytes(displayed)?)?;
    // Six original root/lock/named-root/alias/main identities; decimal u64/u128
    // representation is at most unix: +20+1+20+1+39 = 86 UTF-8 bytes.
    let identities = format_bytes(5 + 20 + 1 + 20 + 1 + 39)?.checked_mul(6)?;
    let headers = std::mem::size_of::<OperationJournalStoreError>()
        // Selected queued error's non-Clone carrier frees this Box before its
        // reply keeper. It is constructed only after the original error is sent.
        .checked_add(std::mem::size_of::<
            super::super::owner::OperationJournalOwnerError,
        >())?
        .checked_add(std::mem::size_of::<DatabaseIdentityError>())?
        .checked_add(std::mem::size_of::<ProfileOwnershipError>())?
        .checked_add(std::mem::size_of::<(String, io::ErrorKind, [usize; 4])>())?
        .checked_add(6 * std::mem::size_of::<String>())?;
    paths
        .checked_add(formatting)?
        .checked_add(components)?
        .checked_add(path_work)?
        .checked_add(identities)?
        .checked_add(headers)
}

fn format_bytes(output: usize) -> Option<usize> {
    // Pinned format! estimates at most twice its literal length; String growth
    // doubles capacity. Eight covers the byte-vector minimum nonzero capacity.
    output.checked_mul(2).map(|bytes| bytes.max(8))
}
