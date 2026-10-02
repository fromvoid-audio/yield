use super::*;

#[test]
fn held_membership_file_and_directory_substitution_reject_before_mutation() {
    for change in 0..4 {
        let f = audited();
        let before = f.owner.catalog_database().get_revision().unwrap();
        let prepared =
            prepare_subtree_scan(&f.owner, &Fixture::paths(&["known"]), &[], None).unwrap();
        match change {
            0 => f.put("known/unobserved.txt", b"late addition"),
            1 => {
                fs::remove_file(f.source.path().join("known/old.wav")).unwrap();
            }
            2 => {
                fs::rename(
                    f.source.path().join("known/nested"),
                    f.source.path().join("retired-dir"),
                )
                .unwrap();
                fs::create_dir(f.source.path().join("known/nested")).unwrap();
            }
            _ => f.put("known/old.wav", b"changed held content"),
        }
        let writer = CountingWriter::new(&f.owner);
        assert!(prepared.commit(&writer, None).is_err());
        assert_eq!(writer.commits.get(), 0);
        assert_eq!(f.owner.catalog_database().get_revision().unwrap(), before);
    }
}

#[test]
fn cancellation_revision_change_owner_rejection_and_elapsed_deadline_never_partial_commit() {
    for failure in 0..4 {
        let f = audited();
        f.put("known/old.wav", b"candidate change");
        let mut prepared =
            prepare_subtree_scan(&f.owner, &Fixture::paths(&["known"]), &[], None).unwrap();
        let cancel = AtomicBool::new(false);
        let mut writer = CountingWriter::new(&f.owner);
        match failure {
            0 => writer.cancel_at_lock = Some(&cancel),
            1 => {
                let mut b = f.owner.catalog_database().write_batch().unwrap();
                b.set_metadata("unrelated", "new revision").unwrap();
                b.commit().unwrap();
            }
            2 => writer.reject = true,
            _ => {
                prepared.started =
                    Instant::now() - SUBTREE_SCAN_MAX_ELAPSED - Duration::from_secs(1)
            }
        }
        let before = f.owner.catalog_database().get_revision().unwrap();
        assert!(prepared.commit(&writer, Some(&cancel)).is_err());
        assert_eq!(f.owner.catalog_database().get_revision().unwrap(), before);
        assert_ne!(
            f.owner
                .catalog_database()
                .entry_for_path(Path::new("known/old.wav"))
                .unwrap()
                .unwrap()
                .content_hash,
            Some(blake3::hash(b"candidate change").to_hex().to_string())
        );
    }
}

#[test]
fn oversized_or_incomplete_traversal_retains_last_good_source_facts() {
    for files in [65, 129] {
        let f = audited();
        for i in 0..files {
            f.put(&format!("known/extra-{i}.txt"), b"bounded");
        }
        let before = f.owner.catalog_database().get_revision().unwrap();
        assert!(prepare_subtree_scan(&f.owner, &Fixture::paths(&["known"]), &[], None).is_err());
        assert_eq!(f.owner.catalog_database().get_revision().unwrap(), before);
    }
    let f = audited();
    fs::remove_dir_all(f.source.path().join("known")).unwrap();
    let before = f.owner.catalog_database().get_revision().unwrap();
    assert!(prepare_subtree_scan(&f.owner, &Fixture::paths(&["known"]), &[], None).is_err());
    assert_eq!(f.owner.catalog_database().get_revision().unwrap(), before);
}

#[cfg(unix)]
#[test]
fn links_and_fifos_are_uncertain_without_following_or_blocking() {
    use std::os::unix::{ffi::OsStrExt, fs::symlink};
    let f = audited();
    symlink(
        f.source.path().join("other"),
        f.source.path().join("known/link"),
    )
    .unwrap();
    assert!(prepare_subtree_scan(&f.owner, &Fixture::paths(&["known"]), &[], None).is_err());
    fs::remove_file(f.source.path().join("known/link")).unwrap();
    let name = f.source.path().join("known/fifo");
    let c = std::ffi::CString::new(name.as_os_str().as_bytes()).unwrap();
    assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0);
    assert!(prepare_subtree_scan(&f.owner, &Fixture::paths(&["known"]), &[], None).is_err());
}

#[test]
fn held_source_root_replacement_and_auxiliary_index_revision_prevent_commit() {
    for replace_root in [true, false] {
        let f = audited();
        let before = f.owner.catalog_database().get_revision().unwrap();
        let prepared =
            prepare_subtree_scan(&f.owner, &Fixture::paths(&["known"]), &[], None).unwrap();
        let moved = f.source.path().with_extension("held-subtree-retired-root");
        if replace_root {
            fs::rename(f.source.path(), &moved).unwrap();
            fs::create_dir(f.source.path()).unwrap();
        } else {
            let db = f.owner.catalog_database();
            let mut index = db.source_index_page(None, 64).unwrap().entries[0].clone();
            index.file_size = Some(index.file_size.unwrap_or_default() + 1);
            let mut batch = db.write_batch().unwrap();
            batch.upsert_source_index_entry(&index).unwrap();
            batch.commit_auxiliary_state().unwrap();
        }
        let writer = CountingWriter::new(&f.owner);
        assert!(
            prepared.commit(&writer, None).is_err(),
            "replace_root={replace_root}"
        );
        assert_eq!(writer.commits.get(), 0);
        assert_eq!(f.owner.catalog_database().get_revision().unwrap(), before);
        if replace_root {
            fs::remove_dir_all(f.source.path()).unwrap();
            fs::rename(moved, f.source.path()).unwrap();
        }
    }
}
