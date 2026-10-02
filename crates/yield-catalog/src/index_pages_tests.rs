// Included inside the existing catalog test module to reuse its isolated real-profile fixtures.
fn index_request(
    source_id: &SourceId,
    generation: u64,
    query_id: u64,
    expected: Option<u64>,
    cursor: Option<CatalogIndexPageCursor>,
    limit: usize,
) -> CatalogCommand {
    CatalogCommand::RequestIndexPage {
        source_id: source_id.clone(),
        lifecycle_generation: generation,
        expected_index_revision: expected,
        query_id,
        after_cursor: cursor,
        page_limit: limit,
    }
}

fn receive_index_page(service: &CatalogService, query_id: u64) -> CatalogIndexPage {
    match receive_until(
        service,
        |event| matches!(event, CatalogEvent::IndexPageReady { query_id: id, .. } | CatalogEvent::IndexPageFailed { query_id: id, .. } if *id == query_id),
    ) {
        CatalogEvent::IndexPageReady {
            completeness, page, ..
        } => {
            assert_eq!(completeness, RetainedIndexCompleteness::RetainedUnknown);
            page
        }
        event => panic!("index read failed: {event:?}"),
    }
}

fn wait_for_index_queue(service: &CatalogService, count: usize) {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let queued = service
            .events
            .queue
            .lock()
            .unwrap()
            .events
            .iter()
            .filter(|event| {
                matches!(
                    event.event,
                    CatalogEvent::IndexPageReady { .. } | CatalogEvent::IndexPageFailed { .. }
                )
            })
            .count();
        if queued >= count {
            return;
        }
        assert!(Instant::now() < deadline, "index page did not queue");
        thread::yield_now();
    }
}

#[test]
fn retained_index_pages_traverse_unicode_and_fence_query_and_index_revision() {
    let fixture = Fixture::new("retained-index-traversal");
    for name in ["a.txt", "b.txt", "c.txt", "日本語.txt"] {
        fs::write(fixture.root.path().join(name), b"retained nonaudio facts").unwrap();
    }
    let id = SourceId::from_string("index-traversal");
    let service = CatalogService::start(&fixture.guard).unwrap();
    register_and_audit(&service, fixture.root.path(), &id);
    service
        .submit(index_request(&id, 1, 10, None, None, 2))
        .unwrap();
    let first = receive_index_page(&service, 10);
    assert!(first.schema_available);
    assert_eq!(first.entries.len(), 2);
    let cursor = first.next.clone().expect("continuation");
    service
        .submit(index_request(
            &id,
            1,
            10,
            Some(first.index_revision),
            Some(cursor.clone()),
            2,
        ))
        .unwrap();
    let second = receive_index_page(&service, 10);
    let names: Vec<_> = first
        .entries
        .iter()
        .chain(second.entries.iter())
        .map(|entry| entry.relative_path.as_path().to_path_buf())
        .collect();
    assert_eq!(
        names,
        ["a.txt", "b.txt", "c.txt", "日本語.txt"].map(PathBuf::from)
    );
    assert!(second.next.is_none());
    assert_eq!(second.index_revision, first.index_revision);
    assert!(
        first
            .entries
            .iter()
            .chain(second.entries.iter())
            .all(|entry| entry.classification
                == yield_library::sample_sources::SourceIndexClassification::UnsupportedNonAudio)
    );

    service
        .submit(index_request(&id, 1, 11, None, Some(cursor.clone()), 2))
        .unwrap();
    assert!(matches!(
        receive_until(&service, |event| matches!(
            event,
            CatalogEvent::IndexPageFailed { query_id: 11, .. }
        )),
        CatalogEvent::IndexPageFailed {
            error: CatalogIndexPageError::ForeignCursor,
            ..
        }
    ));
    // Reusing a query number cannot revive the prior query token/cursor.
    service
        .submit(index_request(&id, 1, 10, None, Some(cursor), 2))
        .unwrap();
    assert!(matches!(
        receive_until(&service, |event| matches!(
            event,
            CatalogEvent::IndexPageFailed { query_id: 10, .. }
        )),
        CatalogEvent::IndexPageFailed {
            error: CatalogIndexPageError::ForeignCursor,
            ..
        }
    ));

    fs::write(fixture.root.path().join("new.txt"), b"new retained fact").unwrap();
    service
        .submit(CatalogCommand::RequestAudit {
            source_id: id.clone(),
            lifecycle_generation: 1,
        })
        .unwrap();
    receive_until(&service, |event| {
        matches!(event, CatalogEvent::AuditCommitted { .. })
    });
    service
        .submit(index_request(
            &id,
            1,
            12,
            Some(first.index_revision),
            None,
            2,
        ))
        .unwrap();
    assert!(matches!(
        receive_until(&service, |event| matches!(
            event,
            CatalogEvent::IndexPageFailed { query_id: 12, .. }
        )),
        CatalogEvent::IndexPageFailed {
            error: CatalogIndexPageError::StaleRevision { .. },
            ..
        }
    ));
}

#[test]
fn retained_index_pages_discard_superseded_queued_delivery_and_recover_credits() {
    let fixture = Fixture::new("retained-index-delivery");
    fs::write(fixture.root.path().join("one.txt"), b"facts").unwrap();
    let id = SourceId::from_string("index-delivery");
    let service = CatalogService::start(&fixture.guard).unwrap();
    register_and_audit(&service, fixture.root.path(), &id);
    service
        .submit(index_request(&id, 1, 20, None, None, 1))
        .unwrap();
    wait_for_index_queue(&service, 1);
    service
        .submit(index_request(&id, 1, 21, None, None, 1))
        .unwrap();
    wait_for_index_queue(&service, 2);
    // Raw and retained-delivery APIs share the query discard boundary.
    let delivery = service.try_recv_delivery().unwrap();
    assert!(delivery.into_event_if_live().is_none());
    let page = receive_index_page(&service, 21);
    assert_eq!(page.entries.len(), 1);
    wait_for_tag_read_payload(&service, 0);
    wait_for_tag_page_input_release(&service);

    service
        .submit(index_request(&id, 1, 24, None, None, 1))
        .unwrap();
    wait_for_index_queue(&service, 1);
    service
        .submit(CatalogCommand::RequestAudit {
            source_id: id.clone(),
            lifecycle_generation: 1,
        })
        .unwrap();
    assert!(
        service
            .try_recv_delivery()
            .unwrap()
            .into_event_if_live()
            .is_none()
    );
    receive_until(&service, |event| {
        matches!(event, CatalogEvent::AuditCommitted { .. })
    });
    wait_for_tag_read_payload(&service, 0);

    service
        .submit(index_request(&id, 1, 22, None, None, 1))
        .unwrap();
    wait_for_index_queue(&service, 1);
    service
        .submit(CatalogCommand::Remove {
            source_id: id.clone(),
            lifecycle_generation: 1,
        })
        .unwrap();
    let old = service.try_recv_delivery().unwrap();
    assert!(old.into_event_if_live().is_none());
    receive_until(&service, |event| {
        matches!(event, CatalogEvent::Removed { .. })
    });
    wait_for_tag_read_payload(&service, 0);
    // A retired request consumes/relinquishes bounded credits without publishing stale facts.
    service
        .submit(index_request(&id, 1, 23, None, None, 1))
        .unwrap();
    wait_for_tag_read_payload(&service, 0);
}

#[test]
fn retained_index_page_admission_bounds_rows_bytes_and_concurrent_delivery() {
    let fixture = Fixture::new("retained-index-admission");
    let service = CatalogService::start(&fixture.guard).unwrap();
    let id = SourceId::from_string("index-bounds");
    for limit in [0, SOURCE_INDEX_PAGE_MAX_ENTRIES + 1, usize::MAX] {
        assert!(matches!(
            service.submit(index_request(&id, 1, 1, None, None, limit)),
            Err(CatalogSubmitError::InvalidIndexPageRequest(
                CatalogIndexPageError::InvalidLimit
            ))
        ));
    }
    let huge = SourceId::from_string("x".repeat(4097));
    assert!(matches!(
        service.submit(index_request(&huge, 1, 1, None, None, 1)),
        Err(CatalogSubmitError::InvalidIndexPageRequest(
            CatalogIndexPageError::InputTooLarge
        ))
    ));
    register_and_audit(&service, fixture.root.path(), &id);
    let maximum = MAX_TAG_FILTER_PAYLOAD_BYTES / MAX_INDEX_PAGE_PAYLOAD_BYTES;
    for count in 1..=maximum {
        service
            .submit(index_request(&id, 1, 50, None, None, 1))
            .unwrap();
        wait_for_index_queue(&service, count);
    }
    assert_eq!(
        admission_state(&service.admission, |state| state.tag_read_payload_bytes),
        maximum * MAX_INDEX_PAGE_PAYLOAD_BYTES
    );
    assert!(matches!(
        service.submit(index_request(&id, 1, 50, None, None, 1)),
        Err(CatalogSubmitError::Full)
    ));
    for _ in 0..maximum {
        receive_index_page(&service, 50);
    }
    wait_for_tag_read_payload(&service, 0);
    service
        .submit(index_request(&id, 1, 51, None, None, 1))
        .unwrap();
    receive_index_page(&service, 51);
}

#[test]
fn retained_index_pages_reject_reopened_lifecycle_cursor_and_preserve_raw_paths() {
    let fixture = Fixture::new("retained-index-reopen");
    fs::write(fixture.root.path().join("a.txt"), b"facts").unwrap();
    fs::write(fixture.root.path().join("b.txt"), b"facts").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStringExt;
        fs::write(
            fixture
                .root
                .path()
                .join(std::ffi::OsString::from_vec(b"raw-\xff.wav".to_vec())),
            b"facts",
        )
        .unwrap();
    }
    let id = SourceId::from_string("index-reopen");
    let service = CatalogService::start(&fixture.guard).unwrap();
    register_and_audit(&service, fixture.root.path(), &id);
    service
        .submit(index_request(&id, 1, 30, None, None, 1))
        .unwrap();
    let first = receive_index_page(&service, 30);
    let cursor = first.next.unwrap();
    service
        .submit(CatalogCommand::Register {
            source_id: id.clone(),
            canonical_root: fixture.root.path().to_path_buf(),
            lifecycle_generation: 2,
        })
        .unwrap();
    receive_until(&service, |event| {
        matches!(
            event,
            CatalogEvent::Registered {
                lifecycle_generation: 2,
                ..
            }
        )
    });
    service
        .submit(index_request(&id, 2, 30, None, Some(cursor), 1))
        .unwrap();
    assert!(matches!(
        receive_until(&service, |event| matches!(
            event,
            CatalogEvent::IndexPageFailed { query_id: 30, .. }
        )),
        CatalogEvent::IndexPageFailed {
            error: CatalogIndexPageError::ForeignCursor,
            ..
        }
    ));
    service
        .submit(index_request(&id, 2, 31, None, None, 128))
        .unwrap();
    let page = receive_index_page(&service, 31);
    #[cfg(unix)]
    assert!(page.entries.iter().any(|entry| {
        entry.relative_path.as_path().as_os_str().as_encoded_bytes() == b"raw-\xff.wav"
            && entry.diagnostic
                == Some(yield_library::sample_sources::SourceIndexDiagnostic::NonUnicodePath)
    }));
    assert!(page.schema_available);
}

#[test]
fn retained_index_pages_publish_visible_malformed_byte_and_legacy_outcomes() {
    let fixture = Fixture::new("retained-index-errors");
    fs::write(fixture.root.path().join("one.txt"), b"facts").unwrap();
    let id = SourceId::from_string("index-errors");
    let service = CatalogService::start(&fixture.guard).unwrap();
    register_and_audit(&service, fixture.root.path(), &id);
    // Deliberately corrupt an existing fixture DB, never a production owner/write route.
    let path = fixture
        .guard
        .profile_root()
        .join(catalog_profile_child(&id))
        .join(".yield.db");
    let connection =
        rusqlite::Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE)
            .unwrap();
    connection
        .execute(
            "UPDATE source_index_entries SET diagnostic = 'unknown diagnostic'",
            [],
        )
        .unwrap();
    service
        .submit(index_request(&id, 1, 40, None, None, 1))
        .unwrap();
    assert!(matches!(
        receive_until(&service, |event| matches!(
            event,
            CatalogEvent::IndexPageFailed { query_id: 40, .. }
        )),
        CatalogEvent::IndexPageFailed {
            error: CatalogIndexPageError::Database,
            ..
        }
    ));
    connection
        .execute(
            "UPDATE source_index_entries SET diagnostic = NULL, file_identity = ?1",
            ["x".repeat(yield_library::sample_sources::db::SOURCE_INDEX_PAGE_MAX_BYTES + 1)],
        )
        .unwrap();
    service
        .submit(index_request(&id, 1, 41, None, None, 1))
        .unwrap();
    assert!(matches!(
        receive_until(&service, |event| matches!(
            event,
            CatalogEvent::IndexPageFailed { query_id: 41, .. }
        )),
        CatalogEvent::IndexPageFailed {
            error: CatalogIndexPageError::ByteLimit,
            ..
        }
    ));
    connection
        .execute("DROP TABLE source_index_entries", [])
        .unwrap();
    service
        .submit(index_request(&id, 1, 42, None, None, 1))
        .unwrap();
    let unavailable = receive_index_page(&service, 42);
    assert!(!unavailable.schema_available);
    assert_eq!(unavailable.index_revision, 0);
    assert!(unavailable.entries.is_empty());
    assert!(unavailable.next.is_none());
    // The read did not repair/migrate the missing table.
    assert_eq!(
        connection
            .query_row(
                "SELECT count(*) FROM sqlite_master WHERE name = 'source_index_entries'",
                [],
                |row| row.get::<_, i64>(0)
            )
            .unwrap(),
        0
    );
    wait_for_tag_read_payload(&service, 0);
}
