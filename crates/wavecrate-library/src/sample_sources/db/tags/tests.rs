use std::path::{Path, PathBuf};

use tempfile::tempdir;

use super::super::{
    SOURCE_TAG_PAGE_MAX_PATH_BYTES, SOURCE_TAG_PAGE_MAX_PATHS, SOURCE_TAG_PAGE_MAX_PREFIX_BYTES,
    SourceDatabase, SourceDbError,
};

#[test]
fn normalization_prevents_case_and_spacing_duplicates() {
    let dir = tempdir().unwrap();
    let db = SourceDatabase::open_for_user_metadata_write(dir.path()).unwrap();
    db.upsert_file(Path::new("one.wav"), 10, 5).unwrap();
    db.upsert_file(Path::new("two.wav"), 10, 5).unwrap();

    let first = db
        .assign_tag_to_path(Path::new("one.wav"), "  Deep   Kick ")
        .unwrap();
    let second = db
        .assign_tag_to_path(Path::new("two.wav"), "deep kick")
        .unwrap();

    assert_eq!(first.id, second.id);
    assert_eq!(first.display_label, "Deep Kick");
    assert_eq!(first.normalized_text, "deep kick");
    assert_eq!(db.search_tags("DEEP    KICK", 8).unwrap().len(), 1);
}

#[test]
fn most_used_tags_order_by_persisted_usage_then_label() {
    let dir = tempdir().unwrap();
    let db = SourceDatabase::open_for_user_metadata_write(dir.path()).unwrap();
    for path in ["one.wav", "two.wav", "three.wav", "four.wav"] {
        db.upsert_file(Path::new(path), 10, 5).unwrap();
    }
    db.assign_tag_to_path(Path::new("one.wav"), "zeta").unwrap();
    db.assign_tag_to_path(Path::new("one.wav"), "alpha")
        .unwrap();
    db.assign_tag_to_path(Path::new("two.wav"), "alpha")
        .unwrap();
    db.assign_tag_to_path(Path::new("three.wav"), "beta")
        .unwrap();
    db.assign_tag_to_path(Path::new("four.wav"), "beta")
        .unwrap();

    let labels = db
        .most_used_tags(8)
        .unwrap()
        .into_iter()
        .map(|usage| (usage.tag.display_label, usage.assignment_count))
        .collect::<Vec<_>>();

    assert_eq!(
        labels,
        vec![
            ("alpha".to_string(), 2),
            ("beta".to_string(), 2),
            ("zeta".to_string(), 1),
        ]
    );
}

#[test]
fn assignment_api_creates_then_resolves_existing_tag() {
    let dir = tempdir().unwrap();
    let db = SourceDatabase::open_for_user_metadata_write(dir.path()).unwrap();
    db.upsert_file(Path::new("one.wav"), 10, 5).unwrap();
    db.upsert_file(Path::new("two.wav"), 10, 5).unwrap();

    let created = db
        .assign_tag_to_path(Path::new("one.wav"), "Texture")
        .unwrap();
    let resolved = db
        .assign_tag_to_path(Path::new("two.wav"), " texture ")
        .unwrap();

    assert_eq!(created.id, resolved.id);
    assert_eq!(
        db.tags_for_path(Path::new("two.wav")).unwrap(),
        vec![created]
    );
    let removed = db
        .remove_tag_from_path(Path::new("two.wav"), "TEXTURE")
        .unwrap();
    assert!(removed);
    assert!(db.tags_for_path(Path::new("two.wav")).unwrap().is_empty());
}

#[test]
fn search_tags_orders_exact_match_before_usage_matches() {
    let dir = tempdir().unwrap();
    let db = SourceDatabase::open_for_user_metadata_write(dir.path()).unwrap();
    for path in ["one.wav", "two.wav", "three.wav"] {
        db.upsert_file(Path::new(path), 10, 5).unwrap();
    }
    db.assign_tag_to_path(Path::new("one.wav"), "deep kick")
        .unwrap();
    db.assign_tag_to_path(Path::new("two.wav"), "kick").unwrap();
    db.assign_tag_to_path(Path::new("three.wav"), "deep kick")
        .unwrap();

    let labels = db
        .search_tags("kick", 8)
        .unwrap()
        .into_iter()
        .map(|usage| usage.tag.display_label)
        .collect::<Vec<_>>();

    assert_eq!(labels, vec!["kick".to_string(), "deep kick".to_string()]);
}

#[test]
fn search_tags_finds_less_common_fuzzy_matches() {
    let dir = tempdir().unwrap();
    let db = SourceDatabase::open_for_user_metadata_write(dir.path()).unwrap();
    for path in ["one.wav", "two.wav", "three.wav"] {
        db.upsert_file(Path::new(path), 10, 5).unwrap();
    }
    db.assign_tag_to_path(Path::new("one.wav"), "Texture")
        .unwrap();
    db.assign_tag_to_path(Path::new("two.wav"), "Texture")
        .unwrap();
    db.assign_tag_to_path(Path::new("three.wav"), "Rare FX")
        .unwrap();

    let labels = db
        .search_tags("rfx", 8)
        .unwrap()
        .into_iter()
        .map(|usage| usage.tag.display_label)
        .collect::<Vec<_>>();

    assert_eq!(labels, vec!["Rare FX".to_string()]);
}

#[test]
fn tag_vocabulary_pages_are_keyset_paged_in_canonical_order() {
    let dir = tempdir().unwrap();
    let db = SourceDatabase::open_for_user_metadata_write(dir.path()).unwrap();
    db.upsert_file(Path::new("one.wav"), 10, 5).unwrap();
    for label in ["Bravo", "alpha", "charlie"] {
        db.assign_tag_to_path(Path::new("one.wav"), label).unwrap();
    }
    let revision = db.get_revision().unwrap();

    let first = db
        .tag_vocabulary_page_at_revision(revision, "", None, 2)
        .unwrap();
    assert_eq!(first.revision, revision);
    assert_eq!(
        first
            .entries
            .iter()
            .map(|entry| entry.normalized_text.as_str())
            .collect::<Vec<_>>(),
        vec!["alpha", "bravo"]
    );
    assert!(
        first
            .entries
            .iter()
            .all(|entry| entry.assignment_count.is_none())
    );
    let second = db
        .tag_vocabulary_page_at_revision(revision, "", first.next_cursor.as_ref(), 2)
        .unwrap();
    assert_eq!(
        second
            .entries
            .iter()
            .map(|entry| entry.normalized_text.as_str())
            .collect::<Vec<_>>(),
        vec!["charlie"]
    );
    assert!(second.next_cursor.is_none());
}

#[test]
fn tag_vocabulary_prefixes_treat_percent_and_underscore_literally() {
    let dir = tempdir().unwrap();
    let db = SourceDatabase::open_for_user_metadata_write(dir.path()).unwrap();
    db.upsert_file(Path::new("one.wav"), 10, 5).unwrap();
    for label in ["foo%literal", "foo_literal", "fooXliteral", "éclair"] {
        db.assign_tag_to_path(Path::new("one.wav"), label).unwrap();
    }
    let revision = db.get_revision().unwrap();

    let percent = db
        .tag_vocabulary_page_at_revision(revision, "foo%", None, 64)
        .unwrap();
    assert_eq!(
        percent
            .entries
            .iter()
            .map(|entry| entry.normalized_text.as_str())
            .collect::<Vec<_>>(),
        vec!["foo%literal"]
    );
    let underscore = db
        .tag_vocabulary_page_at_revision(revision, "foo_", None, 64)
        .unwrap();
    assert_eq!(
        underscore
            .entries
            .iter()
            .map(|entry| entry.normalized_text.as_str())
            .collect::<Vec<_>>(),
        vec!["foo_literal"]
    );
    let unicode = db
        .tag_vocabulary_page_at_revision(revision, "écl", None, 64)
        .unwrap();
    assert_eq!(
        unicode
            .entries
            .iter()
            .map(|entry| entry.normalized_text.as_str())
            .collect::<Vec<_>>(),
        vec!["éclair"]
    );
}

#[test]
fn selected_tag_page_unions_paths_and_counts_distinct_selected_paths() {
    let dir = tempdir().unwrap();
    let db = SourceDatabase::open_for_user_metadata_write(dir.path()).unwrap();
    for path in ["one.wav", "two.wav", "three.wav"] {
        db.upsert_file(Path::new(path), 10, 5).unwrap();
    }
    db.assign_tag_to_path(Path::new("one.wav"), "Alpha")
        .unwrap();
    db.assign_tag_to_path(Path::new("two.wav"), "alpha")
        .unwrap();
    db.assign_tag_to_path(Path::new("one.wav"), "Beta").unwrap();
    db.assign_tag_to_path(Path::new("three.wav"), "beta")
        .unwrap();
    let revision = db.get_revision().unwrap();

    let selected = db
        .tags_for_paths_page_at_revision(
            revision,
            &[
                PathBuf::from("one.wav"),
                PathBuf::from("two.wav"),
                PathBuf::from("one.wav"),
            ],
            None,
            64,
        )
        .unwrap();
    assert_eq!(
        selected
            .entries
            .iter()
            .map(|entry| (entry.normalized_text.as_str(), entry.assignment_count))
            .collect::<Vec<_>>(),
        vec![("alpha", Some(2)), ("beta", Some(1))]
    );

    let vocabulary = db
        .tag_vocabulary_page_at_revision(revision, "", None, 64)
        .unwrap();
    assert!(
        vocabulary
            .entries
            .iter()
            .all(|entry| entry.assignment_count.is_none())
    );
}

#[test]
fn selected_tag_page_handles_large_selection_mixed_counts_dedup_and_cursor() {
    let dir = tempdir().unwrap();
    let db = SourceDatabase::open_for_user_metadata_write(dir.path()).unwrap();
    let paths = (0..129)
        .map(|index| PathBuf::from(format!("sample-{index:03}.wav")))
        .collect::<Vec<_>>();
    for path in &paths {
        db.upsert_file(path, 10, 5).unwrap();
        db.assign_tag_to_path(path, "shared").unwrap();
    }
    for (index, path) in paths.iter().enumerate() {
        if index % 2 == 0 {
            db.assign_tag_to_path(path, "even-only").unwrap();
        }
    }
    db.assign_tag_to_path(&paths[128], "late").unwrap();
    let revision = db.get_revision().unwrap();

    let mut selected = paths.clone();
    selected.extend([paths[0].clone(), paths[128].clone()]);
    let first = db
        .tags_for_paths_page_at_revision(revision, &selected, None, 2)
        .unwrap();
    assert_eq!(
        first
            .entries
            .iter()
            .map(|entry| (entry.normalized_text.as_str(), entry.assignment_count))
            .collect::<Vec<_>>(),
        vec![("even-only", Some(65)), ("late", Some(1))]
    );
    let cursor = first.next_cursor.expect("large selection continuation");

    let mut continuation = selected;
    continuation.reverse();
    continuation.push(paths[0].clone());
    let second = db
        .tags_for_paths_page_at_revision(revision, &continuation, Some(&cursor), 2)
        .unwrap();
    assert!(second.next_cursor.is_none());
    assert_eq!(
        second
            .entries
            .iter()
            .map(|entry| (entry.normalized_text.as_str(), entry.assignment_count))
            .collect::<Vec<_>>(),
        vec![("shared", Some(129))]
    );
}

#[test]
fn canceled_tag_staging_rolls_back_temporary_selection_scope() {
    let dir = tempdir().unwrap();
    let db = SourceDatabase::open_for_user_metadata_write(dir.path()).unwrap();
    db.upsert_file(Path::new("one.wav"), 10, 5).unwrap();
    db.assign_tag_to_path(Path::new("one.wav"), "shared")
        .unwrap();
    let revision = db.get_revision().unwrap();
    let paths = (0..129)
        .map(|index| PathBuf::from(format!("sample-{index:03}.wav")))
        .collect::<Vec<_>>();
    let mut callback_checks = 0;
    assert!(matches!(
        db.tags_for_paths_page_at_revision_with_cancellation(revision, &paths, None, 1, || {
            callback_checks += 1;
            callback_checks >= 3
        },),
        Err(SourceDbError::Canceled)
    ));

    let page = db
        .tags_for_paths_page_at_revision(revision, &[PathBuf::from("one.wav")], None, 1)
        .unwrap();
    assert_eq!(page.entries.len(), 1);
    assert_eq!(page.entries[0].normalized_text, "shared");
    assert_eq!(page.entries[0].assignment_count, Some(1));
}

#[test]
fn tag_pages_reject_overlarge_inputs_before_querying() {
    let dir = tempdir().unwrap();
    let db = SourceDatabase::open_for_user_metadata_write(dir.path()).unwrap();
    let too_many_paths = (0..=SOURCE_TAG_PAGE_MAX_PATHS)
        .map(|index| PathBuf::from(format!("{index}.wav")))
        .collect::<Vec<_>>();
    assert!(matches!(
        db.tags_for_paths_page_at_revision(0, &too_many_paths, None, 1),
        Err(SourceDbError::TagPageTooManyPaths { .. })
    ));

    let too_many_bytes = vec![PathBuf::from(
        "x".repeat(SOURCE_TAG_PAGE_MAX_PATH_BYTES + 1),
    )];
    assert!(matches!(
        db.tags_for_paths_page_at_revision(0, &too_many_bytes, None, 1),
        Err(SourceDbError::TagPagePathBytesTooLarge { .. })
    ));
    let too_long_prefix = "p".repeat(SOURCE_TAG_PAGE_MAX_PREFIX_BYTES + 1);
    assert!(matches!(
        db.tag_vocabulary_page_at_revision(0, &too_long_prefix, None, 1),
        Err(SourceDbError::TagPagePrefixTooLong { .. })
    ));
    assert!(matches!(
        db.tag_vocabulary_page_at_revision(0, "tag\0prefix", None, 1),
        Err(SourceDbError::TagPagePrefixContainsNul)
    ));
}

#[test]
fn tag_page_cursors_are_revision_and_query_fenced() {
    let dir = tempdir().unwrap();
    let db = SourceDatabase::open_for_user_metadata_write(dir.path()).unwrap();
    db.upsert_file(Path::new("one.wav"), 10, 5).unwrap();
    for label in ["alpha", "atom", "beta"] {
        db.assign_tag_to_path(Path::new("one.wav"), label).unwrap();
    }
    let revision = db.get_revision().unwrap();
    let first = db
        .tag_vocabulary_page_at_revision(revision, "a", None, 1)
        .unwrap();
    let cursor = first.next_cursor.as_ref().expect("second matching tag");
    assert!(matches!(
        db.tag_vocabulary_page_at_revision(revision, "b", Some(cursor), 1),
        Err(SourceDbError::Unexpected)
    ));

    db.assign_tag_to_path(Path::new("one.wav"), "later")
        .unwrap();
    assert!(matches!(
        db.tag_vocabulary_page_at_revision(revision, "a", Some(cursor), 1),
        Err(SourceDbError::Unexpected)
    ));
}

#[test]
fn tag_page_bounds_legacy_long_multibyte_and_nul_labels_while_advancing() {
    let dir = tempdir().unwrap();
    let db = SourceDatabase::open_for_user_metadata_write(dir.path()).unwrap();
    db.upsert_file(Path::new("one.wav"), 10, 5).unwrap();
    db.upsert_file(Path::new("two.wav"), 10, 5).unwrap();
    db.upsert_file(Path::new("three.wav"), 10, 5).unwrap();
    db.upsert_file(Path::new("four.wav"), 10, 5).unwrap();
    db.assign_tag_to_path(Path::new("one.wav"), &"a".repeat(2_000))
        .unwrap();
    db.assign_tag_to_path(Path::new("two.wav"), &"é".repeat(600))
        .unwrap();
    db.assign_tag_to_path(Path::new("three.wav"), "nul\0tag")
        .unwrap();
    db.assign_tag_to_path(Path::new("four.wav"), "tail")
        .unwrap();
    let revision = db.get_revision().unwrap();

    let mut cursor = None;
    let mut entries = Vec::new();
    loop {
        let page = db
            .tag_vocabulary_page_at_revision(revision, "", cursor.as_ref(), 1)
            .unwrap();
        entries.extend(page.entries);
        let Some(next_cursor) = page.next_cursor else {
            break;
        };
        cursor = Some(next_cursor);
    }

    assert_eq!(entries.len(), 4);
    assert!(entries.iter().any(|entry| entry.normalized_text == "tail"));
    assert!(
        entries
            .iter()
            .all(|entry| entry.display_label.len() <= SOURCE_TAG_PAGE_MAX_PREFIX_BYTES)
    );
    assert!(
        entries
            .iter()
            .all(|entry| entry.normalized_text.len() <= SOURCE_TAG_PAGE_MAX_PREFIX_BYTES)
    );
    assert_eq!(entries.iter().filter(|entry| entry.is_preview).count(), 3);
}

#[test]
fn selected_tag_page_accepts_full_path_count_without_sqlite_bind_limit() {
    let dir = tempdir().unwrap();
    let db = SourceDatabase::open_for_user_metadata_write(dir.path()).unwrap();
    let paths = (0..SOURCE_TAG_PAGE_MAX_PATHS)
        .map(|index| PathBuf::from(format!("sample-{index:05}.wav")))
        .collect::<Vec<_>>();
    let last = paths.last().unwrap();
    db.upsert_file(last, 10, 5).unwrap();
    db.assign_tag_to_path(last, "boundary").unwrap();
    let revision = db.get_revision().unwrap();
    let page = db
        .tags_for_paths_page_at_revision(revision, &paths, None, 1)
        .unwrap();
    assert_eq!(page.entries.len(), 1);
    assert_eq!(page.entries[0].normalized_text, "boundary");
    assert_eq!(page.entries[0].assignment_count, Some(1));
    assert!(page.next_cursor.is_none());
}
