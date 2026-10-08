// Original owned SQLite and synthetic immutable capture observations only.
#[cfg(test)]
mod recording_restart_funding {
    use super::super::recording_chunks::{
        PAYLOAD_COPIED_BYTES, PAYLOAD_READ_HOOK, PayloadReader, funded,
    };
    use super::*;
    use crate::operation_journal::{
        owner::JournalNamespaceFundingProbe,
        recording_commands::{
            RECORDING_VALUE_PARSE_ENTRIES, RecordingCapturedChunk, RecordingChunkRole,
            recording_proposal_for_test,
        },
    };

    #[test]
    fn funded_recording_restart_dense_pcm_and_historical_bytes_keep_original_custody() {
        let f = fixture();
        let store = OperationJournalStore::open(&f.owner).unwrap();
        let proposal = recording_proposal_for_test(2, 2048);
        store
            .insert_intent(&finite_recording_record(&proposal))
            .unwrap();
        let header = initial_recording_header(&proposal);
        store.retain_recording_chunk(&header).unwrap();
        let pcm = RecordingCapturedChunk::new(
            &proposal,
            Uuid::new_v4(),
            RecordingChunkRole::Pcm { ordinal: 0 },
            68,
            (0..16384).map(|n| n as u8).collect(),
        )
        .unwrap();
        store.retain_recording_chunk(&pcm).unwrap();
        let raw = String::from_utf8(pcm.encode(&proposal).unwrap()).unwrap();
        let historical = format!(" \n{}\t", raw.replace("\"bytes\"", "\"\\u0062ytes\""));
        store
            .connection
            .execute(
                "UPDATE operation_journal_recording_chunks SET record=?1 WHERE chunk_id=?2",
                params![historical.as_bytes(), pcm.chunk_id().to_string()],
            )
            .unwrap();
        let before = store.recording_retained_totals().unwrap();
        store
            .validate_recording_inventory_inside_transaction_with_input(None)
            .unwrap();
        let funding = JournalNamespaceFundingProbe::new();
        let baseline = funding.remaining_bytes();
        super::super::POINT_ROW_MATERIALIZED_BYTES.with(|n| n.set(0));
        PAYLOAD_COPIED_BYTES.with(|n| n.set(0));
        RECORDING_VALUE_PARSE_ENTRIES.with(|n| n.set(0));
        store
            .validate_recording_inventory_inside_transaction_with_input(Some(&funding.input))
            .unwrap();
        assert_eq!(
            super::super::POINT_ROW_MATERIALIZED_BYTES.with(|n| n.get()),
            0
        );
        assert_eq!(PAYLOAD_COPIED_BYTES.with(|n| n.get()), 0);
        assert_eq!(RECORDING_VALUE_PARSE_ENTRIES.with(|n| n.get()), 0);
        assert_eq!(funding.remaining_bytes(), baseline);
        let point = funded::read(&store, pcm.chunk_id(), &proposal, &funding.input)
            .unwrap()
            .unwrap();
        assert_eq!(point.1, pcm);
        let pool = funding.pool_bytes_observer_for_test();
        drop(funding);
        assert!(pool().0 < pool().1);
        drop(point);
        assert_eq!(pool().0, pool().1);
        assert_eq!(store.recording_retained_totals().unwrap(), before);
        let unchanged: Vec<u8> = store
            .connection
            .query_row(
                "SELECT record FROM operation_journal_recording_chunks WHERE chunk_id=?1",
                [pcm.chunk_id().to_string()],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(unchanged, historical.as_bytes());
    }

    #[test]
    fn funded_recording_restart_whole_page_utf8_precedes_uuid_interpretation() {
        let f = fixture();
        let store = OperationJournalStore::open(&f.owner).unwrap();
        let proposal = recording_proposal_for_test(2, 100);
        store
            .insert_intent(&finite_recording_record(&proposal))
            .unwrap();
        store
            .retain_recording_chunk(&initial_recording_header(&proposal))
            .unwrap();
        store
            .connection
            .execute(
                "UPDATE operation_journal_recording_chunks SET operation_id=?1 WHERE slot>=20000",
                ["!".repeat(36)],
            )
            .unwrap();
        let mut invalid = vec![b'z'; 36];
        invalid[35] = 0xff;
        store.connection.execute("UPDATE operation_journal_recording_chunks SET operation_id=CAST(?1 AS TEXT) WHERE slot=10000",[invalid]).unwrap();
        let image = || {
            store.connection.prepare("SELECT CAST(operation_id AS BLOB),record FROM operation_journal_recording_chunks ORDER BY slot").unwrap()
            .query_map([],|r|Ok((r.get::<_,Vec<u8>>(0)?,r.get::<_,Vec<u8>>(1)?))).unwrap().collect::<Result<Vec<_>,_>>().unwrap()
        };
        let before = image();
        let plain = store
            .validate_recording_inventory_inside_transaction_with_input(None)
            .unwrap_err()
            .to_string();
        assert!(
            plain.contains("UTF-8") || plain.contains("utf-8"),
            "{plain}"
        );
        let funding = JournalNamespaceFundingProbe::new();
        let pool = funding.pool_bytes_observer_for_test();
        let error = store
            .validate_recording_inventory_inside_transaction_with_input(Some(&funding.input))
            .unwrap_err();
        assert_eq!(error.to_string(), plain);
        let retained = funding.retain_command_error_for_test(error);
        drop(funding);
        assert!(pool().0 < pool().1);
        drop(retained);
        assert_eq!(pool().0, pool().1);
        assert_eq!(image(), before);
    }

    #[test]
    fn funded_recording_restart_preserves_prefix_and_duplicate_sequence_errors() {
        let f = fixture();
        let store = OperationJournalStore::open(&f.owner).unwrap();
        let proposal = recording_proposal_for_test(2, 100);
        store
            .insert_intent(&finite_recording_record(&proposal))
            .unwrap();
        let header = initial_recording_header(&proposal);
        store.retain_recording_chunk(&header).unwrap();
        let funding = JournalNamespaceFundingProbe::new();
        let original: Vec<u8> = store
            .connection
            .query_row(
                "SELECT record FROM operation_journal_recording_chunks WHERE slot=10000",
                [],
                |r| r.get(0),
            )
            .unwrap();
        let invalid = String::from_utf8(original.clone())
            .unwrap()
            .replace("\"offset\":0", "\"offset\":0,\"offset\":0");
        store
            .connection
            .execute(
                "UPDATE operation_journal_recording_chunks SET record=?1 WHERE slot=10000",
                [invalid.as_bytes()],
            )
            .unwrap();
        let plain = store
            .validate_recording_inventory_inside_transaction_with_input(None)
            .unwrap_err()
            .to_string();
        let error = store
            .validate_recording_inventory_inside_transaction_with_input(Some(&funding.input))
            .unwrap_err();
        assert_eq!(error.to_string(), plain);
        drop(funding.retain_command_error_for_test(error));
        store.connection.execute("UPDATE operation_journal_recording_chunks SET record=?1,sequence=(SELECT sequence FROM operation_journal LIMIT 1) WHERE slot=10000",[original]).unwrap();
        let before = store.recording_retained_totals().unwrap();
        let plain = store
            .validate_recording_inventory_inside_transaction_with_input(None)
            .unwrap_err()
            .to_string();
        assert!(plain.contains("duplicate journal-observation sequence"));
        let pool = funding.pool_bytes_observer_for_test();
        let error = store
            .validate_recording_inventory_inside_transaction_with_input(Some(&funding.input))
            .unwrap_err();
        assert_eq!(error.to_string(), plain);
        let retained = funding.retain_command_error_for_test(error);
        drop(funding);
        assert!(pool().0 < pool().1);
        drop(retained);
        assert_eq!(pool().0, pool().1);
        assert_eq!(store.recording_retained_totals().unwrap(), before);
    }

    #[test]
    fn funded_recording_restart_payload_race_preserves_original_type_and_bound_errors() {
        let f = fixture();
        let store = OperationJournalStore::open(&f.owner).unwrap();
        let proposal = recording_proposal_for_test(2, 100);
        store
            .insert_intent(&finite_recording_record(&proposal))
            .unwrap();
        let header = initial_recording_header(&proposal);
        store.retain_recording_chunk(&header).unwrap();
        let raw = header.encode(&proposal).unwrap();
        store
            .connection
            .execute_batch("PRAGMA ignore_check_constraints=ON")
            .unwrap();
        for oversized in [false, true] {
            let mut observed = None;
            for funded_read in [false, true] {
                store
                    .connection
                    .execute(
                        "UPDATE operation_journal_recording_chunks SET record=?1 WHERE slot=10000",
                        [&raw],
                    )
                    .unwrap();
                PAYLOAD_READ_HOOK.with(|hook|*hook.borrow_mut()=Some((PayloadReader::Chunk,Box::new(move|connection|{
                    if oversized { connection.execute("UPDATE operation_journal_recording_chunks SET record=?1 WHERE slot=10000",[vec![b'x';131073]]).unwrap(); }
                    else { connection.execute("UPDATE operation_journal_recording_chunks SET record=7 WHERE slot=10000",[]).unwrap(); }
                }))));
                PAYLOAD_COPIED_BYTES.with(|n| n.set(0));
                let error = if funded_read {
                    let funding = JournalNamespaceFundingProbe::new();
                    let error = funded::read(&store, header.chunk_id(), &proposal, &funding.input)
                        .err()
                        .unwrap();
                    let text = error.to_string();
                    drop(funding.retain_command_error_for_test(error));
                    text
                } else {
                    store
                        .read_recording_chunk_inside(header.chunk_id(), &proposal)
                        .err()
                        .unwrap()
                        .to_string()
                };
                assert_eq!(PAYLOAD_COPIED_BYTES.with(|n| n.get()), 0);
                if let Some(original) = &observed {
                    assert_eq!(&error, original);
                } else {
                    observed = Some(error);
                }
            }
        }
    }

    #[test]
    fn funded_recording_restart_pressure_precedes_owned_pages_and_chunk_bytes() {
        let f = fixture();
        let store = OperationJournalStore::open(&f.owner).unwrap();
        let proposal = recording_proposal_for_test(2, 100);
        store
            .insert_intent(&finite_recording_record(&proposal))
            .unwrap();
        let header = initial_recording_header(&proposal);
        store.retain_recording_chunk(&header).unwrap();
        let before = store.recording_retained_totals().unwrap();
        let funding = JournalNamespaceFundingProbe::new();
        let held = funding
            .input
            .reserve_historical_scratch(
                funding.remaining_bytes() - funded::working_bytes(&store).unwrap() - 1,
            )
            .unwrap();
        funded::PAGE_CELLS.with(|n| n.set(0));
        let error = store
            .validate_recording_inventory_inside_transaction_with_input(Some(&funding.input))
            .unwrap_err();
        assert!(matches!(
            error,
            OperationJournalStoreError::HistoricalClaimResourceBound
        ));
        assert_eq!(funded::PAGE_CELLS.with(|n| n.get()), 0);
        drop(funding.retain_command_error_for_test(error));
        PAYLOAD_COPIED_BYTES.with(|n| n.set(0));
        let error = funded::read(&store, header.chunk_id(), &proposal, &funding.input)
            .err()
            .unwrap();
        assert!(matches!(
            error,
            OperationJournalStoreError::HistoricalClaimResourceBound
        ));
        assert_eq!(PAYLOAD_COPIED_BYTES.with(|n| n.get()), 0);
        drop(funding.retain_command_error_for_test(error));
        drop(held);
        assert_eq!(store.recording_retained_totals().unwrap(), before);
    }
}
