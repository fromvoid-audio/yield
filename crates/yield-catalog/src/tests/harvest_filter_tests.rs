// Genuine retained owners and audited WAVs. SQL seeds are Harvest metadata, not physical copying.
fn filter_request(
    global: &GlobalLibraryWriter,
    snapshot: &CatalogSnapshot,
    id: u64,
    choice: HarvestFilterChoice,
) -> SourceHarvestFilterRequest {
    SourceHarvestFilterRequest::new(
        global.acquisition_id(),
        snapshot.source_id.clone(),
        snapshot.lifecycle_generation,
        snapshot.root_identity.clone(),
        snapshot.directory_generation,
        snapshot.committed_revision,
        id,
        snapshot.manifest.len(),
        choice,
    )
    .unwrap()
}
fn filter_delivery(service: &CatalogService, id: u64) -> CatalogDelivery {
    let deadline = Instant::now() + Duration::from_secs(8);
    loop {
        if let Ok(delivery) = service.recv_delivery_timeout(Duration::from_millis(10)) {
            if matches!(&delivery.event, CatalogEvent::HarvestFilterCompleted { request, .. } if request.context().request_id() == id)
            {
                return delivery;
            }
        }
        assert!(Instant::now() < deadline, "missing filter completion {id}");
    }
}
fn filter_mask(
    service: &CatalogService,
    request: SourceHarvestFilterRequest,
) -> CatalogHarvestFilter {
    let id = request.context().request_id();
    service
        .submit(CatalogCommand::RequestHarvestFilter {
            request,
            cancellation: HarvestFactsCancellation::new(),
        })
        .unwrap();
    let (event, fence) = filter_delivery(service, id)
        .into_parts_if_live()
        .expect("actual live mask");
    assert!(fence.unwrap().is_live());
    let CatalogEvent::HarvestFilterCompleted {
        result: Ok(mask), ..
    } = event
    else {
        panic!("{event:?}")
    };
    mask
}
#[test]
fn harvest_filter_actual_owner_exact_nine_reference_choices_and_read_only_sql() {
    let (fixture, mut global, service, snapshot) = setup();
    let sql = seed(&fixture, &snapshot);
    sql.execute("INSERT INTO harvest_derivations(parent_source_id,parent_relative_path,parent_file_size,parent_modified_ns,parent_content_hash,child_source_id,child_relative_path,child_file_size,child_modified_ns,child_content_hash,operation,tool_version,created_at) SELECT source_id,relative_path,file_size,modified_ns,content_hash,'historical-child','child.wav',file_size,modified_ns,content_hash,'copy','fixture-v1',1 FROM harvest_files WHERE relative_path='one.wav'", []).unwrap();
    let before: i64 = sql
        .query_row("PRAGMA data_version", [], |r| r.get(0))
        .unwrap();
    let expected: [&[&str]; 8] = [
        &["one.wav", "two.wav", "untracked.wav"],
        &["one.wav", "two.wav", "three.wav", "untracked.wav"],
        &["two.wav", "three.wav", "untracked.wav"],
        &["three.wav"],
        &["one.wav"],
        &[
            "two.wav",
            "three.wav",
            "four.wav",
            "five.wav",
            "untracked.wav",
        ],
        &["four.wav"],
        &["five.wav"],
    ];
    for (i, (choice, paths)) in HarvestFilterChoice::ALL[..8]
        .iter()
        .zip(expected)
        .enumerate()
    {
        let mask = filter_mask(
            &service,
            filter_request(&global, &snapshot, i as u64 + 1, *choice),
        );
        assert!(mask.is_live());
        let words = mask.words();
        for (index, entry) in snapshot.manifest.iter().enumerate() {
            assert_eq!(
                words[index / 64] & (1 << (index % 64)) != 0,
                paths.iter().any(|p| entry.relative_path == Path::new(p)),
                "{choice:?} {:?}",
                entry.relative_path
            );
        }
        assert_eq!(words[0] >> snapshot.manifest.len(), 0);
    }
    assert!(
        HarvestFilterChoice::ALL
            .iter()
            .last()
            .unwrap()
            .includes(HarvestState::Ignored, usize::MAX)
    );
    assert_eq!(
        before,
        sql.query_row("PRAGMA data_version", [], |r| r.get::<_, i64>(0))
            .unwrap()
    );
    service.shutdown();
    global.shutdown();
}
#[test]
fn harvest_filter_actual_multiple_chunks_and_global_invalidation() {
    let fixture = Fixture::new("catalog-harvest-mask-chunks");
    for i in 0..140 {
        write_test_wav(&fixture.root.path().join(format!("{i:04}.wav")), 16_000, 20);
    }
    let mut global = GlobalLibraryWriter::start(&fixture.guard).unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while global.status() != GlobalLibraryWriterStatus::Available {
        assert!(Instant::now() < deadline);
        thread::yield_now();
    }
    let service = CatalogService::start_with_profile_owners(
        &fixture.guard,
        None,
        global.harvest_facts_client(),
    )
    .unwrap();
    let snapshot = register_and_audit(
        &service,
        fixture.root.path(),
        &SourceId::from_string("chunks"),
    );
    let request = filter_request(&global, &snapshot, 101, HarvestFilterChoice::New);
    let mask = filter_mask(&service, request.clone());
    assert_eq!(&*mask.words(), &[u64::MAX, u64::MAX, (1 << 12) - 1]);
    authored_secondary(&fixture, &global);
    assert!(!mask.is_live());
    let mask = filter_mask(&service, request);
    assert!(mask.is_live());
    service.shutdown();
    assert!(!mask.is_live());
    global.shutdown();
}
#[test]
fn harvest_filter_actual_stale_revision_and_missing_owner_fail_closed() {
    let (fixture, mut global, service, mut snapshot) = setup();
    snapshot.committed_revision += 1;
    service
        .submit(CatalogCommand::RequestHarvestFilter {
            request: filter_request(&global, &snapshot, 11, HarvestFilterChoice::New),
            cancellation: HarvestFactsCancellation::new(),
        })
        .unwrap();
    assert!(matches!(
        filter_delivery(&service, 11).event,
        CatalogEvent::HarvestFilterCompleted {
            result: Err(HarvestFactsFailure::StaleSource),
            ..
        }
    ));
    snapshot.committed_revision -= 1;
    global.shutdown();
    service
        .submit(CatalogCommand::RequestHarvestFilter {
            request: filter_request(&global, &snapshot, 12, HarvestFilterChoice::New),
            cancellation: HarvestFactsCancellation::new(),
        })
        .unwrap();
    assert!(matches!(
        filter_delivery(&service, 12).event,
        CatalogEvent::HarvestFilterCompleted {
            result: Err(HarvestFactsFailure::OwnerUnavailable),
            ..
        }
    ));
    service.shutdown();
    drop(fixture);
}
