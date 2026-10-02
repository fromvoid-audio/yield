// Production catalog + real isolated profile/database/source leases, not proposal fixtures.
fn inspection_for(
    service: &CatalogService,
    handle: CatalogIndexEntryHandle,
    request_id: u64,
) -> Result<CatalogIndexInspection, SourceIndexInspectionError> {
    service
        .submit(CatalogCommand::RequestIndexInspection {
            handle,
            request_id,
            cancellation: IndexInspectionCancellation::default(),
        })
        .unwrap();
    match receive_until(service, |event| {
        matches!(event,
        CatalogEvent::IndexInspectionCompleted { request_id: id, .. } if *id == request_id)
    }) {
        CatalogEvent::IndexInspectionCompleted { result, .. } => result,
        _ => unreachable!(),
    }
}
fn inspection_page(service: &CatalogService, id: &SourceId, query: u64) -> CatalogIndexPage {
    service
        .submit(index_request(id, 1, query, None, None, 128))
        .unwrap();
    receive_index_page(service, query)
}
fn handle_for(page: &CatalogIndexPage, path: &Path) -> CatalogIndexEntryHandle {
    let index = page
        .entries
        .iter()
        .position(|entry| entry.relative_path == path)
        .unwrap();
    page.inspection_handles[index].clone()
}
fn wait_inspection_queue(service: &CatalogService) {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if service
            .events
            .queue
            .lock()
            .unwrap()
            .events
            .iter()
            .any(|event| matches!(event.event, CatalogEvent::IndexInspectionCompleted { .. }))
        {
            return;
        }
        assert!(Instant::now() < deadline);
        thread::yield_now();
    }
}

#[test]
#[cfg(target_os = "linux")]
fn retained_index_inspection_holds_nested_unicode_and_raw_objects_with_bounded_permits() {
    use std::os::unix::ffi::OsStringExt;
    let fixture = Fixture::new("index-inspection-held");
    let nested = PathBuf::from("échantillons/日本語.txt");
    fs::create_dir(fixture.root.path().join("échantillons")).unwrap();
    fs::write(fixture.root.path().join(&nested), b"not audio").unwrap();
    let raw = PathBuf::from(std::ffi::OsString::from_vec(b"raw-\xff.txt".to_vec()));
    fs::write(fixture.root.path().join(&raw), b"raw facts").unwrap();
    let id = SourceId::from_string("held-inspection");
    let service = CatalogService::start(&fixture.guard).unwrap();
    register_and_audit(&service, fixture.root.path(), &id);
    let page = inspection_page(&service, &id, 1);
    assert_eq!(page.inspection_handles.len(), page.entries.len());
    let mut held = Vec::new();
    for request in 0..MAX_HELD_INDEX_INSPECTIONS {
        let path = if request % 2 == 0 { &nested } else { &raw };
        let result = inspection_for(&service, handle_for(&page, path), request as u64).unwrap();
        assert_eq!(&result.entry().relative_path, path);
        assert_eq!(result.index_revision(), page.index_revision);
        assert!(result.is_live());
        held.push(result);
    }
    assert!(matches!(
        inspection_for(&service, handle_for(&page, &nested), 99),
        Err(SourceIndexInspectionError::Busy)
    ));
    // Supersession denies projection but cannot release consumer-owned descriptors prematurely.
    assert!(!held[0].is_live());
    drop(held.pop());
    held.push(inspection_for(&service, handle_for(&page, &raw), 100).unwrap());
    assert!(held.last().unwrap().is_live());
    service.cancel_index_query(&id, 1, 1);
    assert!(held.iter().all(|inspection| !inspection.is_live()));
    drop(held);
    let next = inspection_page(&service, &id, 2);
    let result = inspection_for(&service, handle_for(&next, &nested), 101).unwrap();
    // Retaining inspection namespace custody must not prevent catalog retirement.
    service
        .submit(CatalogCommand::Remove {
            source_id: id,
            lifecycle_generation: 1,
        })
        .unwrap();
    receive_until(&service, |event| {
        matches!(event, CatalogEvent::Removed { .. })
    });
    assert!(!result.is_live());
    drop(result);
    wait_for_tag_read_payload(&service, 0);
    wait_for_tag_page_input_release(&service);
}

#[test]
#[cfg(target_os = "linux")]
fn retained_index_inspection_rejects_object_parent_symlink_fifo_and_socket_substitution() {
    use std::os::unix::{fs::symlink, net::UnixListener};
    let fixture = Fixture::new("index-inspection-substitution");
    fs::create_dir(fixture.root.path().join("nested")).unwrap();
    for name in [
        "replace.txt",
        "link.txt",
        "fifo.txt",
        "socket.txt",
        "nested/child.txt",
    ] {
        fs::write(fixture.root.path().join(name), b"original facts").unwrap();
    }
    let outside = tempfile::tempdir().unwrap();
    fs::write(outside.path().join("child.txt"), b"original facts").unwrap();
    let id = SourceId::from_string("inspection-substitution");
    let service = CatalogService::start(&fixture.guard).unwrap();
    register_and_audit(&service, fixture.root.path(), &id);
    let page = inspection_page(&service, &id, 1);
    let replace = fixture.root.path().join("replace.txt");
    fs::rename(&replace, fixture.root.path().join("held-original")).unwrap();
    fs::write(&replace, b"original facts").unwrap();
    assert!(matches!(
        inspection_for(&service, handle_for(&page, Path::new("replace.txt")), 1),
        Err(SourceIndexInspectionError::ChangedEntry)
    ));
    let link = fixture.root.path().join("link.txt");
    fs::remove_file(&link).unwrap();
    symlink(outside.path().join("child.txt"), &link).unwrap();
    assert!(matches!(
        inspection_for(&service, handle_for(&page, Path::new("link.txt")), 2),
        Err(SourceIndexInspectionError::Unavailable)
    ));
    let fifo = fixture.root.path().join("fifo.txt");
    fs::remove_file(&fifo).unwrap();
    assert!(
        std::process::Command::new("mkfifo")
            .arg(&fifo)
            .status()
            .unwrap()
            .success()
    );
    let began = Instant::now();
    assert!(matches!(
        inspection_for(&service, handle_for(&page, Path::new("fifo.txt")), 3),
        Err(SourceIndexInspectionError::Unavailable)
    ));
    assert!(began.elapsed() < Duration::from_secs(2));
    let socket = fixture.root.path().join("socket.txt");
    fs::remove_file(&socket).unwrap();
    let _socket = UnixListener::bind(socket).unwrap();
    assert!(matches!(
        inspection_for(&service, handle_for(&page, Path::new("socket.txt")), 4),
        Err(SourceIndexInspectionError::Unavailable)
    ));
    fs::rename(
        fixture.root.path().join("nested"),
        fixture.root.path().join("old-nested"),
    )
    .unwrap();
    symlink(outside.path(), fixture.root.path().join("nested")).unwrap();
    assert!(matches!(
        inspection_for(
            &service,
            handle_for(&page, Path::new("nested/child.txt")),
            5
        ),
        Err(SourceIndexInspectionError::Unavailable)
    ));
    drop(service);
}

#[test]
#[cfg(target_os = "linux")]
fn retained_index_inspection_discards_canceled_queued_results_and_foreign_queries() {
    let fixture = Fixture::new("index-inspection-delivery");
    fs::write(fixture.root.path().join("one.txt"), b"facts").unwrap();
    let id = SourceId::from_string("inspection-delivery");
    let service = CatalogService::start(&fixture.guard).unwrap();
    register_and_audit(&service, fixture.root.path(), &id);
    let page = inspection_page(&service, &id, 1);
    let handle = handle_for(&page, Path::new("one.txt"));
    let cancel = IndexInspectionCancellation::default();
    service
        .submit(CatalogCommand::RequestIndexInspection {
            handle: handle.clone(),
            request_id: 1,
            cancellation: cancel.clone(),
        })
        .unwrap();
    wait_inspection_queue(&service);
    cancel.cancel();
    assert!(
        service
            .try_recv_delivery()
            .unwrap()
            .into_event_if_live()
            .is_none()
    );
    wait_for_tag_read_payload(&service, 0);
    // Canceling/superseding a query prevents an old opaque handle from reopening admission.
    service.cancel_index_query(&id, 1, 1);
    let next = inspection_page(&service, &id, 2);
    let denied = IndexInspectionCancellation::default();
    service
        .submit(CatalogCommand::RequestIndexInspection {
            handle,
            request_id: 2,
            cancellation: denied.clone(),
        })
        .unwrap();
    assert!(denied.is_canceled());
    wait_for_tag_read_payload(&service, 0);
    assert_eq!(
        admission_state(&service.admission, |s| s.index_inspections.len()),
        0
    );
    let retained = inspection_for(&service, handle_for(&next, Path::new("one.txt")), 3).unwrap();
    // Audit invalidates handles before its index writes; it cannot reuse a held old result.
    service
        .submit(CatalogCommand::RequestAudit {
            source_id: id,
            lifecycle_generation: 1,
        })
        .unwrap();
    assert!(!retained.is_live());
    receive_until(&service, |e| {
        matches!(e, CatalogEvent::AuditCommitted { .. })
    });
    drop(retained);
}

#[test]
#[cfg(target_os = "linux")]
fn retained_index_inspection_rejects_root_replacement_and_reopened_lifecycle() {
    let fixture = Fixture::new("index-inspection-root");
    fs::write(fixture.root.path().join("one.txt"), b"facts").unwrap();
    let id = SourceId::from_string("inspection-root");
    let service = CatalogService::start(&fixture.guard).unwrap();
    register_and_audit(&service, fixture.root.path(), &id);
    let page = inspection_page(&service, &id, 1);
    let handle = handle_for(&page, Path::new("one.txt"));
    let outside = tempfile::tempdir().unwrap();
    let original = outside.path().join("original");
    fs::rename(fixture.root.path(), &original).unwrap();
    fs::create_dir(fixture.root.path()).unwrap();
    fs::write(fixture.root.path().join("one.txt"), b"facts").unwrap();
    assert!(matches!(
        inspection_for(&service, handle.clone(), 1),
        Err(SourceIndexInspectionError::OwnerUnavailable)
    ));
    fs::remove_dir_all(fixture.root.path()).unwrap();
    fs::rename(&original, fixture.root.path()).unwrap();
    service
        .submit(CatalogCommand::Register {
            source_id: id.clone(),
            canonical_root: fixture.root.path().into(),
            lifecycle_generation: 2,
        })
        .unwrap();
    receive_until(&service, |e| {
        matches!(
            e,
            CatalogEvent::Registered {
                lifecycle_generation: 2,
                ..
            }
        )
    });
    service
        .submit(index_request(&id, 2, 2, None, None, 1))
        .unwrap();
    let fresh = receive_index_page(&service, 2);
    let denied = IndexInspectionCancellation::default();
    service
        .submit(CatalogCommand::RequestIndexInspection {
            handle,
            request_id: 2,
            cancellation: denied.clone(),
        })
        .unwrap();
    assert!(denied.is_canceled());
    wait_for_tag_read_payload(&service, 0);
    assert!(
        inspection_for(&service, fresh.inspection_handles[0].clone(), 3)
            .unwrap()
            .is_live()
    );
}
