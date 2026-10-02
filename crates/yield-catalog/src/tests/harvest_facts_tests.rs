//! Actual catalog/source/global SQLite fixtures; graph seeds are metadata, not physical effects.
#[cfg(test)]
mod fixtures {
    use super::super::*;
    use yield_library::sample_sources::library::{GlobalLibraryWriter, GlobalLibraryWriterStatus};
    fn setup() -> (
        Fixture,
        GlobalLibraryWriter,
        CatalogService,
        CatalogSnapshot,
    ) {
        let fixture = Fixture::new("catalog-harvest-facts");
        for path in [
            "one.wav",
            "two.wav",
            "three.wav",
            "four.wav",
            "five.wav",
            "untracked.wav",
        ] {
            write_test_wav(&fixture.root.path().join(path), 16_000, 20);
        }
        let global = GlobalLibraryWriter::start(&fixture.guard).unwrap();
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
        let source = SourceId::from_string("catalog-harvest-facts");
        let snapshot = register_and_audit(&service, fixture.root.path(), &source);
        (fixture, global, service, snapshot)
    }
    fn request(
        global: &GlobalLibraryWriter,
        snapshot: &CatalogSnapshot,
        id: u64,
        paths: &[&str],
    ) -> SourceHarvestFactsRequest {
        SourceHarvestFactsRequest::new(
            global.acquisition_id(),
            snapshot.source_id.clone(),
            snapshot.lifecycle_generation,
            snapshot.root_identity.clone(),
            snapshot.directory_generation,
            snapshot.committed_revision,
            id,
            &paths.iter().map(|p| p.to_string()).collect::<Vec<_>>(),
        )
        .unwrap()
    }
    fn submit(
        service: &CatalogService,
        request: SourceHarvestFactsRequest,
        cancellation: HarvestFactsCancellation,
    ) {
        service
            .submit(CatalogCommand::RequestHarvestFacts {
                request,
                cancellation,
            })
            .unwrap();
    }
    fn delivery(service: &CatalogService, id: u64) -> CatalogDelivery {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Ok(delivery) = service.recv_delivery_timeout(Duration::from_millis(20)) {
                if matches!(&delivery.event, CatalogEvent::HarvestFactsCompleted { request, .. } if request.request_id() == id)
                {
                    return delivery;
                }
            }
            assert!(Instant::now() < deadline, "missing Harvest completion {id}");
        }
    }
    fn facts(delivery: CatalogDelivery) -> CatalogHarvestFacts {
        let (event, fence) = delivery.into_parts_if_live().expect("live delivery");
        assert!(fence.unwrap().is_live());
        match event {
            CatalogEvent::HarvestFactsCompleted {
                result: Ok(facts), ..
            } => facts,
            event => panic!("{event:?}"),
        }
    }
    fn seed(fixture: &Fixture, snapshot: &CatalogSnapshot) -> rusqlite::Connection {
        let source = rusqlite::Connection::open_with_flags(
            fixture
                .guard
                .profile_root()
                .join(catalog_profile_child(&snapshot.source_id))
                .join(".yield.db"),
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )
        .unwrap();
        let global =
            rusqlite::Connection::open(fixture.guard.profile_root().join("library.db")).unwrap();
        for (path, state) in ["one.wav", "two.wav", "three.wav", "four.wav", "five.wav"]
            .into_iter()
            .zip(["new", "seen", "touched", "done", "ignored"])
        {
            let (size, modified, hash): (i64, i64, String) = source
                .query_row(
                    "SELECT file_size,modified_ns,content_hash FROM wav_files WHERE path=?1",
                    [path],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
                )
                .unwrap();
            global.execute("INSERT INTO harvest_files(source_id,relative_path,file_size,modified_ns,content_hash,harvest_state,discovered_at) VALUES (?1,?2,?3,?4,?5,?6,1)",
                rusqlite::params![snapshot.source_id.as_str(),path,size,modified,hash,state]).unwrap();
        }
        global
    }
    #[test]
    fn harvest_catalog_actual_states_absence_and_exact_derivatives_are_read_only() {
        let (fixture, mut global, service, snapshot) = setup();
        let sql = seed(&fixture, &snapshot);
        sql.execute("INSERT INTO harvest_derivations(parent_source_id,parent_relative_path,parent_file_size,parent_modified_ns,parent_content_hash,child_source_id,child_relative_path,child_file_size,child_modified_ns,child_content_hash,operation,tool_version,created_at) SELECT source_id,relative_path,file_size,modified_ns,content_hash,'historical-child',?1,file_size,modified_ns,content_hash,'copy','fixture-v1',1 FROM harvest_files WHERE relative_path='one.wav'", ["child-a.wav"]).unwrap();
        sql.execute("INSERT INTO harvest_derivations(parent_source_id,parent_relative_path,parent_file_size,parent_modified_ns,parent_content_hash,child_source_id,child_relative_path,child_file_size,child_modified_ns,child_content_hash,operation,tool_version,created_at) SELECT source_id,relative_path,file_size,modified_ns,content_hash,'historical-child',?1,file_size,modified_ns,content_hash,'copy','fixture-v1',1 FROM harvest_files WHERE relative_path='one.wav'", ["child-b.wav"]).unwrap();
        let before: i64 = sql
            .query_row("PRAGMA data_version", [], |r| r.get(0))
            .unwrap();
        let source_path = fixture
            .guard
            .profile_root()
            .join(catalog_profile_child(&snapshot.source_id));
        let source_bytes: Vec<_> = [".yield.db", ".yield.db-wal"]
            .into_iter()
            .filter_map(|name| {
                fs::read(source_path.join(name))
                    .ok()
                    .map(|bytes| (name, bytes))
            })
            .collect();
        let query = request(
            &global,
            &snapshot,
            1,
            &[
                "one.wav",
                "two.wav",
                "three.wav",
                "four.wav",
                "five.wav",
                "untracked.wav",
            ],
        );
        submit(&service, query.clone(), HarvestFactsCancellation::new());
        let facts = facts(delivery(&service, 1));
        assert_eq!(facts.request(), &query);
        assert_eq!(
            facts
                .entries()
                .iter()
                .map(|e| e.state())
                .collect::<Vec<_>>(),
            vec![
                HarvestState::New,
                HarvestState::Seen,
                HarvestState::Touched,
                HarvestState::Done,
                HarvestState::Ignored,
                HarvestState::New
            ]
        );
        assert_eq!(facts.entries()[0].immediate_derivative_count(), 2);
        assert!(
            facts.entries()[..5]
                .iter()
                .all(HarvestFactsEntry::is_tracked)
        );
        assert!(!facts.entries()[5].is_tracked());
        assert!(facts.matches_projection(
            &global.acquisition_id(),
            &snapshot.source_id,
            1,
            &snapshot.root_identity,
            snapshot.directory_generation,
            snapshot.committed_revision
        ));
        assert_eq!(
            sql.query_row::<i64, _, _>("PRAGMA data_version", [], |r| r.get(0))
                .unwrap(),
            before
        );
        for (name, bytes) in source_bytes {
            assert_eq!(fs::read(source_path.join(name)).unwrap(), bytes);
        }
        wait_for_tag_read_payload(&service, 0);
        wait_for_tag_page_input_release(&service);
        service.shutdown();
        assert!(!facts.is_live());
        global.shutdown();
    }
    #[test]
    fn harvest_catalog_stale_context_missing_members_and_unavailable_owner_fail_closed() {
        let (fixture, mut global, service, snapshot) = setup();
        let mut stale = snapshot.clone();
        stale.directory_generation += 1;
        let query = request(&global, &stale, 2, &["one.wav"]);
        submit(&service, query, HarvestFactsCancellation::new());
        assert!(matches!(
            delivery(&service, 2).into_event_if_live().unwrap(),
            CatalogEvent::HarvestFactsCompleted {
                result: Err(HarvestFactsFailure::StaleSource),
                ..
            }
        ));
        submit(
            &service,
            request(&global, &snapshot, 3, &["missing.wav"]),
            HarvestFactsCancellation::new(),
        );
        assert!(matches!(
            delivery(&service, 3).into_event_if_live().unwrap(),
            CatalogEvent::HarvestFactsCompleted {
                result: Err(HarvestFactsFailure::IdentityUnavailable),
                ..
            }
        ));
        global.shutdown();
        submit(
            &service,
            request(&global, &snapshot, 4, &["one.wav"]),
            HarvestFactsCancellation::new(),
        );
        assert!(matches!(
            delivery(&service, 4).into_event_if_live().unwrap(),
            CatalogEvent::HarvestFactsCompleted {
                result: Err(HarvestFactsFailure::OwnerUnavailable),
                ..
            }
        ));
        service.shutdown();
        drop(fixture);
    }
    #[test]
    fn harvest_catalog_cancellation_and_supersession_discard_queued_and_cached_results() {
        let (_fixture, mut global, service, snapshot) = setup();
        let cancellation = HarvestFactsCancellation::new();
        submit(
            &service,
            request(&global, &snapshot, 5, &["one.wav"]),
            cancellation.clone(),
        );
        let queued = delivery(&service, 5);
        let cached_cancel = match &queued.event {
            CatalogEvent::HarvestFactsCompleted {
                result: Ok(facts), ..
            } => facts.clone(),
            event => panic!("{event:?}"),
        };
        cancellation.cancel();
        assert!(!cached_cancel.is_live());
        assert!(queued.into_event_if_live().is_none());
        submit(
            &service,
            request(&global, &snapshot, 6, &["one.wav"]),
            HarvestFactsCancellation::new(),
        );
        let cached = facts(delivery(&service, 6));
        submit(
            &service,
            request(&global, &snapshot, 7, &["two.wav"]),
            HarvestFactsCancellation::new(),
        );
        let newer = facts(delivery(&service, 7));
        assert!(!cached.is_live());
        assert!(newer.is_live());
        service.shutdown();
        global.shutdown();
    }
    #[test]
    fn harvest_catalog_input_rejects_empty_duplicates_aliases_and_over_budget_paths() {
        let (_fixture, mut global, service, snapshot) = setup();
        let build = |paths: &[String]| {
            SourceHarvestFactsRequest::new(
                global.acquisition_id(),
                snapshot.source_id.clone(),
                1,
                snapshot.root_identity.clone(),
                snapshot.directory_generation,
                snapshot.committed_revision,
                8,
                paths,
            )
        };
        for paths in [
            vec![],
            vec!["one.wav".into(), "one.wav".into()],
            vec!["../one.wav".into()],
            vec!["one\\two.wav".into()],
            vec!["a".repeat(HARVEST_FACTS_MAX_PATH_BYTES + 1)],
            (0..129).map(|i| format!("{i}.wav")).collect(),
        ] {
            assert_eq!(
                build(&paths).unwrap_err(),
                HarvestFactsFailure::InvalidInput
            );
        }
        service.shutdown();
        global.shutdown();
    }
    include!("harvest_facts_tests/invalidation.rs");
}
