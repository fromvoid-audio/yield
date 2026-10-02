//! Actual owner/SQLite/held namespace checks, not native FSEvents qualification.
use super::super::prepared_exact::tests::{CountingWriter, Fixture};
use super::*;
use std::{fs, path::Path};
mod failures;

fn audited() -> Fixture {
    let f = Fixture::new();
    fs::create_dir_all(f.source.path().join("known/nested")).unwrap();
    fs::create_dir(f.source.path().join("other")).unwrap();
    f.put("known/old.wav", b"old audio");
    f.put("known/old.txt", b"old unsupported");
    f.put("known/nested/child.wav", b"nested audio");
    f.put("other/untouched.wav", b"untouched");
    let writer = CountingWriter::new(&f.owner);
    super::super::audit_source_and_record_with_budget_and_progress_and_writer(
        f.owner.catalog_database(),
        None,
        super::super::ContentAuditBudget::entry_limited(256),
        1,
        &mut |_, _| {},
        &writer,
    )
    .unwrap();
    f
}

#[test]
fn complete_subtree_add_delete_index_and_directory_delta_one_revision_preserves_other_truth() {
    let f = audited();
    let db = f.owner.catalog_database();
    let before = db.get_revision().unwrap();
    let initial_audit = db.last_authoritative_audit_metadata().unwrap();
    let initial_page = db.source_directory_truth_page(None, 1).unwrap();
    let other = db
        .source_directory_truth_page(None, 64)
        .unwrap()
        .entries
        .into_iter()
        .find(|e| e.relative_path == Path::new("other"))
        .unwrap();
    fs::remove_file(f.source.path().join("known/old.wav")).unwrap();
    fs::remove_file(f.source.path().join("known/old.txt")).unwrap();
    fs::remove_dir_all(f.source.path().join("known/nested")).unwrap();
    fs::create_dir(f.source.path().join("known/new")).unwrap();
    f.put("known/new/added.wav", b"added");
    f.put("known/added.txt", b"new index");
    let writer = CountingWriter::new(&f.owner);
    let prepared = prepare_subtree_scan(&f.owner, &Fixture::paths(&["known"]), &[], None).unwrap();
    assert_eq!(writer.locks.get(), 0);
    let result = prepared.commit(&writer, None).unwrap();
    assert_eq!(writer.commits.get(), 1);
    assert_eq!(result.revision, before + 1);
    assert!(result.authoritative_snapshot.is_none());
    assert!(
        db.entry_for_path(Path::new("known/old.wav"))
            .unwrap()
            .unwrap()
            .missing
    );
    assert!(
        db.entry_for_path(Path::new("known/nested/child.wav"))
            .unwrap()
            .unwrap()
            .missing
    );
    assert!(
        db.entry_for_path(Path::new("known/new/added.wav"))
            .unwrap()
            .is_some_and(|e| !e.missing)
    );
    let index = result.source_index_commit.unwrap();
    assert!(
        index
            .removed_paths
            .contains(&PathBuf::from("known/old.txt"))
    );
    assert!(
        index
            .upserted_entries
            .iter()
            .any(|e| e.relative_path == Path::new("known/added.txt"))
    );
    assert_eq!(index.source_revision, before + 1);
    let entries = db.source_directory_truth_page(None, 64).unwrap().entries;
    assert!(entries.contains(&other));
    assert!(
        entries
            .iter()
            .any(|e| e.relative_path == Path::new("known/new"))
    );
    assert!(
        !entries
            .iter()
            .any(|e| e.relative_path == Path::new("known/nested"))
    );
    assert!(
        db.source_directory_truth_page(initial_page.next_cursor.as_ref(), 1)
            .is_err()
    );
    assert_eq!(
        db.last_authoritative_audit_metadata().unwrap(),
        initial_audit
    );
}

#[test]
fn empty_new_and_disjoint_exact_scopes_commit_without_full_manifest_hydration() {
    let f = audited();
    fs::create_dir(f.source.path().join("new-empty")).unwrap();
    f.put("other/untouched.wav", b"changed disjoint exact");
    let writer = CountingWriter::new(&f.owner);
    let result = prepare_subtree_scan(
        &f.owner,
        &Fixture::paths(&["new-empty"]),
        &Fixture::paths(&["other/untouched.wav"]),
        None,
    )
    .unwrap()
    .commit(&writer, None)
    .unwrap();
    assert_eq!(result.touched_path_changes.len(), 1);
    assert!(result.authoritative_snapshot.is_none());
    let dirs = f
        .owner
        .catalog_database()
        .source_directory_truth_page(None, 64)
        .unwrap()
        .entries;
    assert!(
        dirs.iter()
            .any(|e| e.relative_path == Path::new("new-empty"))
    );
}

#[test]
fn former_audio_and_index_entries_replaced_by_directories_are_retired_in_same_delta() {
    let f = audited();
    for path in ["known/old.wav", "known/old.txt"] {
        fs::remove_file(f.source.path().join(path)).unwrap();
        fs::create_dir(f.source.path().join(path)).unwrap();
    }
    let writer = CountingWriter::new(&f.owner);
    let result = prepare_subtree_scan(&f.owner, &Fixture::paths(&["known"]), &[], None)
        .unwrap()
        .commit(&writer, None)
        .unwrap();
    assert!(
        f.owner
            .catalog_database()
            .entry_for_path(Path::new("known/old.wav"))
            .unwrap()
            .unwrap()
            .missing
    );
    assert!(
        result
            .source_index_commit
            .unwrap()
            .removed_paths
            .contains(&PathBuf::from("known/old.txt"))
    );
    let dirs = f
        .owner
        .catalog_database()
        .source_directory_truth_page(None, 64)
        .unwrap()
        .entries;
    assert!(
        dirs.iter()
            .any(|e| e.relative_path == Path::new("known/old.wav"))
    );
    assert!(
        dirs.iter()
            .any(|e| e.relative_path == Path::new("known/old.txt"))
    );
}
