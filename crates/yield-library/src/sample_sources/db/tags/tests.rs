use std::path::{Path, PathBuf};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

use rusqlite::params;
use tempfile::tempdir;

use super::super::{
    SOURCE_DISPLAY_LABEL_MAX_OUTPUT_BYTES, SOURCE_DISPLAY_LABEL_MAX_PATH_BYTES,
    SOURCE_DISPLAY_LABEL_MAX_PATHS, SOURCE_TAG_FILTER_MAX_TAGS, SOURCE_TAG_FILTER_MAX_TOKEN_BYTES,
    SOURCE_TAG_PAGE_MAX_PATH_BYTES, SOURCE_TAG_PAGE_MAX_PATHS, SOURCE_TAG_PAGE_MAX_PREFIX_BYTES,
    SOURCE_TAG_PAGE_MAX_TAG_IDS, SourceDatabase, SourceDbError,
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

#[test]
fn selected_tag_ids_page_returns_zero_mixed_and_all_counts() {
    let dir = tempdir().unwrap();
    let db = SourceDatabase::open_for_user_metadata_write(dir.path()).unwrap();
    for path in ["one.wav", "two.wav", "three.wav"] {
        db.upsert_file(Path::new(path), 10, 5).unwrap();
    }
    let alpha = db
        .assign_tag_to_path(Path::new("one.wav"), "alpha")
        .unwrap();
    db.assign_tag_to_path(Path::new("two.wav"), "alpha")
        .unwrap();
    let beta = db.assign_tag_to_path(Path::new("one.wav"), "beta").unwrap();
    let gamma = db
        .assign_tag_to_path(Path::new("three.wav"), "gamma")
        .unwrap();
    let revision = db.get_revision().unwrap();
    let cancellation = Arc::new(AtomicBool::new(false));

    let page = db
        .tags_for_paths_and_ids_page_at_revision(
            revision,
            &[
                PathBuf::from("one.wav"),
                PathBuf::from("two.wav"),
                PathBuf::from("one.wav"),
            ],
            &[gamma.id, beta.id, alpha.id, alpha.id, 999_999],
            None,
            64,
            &cancellation,
        )
        .unwrap();
    assert_eq!(
        page.entries
            .iter()
            .map(|entry| (
                entry.id,
                entry.normalized_text.as_str(),
                entry.assignment_count
            ))
            .collect::<Vec<_>>(),
        vec![
            (alpha.id, "alpha", Some(2)),
            (beta.id, "beta", Some(1)),
            (gamma.id, "gamma", Some(0)),
        ]
    );

    let empty_selection = db
        .tags_for_paths_and_ids_page_at_revision(
            revision,
            &[],
            &[gamma.id, beta.id, alpha.id],
            None,
            64,
            &cancellation,
        )
        .unwrap();
    assert_eq!(
        empty_selection
            .entries
            .iter()
            .map(|entry| (entry.normalized_text.as_str(), entry.assignment_count))
            .collect::<Vec<_>>(),
        vec![("alpha", Some(0)), ("beta", Some(0)), ("gamma", Some(0)),]
    );
}

#[test]
fn selected_tag_ids_page_cursor_fences_paths_ids_and_revision() {
    let dir = tempdir().unwrap();
    let db = SourceDatabase::open_for_user_metadata_write(dir.path()).unwrap();
    for path in ["one.wav", "two.wav"] {
        db.upsert_file(Path::new(path), 10, 5).unwrap();
    }
    let alpha = db
        .assign_tag_to_path(Path::new("one.wav"), "alpha")
        .unwrap();
    let beta = db.assign_tag_to_path(Path::new("one.wav"), "beta").unwrap();
    let gamma = db
        .assign_tag_to_path(Path::new("one.wav"), "gamma")
        .unwrap();
    let revision = db.get_revision().unwrap();
    let cancellation = Arc::new(AtomicBool::new(false));
    let first = db
        .tags_for_paths_and_ids_page_at_revision(
            revision,
            &[PathBuf::from("one.wav")],
            &[gamma.id, alpha.id, beta.id],
            None,
            1,
            &cancellation,
        )
        .unwrap();
    assert_eq!(first.entries[0].id, alpha.id);
    let cursor = first.next_cursor.as_ref().expect("tag page continuation");

    let second = db
        .tags_for_paths_and_ids_page_at_revision(
            revision,
            &[PathBuf::from("one.wav")],
            &[beta.id, alpha.id, beta.id, gamma.id],
            Some(cursor),
            1,
            &cancellation,
        )
        .unwrap();
    assert_eq!(second.entries[0].id, beta.id);

    assert!(matches!(
        db.tags_for_paths_and_ids_page_at_revision(
            revision,
            &[PathBuf::from("one.wav")],
            &[alpha.id, gamma.id],
            Some(cursor),
            1,
            &cancellation,
        ),
        Err(SourceDbError::Unexpected)
    ));
    assert!(matches!(
        db.tags_for_paths_and_ids_page_at_revision(
            revision,
            &[PathBuf::from("two.wav")],
            &[alpha.id, beta.id, gamma.id],
            Some(cursor),
            1,
            &cancellation,
        ),
        Err(SourceDbError::Unexpected)
    ));

    db.assign_tag_to_path(Path::new("two.wav"), "later")
        .unwrap();
    assert!(matches!(
        db.tags_for_paths_and_ids_page_at_revision(
            revision,
            &[PathBuf::from("one.wav")],
            &[alpha.id, beta.id, gamma.id],
            Some(cursor),
            1,
            &cancellation,
        ),
        Err(SourceDbError::Unexpected)
    ));
}

#[test]
fn selected_tag_ids_page_bounds_ids_legacy_labels_and_cancellation() {
    let dir = tempdir().unwrap();
    let db = SourceDatabase::open_for_user_metadata_write(dir.path()).unwrap();
    db.upsert_file(Path::new("legacy.wav"), 10, 5).unwrap();
    db.connection
        .execute(
            "INSERT INTO source_tags (normalized_text, display_label) VALUES (?1, ?2)",
            params!["legacy\0suffix", "x".repeat(1_000_000)],
        )
        .unwrap();
    let legacy_id: i64 = db
        .connection
        .query_row(
            "SELECT id FROM source_tags WHERE normalized_text = ?1",
            params!["legacy\0suffix"],
            |row| row.get(0),
        )
        .unwrap();
    db.connection
        .execute(
            "INSERT INTO wav_file_tags (path, tag_id) VALUES (?1, ?2)",
            params!["legacy.wav", legacy_id],
        )
        .unwrap();
    let revision = db.get_revision().unwrap();
    let cancellation = Arc::new(AtomicBool::new(true));
    assert!(matches!(
        db.tags_for_paths_and_ids_page_at_revision(
            revision,
            &[PathBuf::from("legacy.wav")],
            &[legacy_id],
            None,
            1,
            &cancellation,
        ),
        Err(SourceDbError::Canceled)
    ));
    cancellation.store(false, Ordering::Release);
    let page = db
        .tags_for_paths_and_ids_page_at_revision(
            revision,
            &[PathBuf::from("legacy.wav")],
            &[legacy_id],
            None,
            1,
            &cancellation,
        )
        .unwrap();
    assert_eq!(page.entries.len(), 1);
    assert_eq!(page.entries[0].id, legacy_id);
    assert_eq!(page.entries[0].assignment_count, Some(1));
    assert!(page.entries[0].is_preview);
    assert!(page.entries[0].display_label.len() <= 256);
    assert!(page.entries[0].normalized_text.len() <= 256);

    let too_many_ids = (1..=(SOURCE_TAG_PAGE_MAX_TAG_IDS as i64 + 1)).collect::<Vec<_>>();
    assert!(matches!(
        db.tags_for_paths_and_ids_page_at_revision(
            revision,
            &[],
            &too_many_ids,
            None,
            1,
            &cancellation,
        ),
        Err(SourceDbError::TagPageTooManyTagIds { .. })
    ));
    assert!(matches!(
        db.tags_for_paths_and_ids_page_at_revision(
            revision,
            &[],
            &[legacy_id, 0],
            None,
            1,
            &cancellation,
        ),
        Err(SourceDbError::InvalidTagId { tag_id: 0 })
    ));
}

#[test]
fn tag_filter_visits_casefolded_and_deduplicated_punctuation_exact_matches() {
    let dir = tempdir().unwrap();
    let db = SourceDatabase::open_for_user_metadata_write(dir.path()).unwrap();
    for path in ["one.wav", "two.wav", "three.wav"] {
        db.upsert_file(Path::new(path), 10, 5).unwrap();
    }
    db.assign_tag_to_path(Path::new("one.wav"), "Warm Tone")
        .unwrap();
    db.assign_tag_to_path(Path::new("one.wav"), "foo%literal")
        .unwrap();
    db.assign_tag_to_path(Path::new("two.wav"), "warm tone")
        .unwrap();
    db.assign_tag_to_path(Path::new("two.wav"), "foo_literal")
        .unwrap();
    db.assign_tag_to_path(Path::new("three.wav"), "warm tone")
        .unwrap();
    db.assign_tag_to_path(Path::new("three.wav"), "foo,bar")
        .unwrap();
    let revision = db.get_revision().unwrap();
    let cancellation = Arc::new(AtomicBool::new(false));
    let mut matches = Vec::new();
    let result = db
        .visit_paths_matching_tags_at_revision(
            revision,
            &[
                String::from(" WARM TONE "),
                String::from("warm tone"),
                String::from("FOO%LITERAL"),
            ],
            &cancellation,
            |path| {
                matches.push(path.to_owned());
                Ok(())
            },
        )
        .unwrap();
    assert_eq!(matches, vec![String::from("one.wav")]);
    assert_eq!(result.revision, revision);
    assert_eq!(result.matched_paths, 1);

    matches.clear();
    let result = db
        .visit_paths_matching_tags_at_revision(
            revision,
            &[String::from("foo,bar")],
            &cancellation,
            |path| {
                matches.push(path.to_owned());
                Ok(())
            },
        )
        .unwrap();
    assert_eq!(matches, vec![String::from("three.wav")]);
    assert_eq!(result.matched_paths, 1);

    matches.clear();
    let result = db
        .visit_paths_matching_tags_at_revision(
            revision,
            &[String::from("foo_literal")],
            &cancellation,
            |path| {
                matches.push(path.to_owned());
                Ok(())
            },
        )
        .unwrap();
    assert_eq!(matches, vec![String::from("two.wav")]);
    assert_eq!(result.matched_paths, 1);
}

#[test]
fn tag_filter_excludes_missing_and_unsupported_rows_and_empty_matches_live_supported() {
    let dir = tempdir().unwrap();
    let db = SourceDatabase::open_for_user_metadata_write(dir.path()).unwrap();
    for path in ["live.wav", "missing.wav", "notes.txt", "._sidecar.wav"] {
        db.upsert_file(Path::new(path), 10, 5).unwrap();
        db.assign_tag_to_path(Path::new(path), "shared").unwrap();
    }
    db.set_missing(Path::new("missing.wav"), true).unwrap();
    let revision = db.get_revision().unwrap();
    let cancellation = Arc::new(AtomicBool::new(false));
    let mut matches = Vec::new();
    let result = db
        .visit_paths_matching_tags_at_revision(
            revision,
            &[String::from("SHARED")],
            &cancellation,
            |path| {
                matches.push(path.to_owned());
                Ok(())
            },
        )
        .unwrap();
    assert_eq!(matches, vec![String::from("live.wav")]);
    assert_eq!(result.matched_paths, 1);

    matches.clear();
    let result = db
        .visit_paths_matching_tags_at_revision(revision, &[] as &[String], &cancellation, |path| {
            matches.push(path.to_owned());
            Ok(())
        })
        .unwrap();
    assert_eq!(matches, vec![String::from("live.wav")]);
    assert_eq!(result.matched_paths, 1);
}

#[test]
fn tag_filter_rejects_stale_and_overlarge_requests() {
    let dir = tempdir().unwrap();
    let db = SourceDatabase::open_for_user_metadata_write(dir.path()).unwrap();
    db.upsert_file(Path::new("one.wav"), 10, 5).unwrap();
    db.assign_tag_to_path(Path::new("one.wav"), "shared")
        .unwrap();
    let revision = db.get_revision().unwrap();
    let cancellation = Arc::new(AtomicBool::new(false));
    db.assign_tag_to_path(Path::new("one.wav"), "later")
        .unwrap();
    assert!(matches!(
        db.visit_paths_matching_tags_at_revision(
            revision,
            &[String::from("shared")],
            &cancellation,
            |_| Ok(())
        ),
        Err(SourceDbError::Unexpected)
    ));

    let too_many = (0..=SOURCE_TAG_FILTER_MAX_TAGS)
        .map(|index| format!("tag-{index}"))
        .collect::<Vec<_>>();
    assert!(matches!(
        db.visit_paths_matching_tags_at_revision(
            revision + 1,
            &too_many,
            &cancellation,
            |_| Ok(())
        ),
        Err(SourceDbError::TagFilterTooManyTokens { .. })
    ));
    let too_long = vec!["x".repeat(SOURCE_TAG_FILTER_MAX_TOKEN_BYTES + 1)];
    assert!(matches!(
        db.visit_paths_matching_tags_at_revision(
            revision + 1,
            &too_long,
            &cancellation,
            |_| Ok(())
        ),
        Err(SourceDbError::TagFilterTokenTooLong { .. })
    ));
}

#[test]
fn tag_filter_cancellation_and_visitor_errors_leave_no_temp_scope() {
    let dir = tempdir().unwrap();
    let db = SourceDatabase::open_for_user_metadata_write(dir.path()).unwrap();
    for path in ["one.wav", "two.wav"] {
        db.upsert_file(Path::new(path), 10, 5).unwrap();
        db.assign_tag_to_path(Path::new(path), "shared").unwrap();
    }
    let revision = db.get_revision().unwrap();
    let cancellation = Arc::new(AtomicBool::new(true));
    let mut visited = 0;
    assert!(matches!(
        db.visit_paths_matching_tags_at_revision(
            revision,
            &[String::from("shared")],
            &cancellation,
            |_| {
                visited += 1;
                Ok(())
            }
        ),
        Err(SourceDbError::Canceled)
    ));
    assert_eq!(visited, 0);

    cancellation.store(false, std::sync::atomic::Ordering::Release);
    let cancellation_for_visitor = Arc::clone(&cancellation);
    let mut visited = 0;
    assert!(matches!(
        db.visit_paths_matching_tags_at_revision(
            revision,
            &[String::from("shared")],
            &cancellation,
            |_| {
                visited += 1;
                cancellation_for_visitor.store(true, std::sync::atomic::Ordering::Release);
                Ok(())
            }
        ),
        Err(SourceDbError::Canceled)
    ));
    assert_eq!(visited, 1);

    cancellation.store(false, std::sync::atomic::Ordering::Release);
    let mut visited = 0;
    assert!(matches!(
        db.visit_paths_matching_tags_at_revision(
            revision,
            &[String::from("shared")],
            &cancellation,
            |_| {
                visited += 1;
                Err(SourceDbError::Unexpected)
            }
        ),
        Err(SourceDbError::Unexpected)
    ));
    assert_eq!(visited, 1);

    let mut matches = Vec::new();
    let result = db
        .visit_paths_matching_tags_at_revision(
            revision,
            &[String::from("shared")],
            &cancellation,
            |path| {
                matches.push(path.to_owned());
                Ok(())
            },
        )
        .unwrap();
    assert_eq!(
        matches,
        vec![String::from("one.wav"), String::from("two.wav")]
    );
    assert_eq!(result.matched_paths, 2);
}

#[test]
fn tag_filter_does_not_decode_legacy_display_labels_and_uses_tag_id_index() {
    let dir = tempdir().unwrap();
    let db = SourceDatabase::open_for_user_metadata_write(dir.path()).unwrap();
    db.upsert_file(Path::new("legacy.wav"), 10, 5).unwrap();
    db.connection
        .execute(
            "INSERT INTO source_tags (normalized_text, display_label) VALUES (?1, ?2)",
            params!["legacy", "x".repeat(1_000_000)],
        )
        .unwrap();
    db.connection
        .execute(
            "INSERT INTO source_tags (normalized_text, display_label) VALUES (?1, ?2)",
            params!["LEGACY", "legacy variant"],
        )
        .unwrap();
    let tag_id: i64 = db
        .connection
        .query_row(
            "SELECT id FROM source_tags WHERE normalized_text = 'legacy'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    db.connection
        .execute(
            "INSERT INTO wav_file_tags (path, tag_id) VALUES (?1, ?2)",
            params!["legacy.wav", tag_id],
        )
        .unwrap();
    let variant_tag_id: i64 = db
        .connection
        .query_row(
            "SELECT id FROM source_tags WHERE normalized_text = 'LEGACY'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    db.connection
        .execute(
            "INSERT INTO wav_file_tags (path, tag_id) VALUES (?1, ?2)",
            params!["legacy.wav", variant_tag_id],
        )
        .unwrap();
    let revision = db.get_revision().unwrap();
    let cancellation = Arc::new(AtomicBool::new(false));
    let mut matches = Vec::new();
    db.visit_paths_matching_tags_at_revision(
        revision,
        &[String::from("LEGACY")],
        &cancellation,
        |path| {
            matches.push(path.to_owned());
            Ok(())
        },
    )
    .unwrap();
    assert_eq!(matches, vec![String::from("legacy.wav")]);

    db.connection
        .execute_batch(
            "CREATE TEMP TABLE tag_filter_tokens (
                ordinal INTEGER PRIMARY KEY,
                normalized_text TEXT NOT NULL
            ) WITHOUT ROWID",
        )
        .unwrap();
    db.connection
        .execute(
            "INSERT INTO temp.tag_filter_tokens (ordinal, normalized_text) VALUES (?1, ?2)",
            params![0_i64, "legacy"],
        )
        .unwrap();
    let explain_sql = format!(
        "EXPLAIN QUERY PLAN {}",
        super::matching_tag_filter_paths_sql_for_test()
    );
    let plan = db
        .connection
        .prepare(&explain_sql)
        .unwrap()
        .query_map([1_i64], |row| row.get::<_, String>(3))
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    assert!(
        plan.iter().any(|detail| {
            detail.contains("SEARCH") && detail.contains("idx_wav_file_tags_tag_id")
        }),
        "tag filter plan did not use tag-id index: {plan:?}"
    );
    assert!(
        plan.iter().any(|detail| {
            detail.contains("SEARCH") && detail.contains("idx_source_tags_normalized_text_nocase")
        }),
        "tag filter plan did not use normalized-token index: {plan:?}"
    );
}

#[test]
fn tag_filter_ascii_casefold_does_not_truncate_embedded_nul_values() {
    let dir = tempdir().unwrap();
    let db = SourceDatabase::open_for_user_metadata_write(dir.path()).unwrap();
    db.upsert_file(Path::new("nul.wav"), 10, 5).unwrap();
    let stored = "A\0x";
    db.connection
        .execute(
            "INSERT INTO source_tags (normalized_text, display_label) VALUES (?1, ?2)",
            params![stored, "nul suffix"],
        )
        .unwrap();
    let tag_id: i64 = db
        .connection
        .query_row(
            "SELECT id FROM source_tags WHERE normalized_text = ?1",
            params![stored],
            |row| row.get(0),
        )
        .unwrap();
    db.connection
        .execute(
            "INSERT INTO wav_file_tags (path, tag_id) VALUES (?1, ?2)",
            params!["nul.wav", tag_id],
        )
        .unwrap();
    let revision = db.get_revision().unwrap();
    let cancellation = Arc::new(AtomicBool::new(false));

    let mut matches = Vec::new();
    db.visit_paths_matching_tags_at_revision(
        revision,
        &[String::from("a\0x")],
        &cancellation,
        |path| {
            matches.push(path.to_owned());
            Ok(())
        },
    )
    .unwrap();
    assert_eq!(matches, vec![String::from("nul.wav")]);

    matches.clear();
    db.visit_paths_matching_tags_at_revision(
        revision,
        &[String::from("a\0y")],
        &cancellation,
        |path| {
            matches.push(path.to_owned());
            Ok(())
        },
    )
    .unwrap();
    assert!(matches.is_empty());
}

#[test]
fn display_labels_preserve_native_order_playback_sanitization_and_empty_fallback() {
    let dir = tempdir().unwrap();
    let db = SourceDatabase::open_for_user_metadata_write(dir.path()).unwrap();
    for path in [
        "one.wav",
        "empty.wav",
        "missing.wav",
        "notes.txt",
        "._sidecar.wav",
    ] {
        db.upsert_file(Path::new(path), 10, 5).unwrap();
    }
    for label in ["zulu", "Alpha", "loop", "one-shot", "warm"] {
        db.assign_tag_to_path(Path::new("one.wav"), label).unwrap();
    }
    for path in ["missing.wav", "notes.txt", "._sidecar.wav"] {
        db.assign_tag_to_path(Path::new(path), "hidden").unwrap();
    }
    db.set_missing(Path::new("missing.wav"), true).unwrap();
    let revision = db.get_revision().unwrap();
    let cancellation = Arc::new(AtomicBool::new(false));

    let result = db
        .display_labels_for_paths_at_revision(
            revision,
            &[
                PathBuf::from("one.wav"),
                PathBuf::from("empty.wav"),
                PathBuf::from("one.wav"),
                PathBuf::from("missing.wav"),
                PathBuf::from("notes.txt"),
                PathBuf::from("._sidecar.wav"),
            ],
            &cancellation,
        )
        .unwrap();

    assert_eq!(result.revision, revision);
    assert_eq!(
        result
            .entries
            .iter()
            .map(|entry| (
                entry.relative_path.to_string_lossy().into_owned(),
                entry.display_label.clone(),
                entry.truncated,
            ))
            .collect::<Vec<_>>(),
        vec![
            (String::from("empty.wav"), None, false),
            (
                String::from("one.wav"),
                Some(String::from("Alpha_loop_warm_zulu")),
                false,
            ),
        ]
    );
}

#[test]
fn display_labels_bound_legacy_text_and_high_tag_cardinality() {
    let dir = tempdir().unwrap();
    let db = SourceDatabase::open_for_user_metadata_write(dir.path()).unwrap();
    for path in ["legacy-malformed.wav", "legacy-long.wav", "many.wav"] {
        db.upsert_file(Path::new(path), 10, 5).unwrap();
    }

    let insert_legacy_tag = |path: &str, normalized: &str, display: &[u8]| {
        db.connection
            .execute(
                "INSERT INTO source_tags (normalized_text, display_label) VALUES (?1, ?2)",
                params![normalized, display],
            )
            .unwrap();
        let tag_id = db.connection.last_insert_rowid();
        db.connection
            .execute(
                "INSERT INTO wav_file_tags (path, tag_id) VALUES (?1, ?2)",
                params![path, tag_id],
            )
            .unwrap();
    };
    insert_legacy_tag("legacy-malformed.wav", "nul-display", b"nul\0display");
    insert_legacy_tag(
        "legacy-malformed.wav",
        "invalid-display",
        &[b'i', 0xff, b'n', 0xfe],
    );
    let long_display = b"long".repeat(2_000);
    insert_legacy_tag("legacy-long.wav", "long-display", &long_display);

    for index in 0..=MAX_LEGACY_DISPLAY_LABEL_TAGS {
        let normalized = format!("tag-{index:03}");
        insert_legacy_tag("many.wav", &normalized, normalized.as_bytes());
    }

    let revision = db.get_revision().unwrap();
    let cancellation = Arc::new(AtomicBool::new(false));
    let assignments_before: i64 = db
        .connection
        .query_row("SELECT count(*) FROM wav_file_tags", [], |row| row.get(0))
        .unwrap();
    let result = db
        .display_labels_for_paths_at_revision(
            revision,
            &[
                PathBuf::from("legacy-malformed.wav"),
                PathBuf::from("legacy-long.wav"),
                PathBuf::from("many.wav"),
            ],
            &cancellation,
        )
        .unwrap();
    assert_eq!(result.revision, revision);
    assert_eq!(db.get_revision().unwrap(), revision);
    let assignments_after: i64 = db
        .connection
        .query_row("SELECT count(*) FROM wav_file_tags", [], |row| row.get(0))
        .unwrap();
    assert_eq!(assignments_after, assignments_before);

    let malformed = result
        .entries
        .iter()
        .find(|entry| entry.relative_path == Path::new("legacy-malformed.wav"))
        .unwrap();
    assert!(malformed.truncated);
    assert!(
        malformed
            .display_label
            .as_deref()
            .is_some_and(|label| !label.contains('\0') && label.contains('\u{fffd}'))
    );

    let long = result
        .entries
        .iter()
        .find(|entry| entry.relative_path == Path::new("legacy-long.wav"))
        .unwrap();
    assert!(long.truncated);
    assert_eq!(
        long.display_label.as_ref().unwrap().len(),
        SOURCE_DISPLAY_LABEL_MAX_OUTPUT_BYTES
    );

    let many = result
        .entries
        .iter()
        .find(|entry| entry.relative_path == Path::new("many.wav"))
        .unwrap();
    assert!(many.truncated);
    assert!(many.display_label.as_ref().unwrap().len() <= SOURCE_DISPLAY_LABEL_MAX_OUTPUT_BYTES);
}

#[test]
fn display_labels_reject_raw_path_bounds_and_deduplicate_normalized_paths() {
    let dir = tempdir().unwrap();
    let db = SourceDatabase::open_for_user_metadata_write(dir.path()).unwrap();
    db.upsert_file(Path::new("folder/one.wav"), 10, 5).unwrap();
    let revision = db.get_revision().unwrap();
    let cancellation = Arc::new(AtomicBool::new(false));

    let duplicate = db
        .display_labels_for_paths_at_revision(
            revision,
            &[
                PathBuf::from("folder/./one.wav"),
                PathBuf::from("folder/one.wav"),
            ],
            &cancellation,
        )
        .unwrap();
    assert_eq!(duplicate.entries.len(), 1);
    assert_eq!(
        duplicate.entries[0].relative_path,
        PathBuf::from("folder/one.wav")
    );

    let too_many = (0..=SOURCE_DISPLAY_LABEL_MAX_PATHS)
        .map(|index| PathBuf::from(format!("{index}.wav")))
        .collect::<Vec<_>>();
    assert!(matches!(
        db.display_labels_for_paths_at_revision(revision, &too_many, &cancellation),
        Err(SourceDbError::DisplayLabelTooManyPaths { .. })
    ));

    let too_many_bytes = vec![PathBuf::from(
        "x".repeat(SOURCE_DISPLAY_LABEL_MAX_PATH_BYTES + 1),
    )];
    assert!(matches!(
        db.display_labels_for_paths_at_revision(revision, &too_many_bytes, &cancellation),
        Err(SourceDbError::DisplayLabelPathBytesTooLarge { .. })
    ));

    let combined_bytes = vec![PathBuf::from(
        "y".repeat(SOURCE_DISPLAY_LABEL_MAX_PATH_BYTES / 2 + 1),
    )];
    assert!(matches!(
        db.display_labels_for_paths_at_revision(revision, &combined_bytes, &cancellation),
        Err(SourceDbError::DisplayLabelPathBytesTooLarge { .. })
    ));
}

#[test]
fn display_labels_fence_revision_and_cancelled_reads_recover_connection() {
    let dir = tempdir().unwrap();
    let db = SourceDatabase::open_for_user_metadata_write(dir.path()).unwrap();
    db.upsert_file(Path::new("one.wav"), 10, 5).unwrap();
    db.assign_tag_to_path(Path::new("one.wav"), "warm").unwrap();
    for index in 0..10_000 {
        let normalized = format!("legacy-{index:05}");
        db.connection
            .execute(
                "INSERT INTO source_tags (normalized_text, display_label) VALUES (?1, ?2)",
                params![normalized, normalized],
            )
            .unwrap();
        let tag_id = db.connection.last_insert_rowid();
        db.connection
            .execute(
                "INSERT INTO wav_file_tags (path, tag_id) VALUES (?1, ?2)",
                params!["one.wav", tag_id],
            )
            .unwrap();
    }
    let revision = db.get_revision().unwrap();
    let cancellation = Arc::new(AtomicBool::new(false));

    db.assign_tag_to_path(Path::new("one.wav"), "later")
        .unwrap();
    assert!(matches!(
        db.display_labels_for_paths_at_revision(
            revision,
            &[PathBuf::from("one.wav")],
            &cancellation,
        ),
        Err(SourceDbError::DisplayLabelStaleRevision { .. })
    ));

    let revision = db.get_revision().unwrap();
    let canceled = Arc::new(AtomicBool::new(false));
    let callback_cancellation = Arc::clone(&canceled);
    let callback = Arc::new(move || {
        callback_cancellation.store(true, Ordering::Release);
    });
    let result = db.display_labels_for_paths_at_revision_with_progress_callback(
        revision,
        &[PathBuf::from("one.wav")],
        &canceled,
        Some(callback),
    );
    assert!(matches!(result, Err(SourceDbError::Canceled)));

    canceled.store(false, Ordering::Release);
    let recovered = db
        .display_labels_for_paths_at_revision(revision, &[PathBuf::from("one.wav")], &canceled)
        .unwrap();
    assert_eq!(recovered.entries.len(), 1);
}

#[test]
fn display_label_query_uses_path_primary_key_lookup() {
    let dir = tempdir().unwrap();
    let db = SourceDatabase::open_for_user_metadata_write(dir.path()).unwrap();
    db.upsert_file(Path::new("one.wav"), 10, 5).unwrap();
    let sql = format!(
        "EXPLAIN QUERY PLAN {}",
        super::display_label_rows_sql_for_test()
    );
    let plan = db
        .connection
        .prepare(&sql)
        .unwrap()
        .query_map(params!["one.wav", 65_i64], |row| row.get::<_, String>(3))
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    assert!(
        plan.iter()
            .any(|detail| detail.contains("SEARCH") && detail.contains("wav_files")),
        "display-label plan did not seek the selected wav row: {plan:?}"
    );
    assert!(
        plan.iter().any(|detail| {
            detail.contains("SEARCH") && detail.contains("PRIMARY KEY") && detail.contains("path")
        }),
        "display-label plan did not use the path primary key: {plan:?}"
    );
}

const MAX_LEGACY_DISPLAY_LABEL_TAGS: usize = 70;
