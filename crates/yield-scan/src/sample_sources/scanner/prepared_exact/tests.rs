//! Actual retained owner/SQLite/filesystem regressions; no native replay qualification.
use super::*;
use std::{cell::Cell, path::PathBuf};
use tempfile::{TempDir, tempdir};
use yield_library::sample_sources::db::META_SOURCE_INDEX_REVISION;
use yield_library::sample_sources::db::SourceWriteBatch;
use yield_library::{
    app_dirs::{ConfigBaseGuard, PersistenceProfileGuard, WritableProfileGuard},
    sample_sources::{SourceDatabase, SourceId},
    test_runtime::TestRuntimeGuard,
};

pub(in crate::sample_sources::scanner) struct Fixture {
    pub(in crate::sample_sources::scanner) owner: SourceWriter,
    pub(in crate::sample_sources::scanner) source: TempDir,
    _guard: WritableProfileGuard,
    _profile: PersistenceProfileGuard,
    _config: ConfigBaseGuard,
    _base: TempDir,
    _runtime: TestRuntimeGuard,
}
impl Fixture {
    pub(in crate::sample_sources::scanner) fn new() -> Self {
        let runtime = TestRuntimeGuard::acquire();
        let base =
            tempfile::tempdir_in(std::fs::canonicalize(std::env::temp_dir()).unwrap()).unwrap();
        let config = ConfigBaseGuard::set(base.path().to_path_buf());
        let profile = PersistenceProfileGuard::named("atomic-exact-scan");
        let guard = WritableProfileGuard::acquire_current().unwrap();
        let source = tempdir().unwrap();
        let db = SourceDatabase::open_for_catalog_with_profile_guard(
            source.path(),
            &guard,
            Path::new("scan-db"),
        )
        .unwrap();
        let owner = SourceWriter::new_for_scan(db, SourceId::from_string("atomic-exact-source"), 7)
            .unwrap();
        Self {
            owner,
            source,
            _guard: guard,
            _profile: profile,
            _config: config,
            _base: base,
            _runtime: runtime,
        }
    }
    pub(in crate::sample_sources::scanner) fn put(&self, path: &str, bytes: &[u8]) {
        std::fs::write(self.source.path().join(path), bytes).unwrap();
    }
    pub(in crate::sample_sources::scanner) fn paths(paths: &[&str]) -> Vec<RootRelativePath> {
        paths
            .iter()
            .map(|p| RootRelativePath::try_from_path(PathBuf::from(p)).unwrap())
            .collect()
    }
}

pub(in crate::sample_sources::scanner) struct CountingWriter<'a> {
    pub(in crate::sample_sources::scanner) owner: &'a SourceWriter,
    pub(in crate::sample_sources::scanner) locks: Cell<usize>,
    pub(in crate::sample_sources::scanner) commits: Cell<usize>,
    pub(in crate::sample_sources::scanner) reject: bool,
    pub(in crate::sample_sources::scanner) cancel_at_lock: Option<&'a AtomicBool>,
}
impl<'a> CountingWriter<'a> {
    pub(in crate::sample_sources::scanner) fn new(owner: &'a SourceWriter) -> Self {
        Self {
            owner,
            locks: Cell::new(0),
            commits: Cell::new(0),
            reject: false,
            cancel_at_lock: None,
        }
    }
}
impl ScanWriter for CountingWriter<'_> {
    type Guard = ();
    fn lock(&self, _: ScanWritePhase) {
        self.locks.set(self.locks.get() + 1);
        if let Some(c) = self.cancel_at_lock {
            c.store(true, Ordering::Release);
        }
    }
    fn commit_bounded_manifest_batch(
        &self,
        batch: SourceWriteBatch<'_>,
        expected: u64,
    ) -> Result<ManifestCommitResult, ScanError> {
        self.commits.set(self.commits.get() + 1);
        if self.reject {
            return Err(ScanError::SourceOwner(
                "injected owner rejection before commit".into(),
            ));
        }
        self.owner.commit_bounded_manifest_batch(batch, expected)
    }
}

#[test]
fn prepared_exact_audio_and_index_commit_one_owned_revision() {
    let f = Fixture::new();
    f.put("音.wav", b"first");
    f.put("notes.txt", b"notes");
    let writer = CountingWriter::new(&f.owner);
    let before = f.owner.current_scan_fence().unwrap().revision();
    let paths = Fixture::paths(&["音.wav", "notes.txt"]);
    let prepared = prepare_exact_scan(&f.owner, &paths, None).unwrap();
    assert_eq!(prepared.expected_revision(), before);
    assert_eq!(prepared.entry_count(), 2);
    assert_eq!(writer.locks.get(), 0);
    assert!(
        f.owner
            .catalog_database()
            .entry_for_path(Path::new("音.wav"))
            .unwrap()
            .is_none()
    );
    let result = prepared.commit(&writer, None).unwrap();
    assert_eq!(result.revision, before + 1);
    assert_eq!((writer.locks.get(), writer.commits.get()), (1, 1));
    assert!(result.authoritative_snapshot.is_none());
    assert_eq!(result.touched_path_changes.len(), 1);
    assert_eq!(
        result
            .source_index_commit
            .as_ref()
            .unwrap()
            .upserted_entries[0]
            .relative_path,
        Path::new("notes.txt")
    );
    let audio = f
        .owner
        .catalog_database()
        .entry_for_path(Path::new("音.wav"))
        .unwrap()
        .unwrap();
    assert_eq!(
        audio.content_hash.unwrap(),
        blake3::hash(b"first").to_hex().to_string()
    );
    assert!(
        f.owner
            .catalog_database()
            .get_metadata("last_scan_completed_at")
            .unwrap()
            .is_none()
    );
}

#[test]
fn prepared_exact_owner_rejection_rolls_back_audio_and_index_together() {
    let f = Fixture::new();
    f.put("one.wav", b"audio");
    f.put("notes.txt", b"notes");
    let before = f.owner.current_scan_fence().unwrap().revision();
    let mut writer = CountingWriter::new(&f.owner);
    writer.reject = true;
    let paths = Fixture::paths(&["one.wav", "notes.txt"]);
    assert!(
        prepare_exact_scan(&f.owner, &paths, None)
            .unwrap()
            .commit(&writer, None)
            .is_err()
    );
    assert_eq!(writer.commits.get(), 1);
    assert_eq!(f.owner.current_scan_fence().unwrap().revision(), before);
    assert!(f.owner.catalog_database().list_files().unwrap().is_empty());
    assert!(
        f.owner
            .catalog_database()
            .source_index_page(None, 1)
            .unwrap()
            .entries
            .is_empty()
    );
}

#[test]
fn prepared_exact_foreign_writer_and_changed_index_revision_roll_back() {
    let f = Fixture::new();
    f.put("one.wav", b"audio");
    f.put("notes.txt", b"notes");
    let paths = Fixture::paths(&["one.wav", "notes.txt"]);
    let other_source = tempdir().unwrap();
    let other_db = SourceDatabase::open_for_catalog_with_profile_guard(
        other_source.path(),
        &f._guard,
        Path::new("other-db"),
    )
    .unwrap();
    let other_owner =
        SourceWriter::new_for_scan(other_db, SourceId::from_string("other-source"), 7).unwrap();
    let prepared = prepare_exact_scan(&f.owner, &paths, None).unwrap();
    assert!(prepared.commit(&other_owner, None).is_err());
    assert!(f.owner.catalog_database().list_files().unwrap().is_empty());
    assert!(
        other_owner
            .catalog_database()
            .list_files()
            .unwrap()
            .is_empty()
    );
    let prepared = prepare_exact_scan(&f.owner, &paths, None).unwrap();
    let before = f.owner.current_scan_fence().unwrap().revision();
    let mut batch = f.owner.catalog_database().write_batch().unwrap();
    batch
        .set_metadata(META_SOURCE_INDEX_REVISION, "999")
        .unwrap();
    batch.commit_auxiliary_state().unwrap();
    assert!(prepared.commit(&f.owner, None).is_err());
    assert_eq!(f.owner.current_scan_fence().unwrap().revision(), before);
    assert!(f.owner.catalog_database().list_files().unwrap().is_empty());
}

#[test]
fn prepared_exact_stale_revision_and_cancellation_never_commit() {
    let f = Fixture::new();
    f.put("one.wav", b"audio");
    let paths = Fixture::paths(&["one.wav"]);
    let prepared = prepare_exact_scan(&f.owner, &paths, None).unwrap();
    f.owner
        .catalog_database()
        .set_metadata("unrelated", "changed")
        .unwrap();
    let writer = CountingWriter::new(&f.owner);
    assert!(matches!(
        prepared.commit(&writer, None),
        Err(ScanError::StaleRevision { .. })
    ));
    assert_eq!(writer.commits.get(), 0);
    let cancel = AtomicBool::new(false);
    let prepared = prepare_exact_scan(&f.owner, &paths, Some(&cancel)).unwrap();
    let mut writer = CountingWriter::new(&f.owner);
    writer.cancel_at_lock = Some(&cancel);
    assert!(matches!(
        prepared.commit(&writer, Some(&cancel)),
        Err(ScanError::Canceled)
    ));
    assert_eq!(writer.commits.get(), 0);
    assert!(f.owner.catalog_database().list_files().unwrap().is_empty());
    assert!(matches!(
        prepare_exact_scan(&f.owner, &paths, Some(&cancel)),
        Err(ScanError::Canceled)
    ));
}

#[test]
fn prepared_exact_substitution_and_root_replacement_keep_last_good_rows() {
    let f = Fixture::new();
    f.put("one.wav", b"baseline");
    f.put("notes.txt", b"baseline");
    let paths = Fixture::paths(&["one.wav", "notes.txt"]);
    prepare_exact_scan(&f.owner, &paths, None)
        .unwrap()
        .commit(&f.owner, None)
        .unwrap();
    let before = f.owner.current_scan_fence().unwrap().revision();
    f.put("one.wav", b"candidate");
    let prepared = prepare_exact_scan(&f.owner, &paths, None).unwrap();
    std::fs::rename(
        f.source.path().join("notes.txt"),
        f.source.path().join("old.txt"),
    )
    .unwrap();
    f.put("notes.txt", b"baseline");
    assert!(prepared.commit(&f.owner, None).is_err());
    assert_eq!(f.owner.current_scan_fence().unwrap().revision(), before);
    assert_eq!(
        f.owner
            .catalog_database()
            .entry_for_path(Path::new("one.wav"))
            .unwrap()
            .unwrap()
            .content_hash
            .unwrap(),
        blake3::hash(b"baseline").to_hex().to_string()
    );
    let prepared = prepare_exact_scan(&f.owner, &paths, None).unwrap();
    let moved = f.source.path().with_extension("moved");
    std::fs::rename(f.source.path(), &moved).unwrap();
    std::fs::create_dir(f.source.path()).unwrap();
    assert!(prepared.commit(&f.owner, None).is_err());
    std::fs::remove_dir_all(moved).unwrap();
}

#[test]
fn prepared_exact_missing_directory_duplicate_and_input_limits_do_not_mutate() {
    let f = Fixture::new();
    f.put("one.wav", b"audio");
    std::fs::create_dir(f.source.path().join("folder")).unwrap();
    for paths in [
        Fixture::paths(&[]),
        Fixture::paths(&["one.wav", "missing.wav"]),
        Fixture::paths(&["folder"]),
        Fixture::paths(&["one.wav", "one.wav"]),
        Fixture::paths(&["one.wav"; EXACT_SCAN_MAX_ENTRIES + 1]),
        Fixture::paths(&[&"x".repeat(1025)]),
    ] {
        assert!(prepare_exact_scan(&f.owner, &paths, None).is_err());
    }
    let large = File::create(f.source.path().join("large.wav")).unwrap();
    large.set_len(EXACT_SCAN_MAX_HASH_BYTES + 1).unwrap();
    assert!(prepare_exact_scan(&f.owner, &Fixture::paths(&["large.wav"]), None).is_err());
    assert!(f.owner.catalog_database().list_files().unwrap().is_empty());
}

#[cfg(unix)]
#[test]
fn prepared_exact_raw_paths_are_preserved_and_links_are_rejected() {
    use std::os::unix::{ffi::OsStringExt, fs::symlink};
    let f = Fixture::new();
    let invalid = PathBuf::from(std::ffi::OsString::from_vec(b"raw-\xff.txt".to_vec()));
    // APFS rejects invalid UTF-8 before scanner admission. Qualify that native
    // boundary and the lossless path value separately, then use a filename the
    // filesystem can actually hold for the original commit/link assertions.
    #[cfg(target_os = "macos")]
    let raw = {
        assert_eq!(
            RootRelativePath::try_from_path(invalid.clone())
                .unwrap()
                .as_path(),
            invalid.as_path()
        );
        let error = std::fs::write(f.source.path().join(&invalid), b"raw").unwrap_err();
        assert_eq!(error.raw_os_error(), Some(92), "APFS EILSEQ: {error}");
        assert!(!f.source.path().join(&invalid).exists());
        assert!(f.owner.catalog_database().list_files().unwrap().is_empty());
        PathBuf::from("raw-é.txt")
    };
    #[cfg(not(target_os = "macos"))]
    let raw = invalid;
    std::fs::write(f.source.path().join(&raw), b"raw").unwrap();
    let paths = vec![RootRelativePath::try_from_path(raw.clone()).unwrap()];
    let result = prepare_exact_scan(&f.owner, &paths, None)
        .unwrap()
        .commit(&f.owner, None)
        .unwrap();
    assert_eq!(
        result.source_index_commit.unwrap().upserted_entries[0].relative_path,
        raw
    );
    symlink(&raw, f.source.path().join("link.txt")).unwrap();
    let before = f.owner.current_scan_fence().unwrap().revision();
    assert!(prepare_exact_scan(&f.owner, &Fixture::paths(&["link.txt"]), None).is_err());
    assert_eq!(f.owner.current_scan_fence().unwrap().revision(), before);
}
