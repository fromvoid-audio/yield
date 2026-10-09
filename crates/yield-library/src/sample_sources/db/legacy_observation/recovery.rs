//! Historical current13 observation; receipt/intent matching never grants effects or ownership.
use super::*;
use crate::operation_journal::{
    OperationId, UniversalCommandArgs, legacy_adoption_commands::LegacyAdoptionIntentArgs,
};

/// Bounded historical evidence observed through current retained custody and the actual journal.
/// It carries no current lease, capacity admission, writable promotion or audit/revision grant.
pub struct LegacyAdoptionRecoveryImage {
    operation: OperationId,
    request: LegacyAdoptionIntentArgs,
    lease_table: bool,
}
impl LegacyAdoptionRecoveryImage {
    /// Original persisted operation identity, not a new admission or success grant.
    pub fn operation_id(&self) -> OperationId {
        self.operation
    }
    /// Immutable historical request matched to the actual original V19 journal row.
    pub fn request(&self) -> &LegacyAdoptionIntentArgs {
        &self.request
    }
    /// Observed table presence; this does not establish a live current Source lease.
    pub fn lease_table_present(&self) -> bool {
        self.lease_table
    }
}
impl SourceDatabase {
    /// Open bounded unfinished-adoption recovery through an existing retained sidecar.
    /// This never creates/replaces a sidecar, replays SQLite recovery or promotes a writer.
    pub fn open_legacy_recovery_read_owner(
        root: impl AsRef<Path>,
        profile: &WritableProfileGuard,
        child: impl AsRef<Path>,
    ) -> Result<LegacySourceReadOwner> {
        LegacySourceReadOwner::open_recovery(root.as_ref(), profile, child.as_ref())
    }
}
impl LegacySourceReadOwner {
    fn open_recovery(root: &Path, profile: &WritableProfileGuard, child: &Path) -> Result<Self> {
        let _extensions = crate::sqlite_ext::try_extension_free_metadata_scope()
            .ok_or(LegacyObservationError("SQLite extension runtime unknown"))?;
        profile.validate_current().map_err(refused)?;
        let database_root =
            Arc::new(BoundDatabaseRoot::for_profile_child(profile, child).map_err(refused)?);
        let media_root =
            Arc::new(BoundDatabaseRoot::for_external_source_root(profile, root).map_err(refused)?);
        absent_sidecars(&database_root)?;
        let name = Path::new(super::super::DB_FILE_NAME);
        let path = database_root.path().join(name);
        let original = sqlite_identity::open_existing_database_file(&database_root, name, &path)
            .map_err(refused)?;
        if original.file.metadata().map_err(refused)?.len() > 64 * 1024 * 1024 {
            return Err(LegacyObservationError("recovery main-file bound"));
        }
        let binding = Arc::new(sqlite_identity::DatabaseFileBinding::new(
            database_root.clone(),
            name,
            &path,
            original.identity,
        ));
        let reservation = SourceLeaseReservation::acquire_existing(
            database_root.clone(),
            media_root.clone(),
            profile,
        )
        .map_err(refused)?;
        reservation
            .validate_before_open(profile, &binding)
            .map_err(refused)?;
        absent_sidecars(&database_root)?;
        // The pre-open VFS fence also rejects hot-journal recovery and auxiliary mutations.
        let connection = database_root
            .open_existing_sqlite_no_effect(name)
            .map_err(refused)?;
        binding.validate(&connection).map_err(refused)?;
        connection.busy_timeout(Duration::ZERO).map_err(refused)?;
        connection.execute_batch("PRAGMA query_only=ON; PRAGMA temp_store=MEMORY; PRAGMA cache_spill=OFF; PRAGMA mmap_size=0").map_err(refused)?;
        let owner = Self {
            connection,
            binding,
            reservation,
            media_root,
            profile: profile.try_clone().map_err(refused)?,
            original_digest: None,
            main_reserve_root: None,
        };
        owner.read_recovery_receipt(profile)?;
        Ok(owner)
    }
    fn read_recovery_receipt(
        &self,
        profile: &WritableProfileGuard,
    ) -> Result<(
        crate::operation_journal::legacy_adoption_commands::receipt::AdoptionReceiptEnvelope,
        bool,
    )> {
        if !self.connection.is_autocommit() {
            return Err(LegacyObservationError(
                "recovery read transaction already active",
            ));
        }
        self.read_recovery_receipt_core(profile)
    }
    fn read_recovery_receipt_core(
        &self,
        profile: &WritableProfileGuard,
    ) -> Result<(
        crate::operation_journal::legacy_adoption_commands::receipt::AdoptionReceiptEnvelope,
        bool,
    )> {
        self.validate(profile)?;
        if !self.connection.validate_original_legacy_read_policy() {
            return Err(LegacyObservationError("recovery read fence changed"));
        }
        let mode: String = self
            .connection
            .pragma_query_value(None, "journal_mode", |r| r.get(0))
            .map_err(refused)?;
        if !mode.eq_ignore_ascii_case("delete") {
            return Err(LegacyObservationError(
                "unsupported recovery journal policy",
            ));
        }
        let lease: bool = self
            .connection
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE name='source_lease')",
                [],
                |r| r.get(0),
            )
            .map_err(refused)?;
        let receipt = super::super::schema::read_adopted_schema_receipt(&self.connection, lease)
            .map_err(refused)?;
        if !receipt
            .historical_request()
            .matches_recovery_source_bindings(
                profile.profile_root_identity(),
                self.media_root.identity(),
                self.binding.root().identity(),
                self.binding.identity(),
                self.reservation.observation_lock_identity(),
            )
        {
            return Err(LegacyObservationError("historical source bindings differ"));
        }
        self.validate(profile)?;
        Ok((receipt, lease))
    }
    /// Match a bounded canonical source receipt to the actual V19 row under this journal owner.
    /// This observation cannot resume a migration/bootstrap or release any claim. Later ordinary
    /// current-source edits use current ownership, rather than this unfinished-adoption reader.
    pub fn capture_recovery(
        &self,
        profile: &WritableProfileGuard,
        journal: &crate::operation_journal::store::OperationJournalStore,
    ) -> Result<LegacyAdoptionRecoveryImage> {
        self.capture_recovery_inspected(profile, |operation, request| {
            journal
                .validate_legacy_recovery_namespace(profile, request)
                .map_err(refused)?;
            let record = journal
                .get(operation)
                .map_err(refused)?
                .ok_or(LegacyObservationError("original recovery intent absent"))?;
            if record.schema_version != 19
                || record.operation_id != operation
                || !matches!(record.universal_command_args(), Some(UniversalCommandArgs::SourceLegacyAdoption(args)) if args == request)
            {
                return Err(LegacyObservationError("original recovery intent differs"));
            }
            journal
                .validate_legacy_recovery_namespace(profile, request)
                .map_err(refused)
        })
    }

    // The journal worker supplies its original funded complete-record visitor here. Its
    // InputPermit and decoded graph stay inside the worker; neither crosses the Source API.
    pub(crate) fn capture_recovery_inspected(
        &self,
        profile: &WritableProfileGuard,
        inspect: impl FnOnce(OperationId, &LegacyAdoptionIntentArgs) -> Result<()>,
    ) -> Result<LegacyAdoptionRecoveryImage> {
        let deadline = Instant::now() + Duration::from_secs(2);
        let _extensions = crate::sqlite_ext::try_extension_free_metadata_scope()
            .ok_or(LegacyObservationError("SQLite extension runtime unknown"))?;
        self.validate(profile)?;
        if !self.connection.validate_original_legacy_read_fence() {
            return Err(LegacyObservationError(
                "recovery acquisition is not quiescent",
            ));
        }
        let before: u64 = self
            .connection
            .pragma_query_value(None, "data_version", |r| r.get(0))
            .map_err(refused)?;
        let transaction = self.connection.unchecked_transaction().map_err(refused)?;
        let progress = Progress::install(&self.connection, deadline);
        let observed = (|| -> Result<_> {
            let (receipt, lease) = self.read_recovery_receipt_core(profile)?;
            let operation = receipt.historical_operation();
            let preserved = rows::capture_preserved(&transaction, deadline)?;
            if !receipt.matches_preserved_rows(preserved.digest) {
                return Err(LegacyObservationError("unfinished adoption rows changed"));
            }
            inspect(operation, receipt.historical_request())?;
            Ok((receipt, lease, operation))
        })();
        // Remove deadline interruption before the original read transaction closes.
        drop(progress);
        let (receipt, lease, operation) = observed?;
        transaction.commit().map_err(refused)?;
        let after: u64 = self
            .connection
            .pragma_query_value(None, "data_version", |r| r.get(0))
            .map_err(refused)?;
        self.validate(profile)?;
        if before != after
            || !self.connection.validate_original_legacy_read_fence()
            || Instant::now() >= deadline
        {
            return Err(LegacyObservationError(
                "recovery observation changed or expired",
            ));
        }

        Ok(LegacyAdoptionRecoveryImage {
            operation,
            request: receipt.historical_request().clone(),
            lease_table: lease,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        app_dirs::AppRootGuard,
        operation_journal::{ParticipantId, store::OperationJournalStore},
    };
    use std::sync::atomic::AtomicBool;
    #[test]
    fn legacy_recovery_read_owner_retains_existing_lock_rejects_missing_intent_and_never_resets_evidence()
     {
        // Real readonly profile/root/sidecar custody and SQL/data receipt fixture only.
        // No capacity admission, committed intent, current lease or audit grant is fabricated.
        let _runtime = crate::test_runtime::TestRuntimeGuard::acquire();
        let profile_root = tempfile::tempdir().unwrap();
        let media = tempfile::tempdir().unwrap();
        let _root = AppRootGuard::set(profile_root.path().to_path_buf()).unwrap();
        let profile = WritableProfileGuard::acquire_current().unwrap();
        std::fs::create_dir(profile_root.path().join("legacy")).unwrap();
        let path = profile_root
            .path()
            .join("legacy")
            .join(super::super::super::DB_FILE_NAME);
        let sql = include_str!("../../../../tests/fixtures/profile-v11/source.sql");
        rusqlite::Connection::open(&path)
            .unwrap()
            .execute_batch(sql)
            .unwrap();
        let journal = OperationJournalStore::open(&profile).unwrap();
        let original =
            SourceDatabase::open_legacy_read_owner(media.path(), &profile, "legacy").unwrap();
        let args = original
            .prepare_legacy_adoption_input(
                &profile,
                &journal,
                ParticipantId::new(),
                super::super::super::SourceCommandId::new(),
                &AtomicBool::new(false),
            )
            .unwrap();
        drop(original);
        let data = rusqlite::Connection::open(&path).unwrap();
        data.execute_batch("PRAGMA max_page_count=16384; PRAGMA synchronous=EXTRA; PRAGMA fullfsync=ON; PRAGMA temp_store=MEMORY; PRAGMA cache_spill=OFF; PRAGMA query_only=ON").unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        let before_schema = image::capture(&data).unwrap();
        let before_rows = rows::capture(&data, deadline).unwrap();
        let expected = super::super::super::schema::expected_legacy_postimage(
            &super::super::super::schema::bounded_schema_image(&data).unwrap(),
        )
        .unwrap();
        data.execute_batch("PRAGMA query_only=OFF; BEGIN EXCLUSIVE")
            .unwrap();
        let operation = OperationId::new();
        super::super::migration::apply_atomic_migration_body(
            &data,
            &args,
            operation,
            before_schema,
            &before_rows,
            &expected,
            deadline,
            || Ok(()),
        )
        .unwrap();
        data.execute_batch("COMMIT").unwrap();
        drop(data);
        let before = std::fs::read(&path).unwrap();
        let lock_path = profile_root
            .path()
            .join("legacy")
            .join(super::super::super::source_lease::SOURCE_LEASE_LOCK_FILE_NAME);
        let lock_identity = crate::filesystem_identity::stable_filesystem_identity_from_open_file(
            &std::fs::File::open(&lock_path).unwrap(),
        )
        .unwrap();
        let owner =
            SourceDatabase::open_legacy_recovery_read_owner(media.path(), &profile, "legacy")
                .unwrap();
        assert!(owner.connection.validate_original_legacy_read_fence());
        assert!(
            owner.capture_recovery(&profile, &journal).is_err(),
            "a source receipt cannot replace its actual journal intent"
        );
        assert!(owner.connection.validate_original_legacy_read_fence());
        assert!(
            SourceDatabase::open_legacy_recovery_read_owner(media.path(), &profile, "legacy")
                .is_err()
        );
        assert_eq!(std::fs::read(&path).unwrap(), before);
        assert_eq!(
            crate::filesystem_identity::stable_filesystem_identity_from_open_file(
                &std::fs::File::open(&lock_path).unwrap()
            )
            .unwrap(),
            lock_identity
        );
        drop(owner);
        // Canonical paired journal/receipt data is a read-only fixture, never an
        // admitted migration, provider ledger, initial claim or live Source grant.
        let record = crate::operation_journal::StoredOperationRecord::new_v19_legacy_adoption(
            operation,
            args.clone(),
            1,
        )
        .unwrap();
        let wire = crate::operation_journal::codec::encode(&record).unwrap();
        let journal_path = profile_root
            .path()
            .join(crate::operation_journal::store::JOURNAL_DATABASE_ROOT_NAME)
            .join(crate::operation_journal::store::JOURNAL_DATABASE_FILE_NAME);
        drop(journal); // Release its exclusive journal connection before fixture setup.
        rusqlite::Connection::open(&journal_path)
            .unwrap()
            .execute(
                "INSERT INTO operation_journal(operation_id,sequence,record) VALUES(?1,1,?2)",
                rusqlite::params![operation.as_uuid().to_string(), wire],
            )
            .unwrap();
        let owner =
            SourceDatabase::open_legacy_recovery_read_owner(media.path(), &profile, "legacy")
                .unwrap();
        let journal = OperationJournalStore::open(&profile).unwrap();
        let observed = owner.capture_recovery(&profile, &journal).unwrap();
        assert_eq!(observed.operation_id(), operation);
        assert!(!observed.lease_table_present());
        let journal_before = std::fs::read(&journal_path).unwrap();
        let mut worker = crate::operation_journal::owner::OperationJournalOwner::start_without_capacity_for_test(journal).unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while worker.status()
            == crate::operation_journal::owner::OperationJournalOwnerStatus::Initializing
        {
            assert!(Instant::now() < deadline);
            std::thread::yield_now();
        }
        let recovered = worker
            .client()
            .recover_legacy_read_custody(owner)
            .unwrap()
            .recv_timeout(Duration::from_secs(5))
            .unwrap()
            .unwrap();
        assert_eq!(recovered.operation_id(), operation);
        recovered
            .revalidate()
            .unwrap()
            .recv_timeout(Duration::from_secs(5))
            .unwrap()
            .unwrap();
        assert!(
            recovered
                .validate_claims()
                .unwrap()
                .recv_timeout(Duration::from_secs(5))
                .unwrap()
                .is_err(),
            "paired data must not establish retained provider capacity"
        );
        // A failed capacity observation does not consume the retained readonly reader.
        recovered
            .revalidate()
            .unwrap()
            .recv_timeout(Duration::from_secs(5))
            .unwrap()
            .unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), before);
        assert_eq!(std::fs::read(&journal_path).unwrap(), journal_before);
        assert!(
            SourceDatabase::open_legacy_recovery_read_owner(media.path(), &profile, "legacy")
                .is_err()
        );
        assert!(worker.shutdown());
        assert_eq!(std::fs::read(&path).unwrap(), before);
        assert_eq!(std::fs::read(&journal_path).unwrap(), journal_before);
        assert!(
            SourceDatabase::open_legacy_recovery_read_owner(
                tempfile::tempdir().unwrap().path(),
                &profile,
                "legacy"
            )
            .is_err()
        );
        assert_eq!(std::fs::read(&path).unwrap(), before);
        // Disposable fixture corruption: recovery must not recreate the deleted sidecar.
        std::fs::remove_file(&lock_path).unwrap();
        assert!(
            SourceDatabase::open_legacy_recovery_read_owner(media.path(), &profile, "legacy")
                .is_err()
        );
        assert!(!lock_path.exists());
        assert_eq!(std::fs::read(&path).unwrap(), before);
    }
}
