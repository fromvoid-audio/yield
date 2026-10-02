// Reuse actual retained-owner setup and production feature publication, not a fake grant.
use super::*;
use crate::sample_sources::db::write::{
    SOURCE_LAYOUT_ALGORITHM_V1, SOURCE_LAYOUT_CANONICAL_MAX_BYTES, SourceLayoutComputedPoint,
    SourceLayoutComputedView, SourceLayoutGeneration, SourceLayoutGenerationError,
    SourcePreparedLayoutGeneration,
};
fn cpu(capture: &SourceLayoutCapture) -> [u8; 32] {
    let mut hash = blake3::Hasher::new();
    hash.update(b"yield.starmap.cpu-input.v1\0");
    hash.update(&(SOURCE_LAYOUT_ALGORITHM_V1.len() as u64).to_le_bytes());
    hash.update(SOURCE_LAYOUT_ALGORITHM_V1.as_bytes());
    hash.update(&(capture.features().len() as u64).to_le_bytes());
    for row in capture.features() {
        hash.update(&(row.sample_identity().len() as u64).to_le_bytes());
        hash.update(row.sample_identity().as_bytes());
        for value in row.payload().bytes().chunks_exact(4) {
            let value = f32::from_le_bytes(value.try_into().unwrap());
            hash.update(&value.to_le_bytes());
        }
    }
    *hash.finalize().as_bytes()
}
fn points(capture: &SourceLayoutCapture) -> Vec<SourceLayoutComputedPoint<'_>> {
    capture
        .features()
        .iter()
        .enumerate()
        .map(|(i, r)| SourceLayoutComputedPoint {
            sample_identity: r.sample_identity(),
            coordinates: [i as f32 + 0.25, -0.0],
            cluster_id: (capture.features().len() >= 5).then_some(i as i32),
        })
        .collect()
}
fn prepare(
    writer: &SourceWriter,
    capture: &SourceLayoutCapture,
) -> Result<SourcePreparedLayoutGeneration, SourceLayoutGenerationError> {
    writer.prepare_layout_generation(
        capture,
        &SourceLayoutComputedView {
            algorithm_version: SOURCE_LAYOUT_ALGORITHM_V1,
            feature_input_digest: &cpu(capture),
            points: &points(capture),
        },
        &AtomicBool::new(false),
        Instant::now() + Duration::from_secs(2),
    )
}
#[test]
fn layout_generation_real_owner_prepares_complete_canonical_readonly_observation() {
    let (_fixture, mut writer) = setup();
    publish(&mut writer, &["two.wav", "one.wav"]);
    let input = capture(&writer).unwrap();
    let before = persisted(&writer);
    let changes = writer.database_for_tests().connection.total_changes();
    writer
        .database_for_tests()
        .connection
        .pragma_update(None, "query_only", true)
        .unwrap();
    let prepared = prepare(&writer, &input).unwrap();
    assert!(prepared.bytes().len() <= SOURCE_LAYOUT_CANONICAL_MAX_BYTES);
    assert_eq!(
        prepared.bytes_digest(),
        blake3::hash(prepared.bytes()).as_bytes()
    );
    let decoded = SourceLayoutGeneration::decode(prepared.bytes()).unwrap();
    assert_eq!(decoded.input_digest(), input.input_digest());
    assert_eq!(decoded.feature_input_digest(), &cpu(&input));
    assert_eq!(decoded.source_id(), input.revision().source_id().as_str());
    assert_eq!(
        decoded.root_identity(),
        input.revision().root_identity().as_bytes()
    );
    assert_eq!(
        decoded.database_identity(),
        input.revision().database_identity()
    );
    assert_eq!(
        decoded.root_binding_identity(),
        input.revision().root_binding_identity()
    );
    assert_eq!(decoded.origin_revision(), input.revision().revision());
    assert_eq!(
        decoded.rows().iter().map(|r| r.path()).collect::<Vec<_>>(),
        ["one.wav", "two.wav"]
    );
    for (row, feature) in decoded.rows().iter().zip(input.features()) {
        assert_eq!(row.file_identity(), feature.file_identity());
        assert_eq!(row.content_hash(), feature.content_hash());
        assert_eq!(row.feature_reference(), feature.key().reference());
        assert_eq!(row.size(), feature.size());
        assert_eq!(row.modified_ns(), feature.modified_ns());
        assert_eq!(row.coordinates()[1].to_bits(), (-0.0f32).to_bits());
        assert_eq!(row.cluster_id(), None);
    }
    assert_eq!(prepared.bytes(), prepare(&writer, &input).unwrap().bytes());
    prepared.validate_read_fence().unwrap();
    writer
        .validate_prepared_layout_generation(
            &prepared,
            &AtomicBool::new(false),
            Instant::now() + Duration::from_secs(2),
        )
        .unwrap();
    assert_eq!(persisted(&writer), before);
    assert_eq!(
        writer.database_for_tests().connection.total_changes(),
        changes
    );
}
#[test]
fn layout_generation_rejects_wrong_cpu_version_coverage_order_and_invalid_points() {
    let (_fixture, mut writer) = setup();
    publish(&mut writer, &["one.wav", "two.wav"]);
    let input = capture(&writer).unwrap();
    let digest = cpu(&input);
    let before = persisted(&writer);
    for case in 0..9 {
        let mut result = points(&input);
        let mut hash = digest;
        let mut version = SOURCE_LAYOUT_ALGORITHM_V1;
        match case {
            0 => hash[0] ^= 1,
            1 => version = "features-v2",
            2 => {
                result.pop();
            }
            3 => result.swap(0, 1),
            4 => result[0].coordinates[0] = f32::NAN,
            5 => result[0].coordinates[1] = f32::INFINITY,
            6 => result[0].cluster_id = Some(0),
            7 => result[0].cluster_id = Some(-1),
            8 => result[0].sample_identity = "other::one.wav",
            _ => unreachable!(),
        }
        assert!(
            matches!(
                writer.prepare_layout_generation(
                    &input,
                    &SourceLayoutComputedView {
                        algorithm_version: version,
                        feature_input_digest: &hash,
                        points: &result
                    },
                    &AtomicBool::new(false),
                    Instant::now() + Duration::from_secs(2)
                ),
                Err(SourceLayoutGenerationError::Invalid(_))
            ),
            "case {case}"
        );
    }
    assert_eq!(persisted(&writer), before);
}
#[test]
fn layout_generation_cancel_deadline_and_caller_transaction_reject_without_mutation() {
    let (_fixture, mut writer) = setup();
    publish(&mut writer, &["one.wav", "two.wav"]);
    let input = capture(&writer).unwrap();
    let digest = cpu(&input);
    let result = points(&input);
    let view = SourceLayoutComputedView {
        algorithm_version: SOURCE_LAYOUT_ALGORITHM_V1,
        feature_input_digest: &digest,
        points: &result,
    };
    let before = persisted(&writer);
    assert!(matches!(
        writer.prepare_layout_generation(
            &input,
            &view,
            &AtomicBool::new(true),
            Instant::now() + Duration::from_secs(2)
        ),
        Err(SourceLayoutGenerationError::Cancelled)
    ));
    assert!(matches!(
        writer.prepare_layout_generation(&input, &view, &AtomicBool::new(false), Instant::now()),
        Err(SourceLayoutGenerationError::Deadline)
    ));
    writer
        .database_for_tests()
        .connection
        .execute_batch("BEGIN IMMEDIATE")
        .unwrap();
    assert!(matches!(
        prepare(&writer, &input),
        Err(SourceLayoutGenerationError::Capture(
            SourceLayoutCaptureError::TransactionActive
        ))
    ));
    writer
        .database_for_tests()
        .connection
        .execute_batch("ROLLBACK")
        .unwrap();
    assert_eq!(persisted(&writer), before);
}
#[test]
fn layout_generation_metadata_commit_and_restart_keep_content_identity_but_expire_issuer() {
    let (fixture, mut writer) = setup();
    publish(&mut writer, &["one.wav", "two.wav"]);
    let input = capture(&writer).unwrap();
    let old = prepare(&writer, &input).unwrap();
    let archived = old.bytes().to_vec();
    writer
        .commit_catalog_curation_command(
            input.revision(),
            &fixture.profile_guard,
            &[SourceCurationUpdate::SetRating {
                path: CurationFixture::path("one.wav"),
                rating: Rating::KEEP_3,
            }],
        )
        .unwrap();
    assert!(old.validate_read_fence().is_err());
    assert!(prepare(&writer, &input).is_err());
    let fresh_input = capture(&writer).unwrap();
    let fresh = prepare(&writer, &fresh_input).unwrap();
    assert_eq!(old.input_digest(), fresh.input_digest());
    assert_eq!(old.point_digest(), fresh.point_digest());
    let historical = SourceLayoutGeneration::decode(&archived).unwrap();
    assert_ne!(historical.origin_revision(), fresh.revision().revision());
    assert_eq!(historical.input_digest(), fresh.input_digest());
    writer.close().unwrap();
    assert!(fresh.validate_read_fence().is_err());
    let database = SourceDatabase::open_for_catalog_with_profile_guard(
        fixture._source.path(),
        &fixture.profile_guard,
        Path::new("curation-profile-bound"),
    )
    .unwrap();
    let new = SourceWriter::new(database, SourceId::from_string("layout-features"), 8).unwrap();
    let before = persisted(&new);
    assert!(matches!(
        new.validate_prepared_layout_generation(
            &fresh,
            &AtomicBool::new(false),
            Instant::now() + Duration::from_secs(2)
        ),
        Err(SourceLayoutGenerationError::Capture(
            SourceLayoutCaptureError::Stale
        ))
    ));
    let current = capture(&new).unwrap();
    let next = prepare(&new, &current).unwrap();
    assert_eq!(fresh.bytes(), next.bytes());
    assert_eq!(fresh.point_digest(), next.point_digest());
    assert_eq!(persisted(&new), before);
}
#[test]
fn layout_generation_changed_membership_and_expired_lease_preserve_evidence() {
    let (_fixture, mut writer) = setup();
    publish(&mut writer, &["one.wav", "two.wav"]);
    let input = capture(&writer).unwrap();
    let prepared = prepare(&writer, &input).unwrap();
    writer
        .database_for_tests()
        .connection
        .execute(
            "UPDATE wav_files SET file_size=file_size+1 WHERE path='one.wav'",
            [],
        )
        .unwrap();
    let before = persisted(&writer);
    assert!(prepare(&writer, &input).is_err());
    assert!(
        writer
            .validate_prepared_layout_generation(
                &prepared,
                &AtomicBool::new(false),
                Instant::now() + Duration::from_secs(2)
            )
            .is_err()
    );
    assert_eq!(persisted(&writer), before);
    writer
        .database_for_tests()
        .connection
        .execute(
            "UPDATE source_lease SET expires_at=acquired_at WHERE singleton=1",
            [],
        )
        .unwrap();
    let before = persisted(&writer);
    assert!(prepare(&writer, &input).is_err());
    assert_eq!(persisted(&writer), before);
    SourceLayoutGeneration::decode(prepared.bytes()).unwrap(); // Historical decode is still passive.
}
#[test]
fn layout_generation_strict_binary_decode_rejects_truncation_unknown_and_corrupt_records() {
    let (_fixture, mut writer) = setup();
    publish(&mut writer, &["one.wav", "two.wav"]);
    let input = capture(&writer).unwrap();
    let prepared = prepare(&writer, &input).unwrap();
    let bytes = prepared.bytes();
    for length in 0..bytes.len() {
        assert!(
            SourceLayoutGeneration::decode(&bytes[..length]).is_err(),
            "length {length}"
        );
    }
    for extra in [b"\0".as_slice(), b"duplicate", b"unknown"] {
        let mut bad = bytes.to_vec();
        bad.extend_from_slice(extra);
        assert!(SourceLayoutGeneration::decode(&bad).is_err());
    }
    let mut bad = bytes.to_vec();
    bad[4] = 2;
    assert!(SourceLayoutGeneration::decode(&bad).is_err());
    let mut bad = bytes.to_vec();
    bad[8..12].copy_from_slice(&u32::MAX.to_le_bytes());
    assert!(matches!(
        SourceLayoutGeneration::decode(&bad),
        Err(SourceLayoutGenerationError::Limit(_))
    ));
    for needle in [
        b"one.wav".as_slice(),
        input.features()[0].content_hash().as_bytes(),
        input.features()[0].key().reference().as_bytes(),
    ] {
        let at = bytes
            .windows(needle.len())
            .position(|v| v == needle)
            .unwrap();
        let mut bad = bytes.to_vec();
        bad[at] ^= 1;
        assert!(SourceLayoutGeneration::decode(&bad).is_err());
    }
    let mut cursor = 8usize;
    for _ in 0..4 {
        let length = u32::from_le_bytes(bytes[cursor..cursor + 4].try_into().unwrap()) as usize;
        cursor += 4 + length;
    }
    cursor += 8;
    for _ in 0..2 {
        let length = u32::from_le_bytes(bytes[cursor..cursor + 4].try_into().unwrap()) as usize;
        cursor += 4 + length;
    }
    cursor += 96;
    let mut bad = bytes.to_vec();
    bad[cursor..cursor + 4].copy_from_slice(&257u32.to_le_bytes());
    assert!(matches!(
        SourceLayoutGeneration::decode(&bad),
        Err(SourceLayoutGenerationError::Limit("complete row count"))
    ));
    let mut bad = bytes.to_vec();
    let end = bad.len() - 32;
    bad[end - 9..end - 5].copy_from_slice(&f32::NAN.to_bits().to_le_bytes());
    assert!(matches!(
        SourceLayoutGeneration::decode(&bad),
        Err(SourceLayoutGenerationError::Invalid("finite coordinates"))
    ));
    let mut bad = bytes.to_vec();
    bad[end - 1] = 2;
    assert!(matches!(
        SourceLayoutGeneration::decode(&bad),
        Err(SourceLayoutGenerationError::Invalid("cluster tag"))
    ));
    let mut bad = bytes.to_vec();
    bad[12] = 255;
    assert!(matches!(
        SourceLayoutGeneration::decode(&bad),
        Err(SourceLayoutGenerationError::Invalid("UTF8 field"))
    ));
    let huge = vec![0; SOURCE_LAYOUT_CANONICAL_MAX_BYTES + 1];
    assert!(matches!(
        SourceLayoutGeneration::decode(&huge),
        Err(SourceLayoutGenerationError::Limit("encoded bytes"))
    ));
}
#[test]
fn layout_generation_max256_actual_features_encode_and_validate_bounded_clusters() {
    let (_fixture, writer) = setup_max_manifest();
    let input = capture(&writer).unwrap();
    let before = persisted(&writer);
    let started = Instant::now();
    let prepared = prepare(&writer, &input).unwrap();
    let decoded = SourceLayoutGeneration::decode(prepared.bytes()).unwrap();
    assert_eq!(decoded.rows().len(), 256);
    assert_eq!(decoded.rows()[255].cluster_id(), Some(255));
    assert_eq!(decoded.input_digest(), input.input_digest());
    assert!(prepared.bytes().len() <= SOURCE_LAYOUT_CANONICAL_MAX_BYTES);
    assert_eq!(persisted(&writer), before);
    eprintln!(
        "actual 256-row canonical preparation: {:?}, encoded {} bytes",
        started.elapsed(),
        prepared.bytes().len()
    );
    let digest = cpu(&input);
    let mut result = points(&input);
    result[0].cluster_id = None;
    assert!(matches!(
        writer.prepare_layout_generation(
            &input,
            &SourceLayoutComputedView {
                algorithm_version: SOURCE_LAYOUT_ALGORITHM_V1,
                feature_input_digest: &digest,
                points: &result
            },
            &AtomicBool::new(false),
            Instant::now() + Duration::from_secs(2)
        ),
        Err(SourceLayoutGenerationError::Invalid("cluster policy"))
    ));
    result[0].cluster_id = Some(-1);
    assert!(prepare(&writer, &input).is_ok()); // A rejected CPU observation doesn't poison the owner.
}
#[test]
fn layout_generation_foreign_owner_and_near_expiry_do_not_reacquire_or_renew() {
    let (_first_fixture, mut writer) = setup();
    publish(&mut writer, &["one.wav", "two.wav"]);
    let input = capture(&writer).unwrap();
    let prepared = prepare(&writer, &input).unwrap();
    let (_other_fixture, mut other) = setup();
    publish(&mut other, &["one.wav", "two.wav"]);
    let before = persisted(&other);
    assert!(matches!(
        prepare(&other, &input),
        Err(SourceLayoutGenerationError::Capture(
            SourceLayoutCaptureError::Stale
        ))
    ));
    assert!(
        other
            .validate_prepared_layout_generation(
                &prepared,
                &AtomicBool::new(false),
                Instant::now() + Duration::from_secs(2)
            )
            .is_err()
    );
    assert_eq!(persisted(&other), before);
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    writer
        .database_for_tests()
        .connection
        .execute(
            "UPDATE source_lease SET expires_at=?1 WHERE singleton=1",
            [now + 30],
        )
        .unwrap();
    let before = persisted(&writer);
    let current = capture(&writer).unwrap();
    prepare(&writer, &current).unwrap();
    assert_eq!(persisted(&writer), before);
}
#[cfg(unix)]
#[test]
fn layout_generation_replaced_root_rejects_without_overwriting_historical_bytes() {
    struct Restore {
        original: PathBuf,
        moved: PathBuf,
    }
    impl Drop for Restore {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir(&self.original);
            let _ = std::fs::rename(&self.moved, &self.original);
        }
    }
    let (fixture, mut writer) = setup();
    publish(&mut writer, &["one.wav", "two.wav"]);
    let input = capture(&writer).unwrap();
    let prepared = prepare(&writer, &input).unwrap();
    let original = fixture._source.path().to_path_buf();
    let moved = original.with_extension("generation-original");
    std::fs::rename(&original, &moved).unwrap();
    let _restore = Restore {
        original: original.clone(),
        moved,
    };
    std::fs::create_dir(&original).unwrap();
    let before = persisted(&writer);
    assert!(prepare(&writer, &input).is_err());
    assert!(prepared.validate_read_fence().is_err());
    assert_eq!(persisted(&writer), before);
    SourceLayoutGeneration::decode(prepared.bytes()).unwrap();
}
#[test]
fn layout_generation_actual_long_quoted_unicode_paths_roundtrip_without_escape_aliasing() {
    let long = format!(
        "{}/{}/{}/quote-\"-\n-\t-é.wav",
        "a".repeat(250),
        "b".repeat(250),
        "c".repeat(250)
    );
    let names = [long.as_str(), "one.wav", "two.wav"];
    let (_fixture, mut writer) = setup_named(&names);
    publish(&mut writer, &names);
    let input = capture(&writer).unwrap();
    let before = persisted(&writer);
    let prepared = prepare(&writer, &input).unwrap();
    let decoded = SourceLayoutGeneration::decode(prepared.bytes()).unwrap();
    assert_eq!(decoded.rows()[0].path(), long);
    assert_eq!(decoded.input_digest(), input.input_digest());
    assert_eq!(prepare(&writer, &input).unwrap().bytes(), prepared.bytes());
    assert_eq!(persisted(&writer), before);
    let mut bad = prepared.bytes().to_vec();
    // Explicit binary length checks reject >1024 before borrowing any oversized path.
    let at = bad
        .windows(long.len())
        .position(|v| v == long.as_bytes())
        .unwrap();
    bad[at - 4..at].copy_from_slice(&1025u32.to_le_bytes());
    assert!(matches!(
        SourceLayoutGeneration::decode(&bad),
        Err(SourceLayoutGenerationError::Limit("field bytes"))
    ));
}
#[test]
fn layout_generation_historical_origin_corruption_cannot_hide_outside_content_digests() {
    let (_fixture, mut writer) = setup();
    publish(&mut writer, &["one.wav", "two.wav"]);
    let input = capture(&writer).unwrap();
    let prepared = prepare(&writer, &input).unwrap();
    let mut corrupt = prepared.bytes().to_vec();
    let mut cursor = 8usize;
    for _ in 0..4 {
        let length = u32::from_le_bytes(corrupt[cursor..cursor + 4].try_into().unwrap()) as usize;
        cursor += 4 + length;
    }
    corrupt[cursor] ^= 1; // Another valid integer, with unchanged manifest and point digests.
    assert!(
        SourceLayoutGeneration::decode(&corrupt).is_err(),
        "historical origin corruption must fail closed"
    );
}

// Exact canonical producer bytes in a test-owned row are allocation evidence only;
// inserting them here does not exercise or complete admitted production publication.
pub(super) fn canonical_blob_budget() {
    let (_fixture, mut writer) = setup();
    publish(&mut writer, &["one.wav", "two.wav"]);
    let input = capture(&writer).unwrap();
    let prepared = prepare(&writer, &input).unwrap();
    assert!(prepared.bytes().contains(&0));
    writer
        .database_for_tests()
        .connection
        .execute(
            "INSERT INTO metadata(key,value) VALUES ('source_starmap_generation_v1',?1)",
            [prepared.bytes()],
        )
        .unwrap();
    let before = source_writer_state_snapshot(writer.database_for_tests());
    let changes = writer.database_for_tests().connection.total_changes();
    let control = writer
        .database_for_tests()
        .source_lease_control_total_changes_for_tests()
        .unwrap()
        .unwrap();
    let rev = writer.current_revision().unwrap();
    let budget = writer.capture_layout_metadata_budget(&rev).unwrap();
    assert_eq!(
        budget.existing_generation_bytes(),
        prepared.bytes().len() as u64
    );
    assert_eq!(
        budget.existing_generation_digest(),
        Some(*prepared.bytes_digest())
    );
    let held = writer
        .prepare_layout_wal_reservation(budget, Instant::now() + Duration::from_secs(2))
        .unwrap();
    held.revalidate().unwrap();
    let writer = held.abort().unwrap();
    assert_eq!(
        writer.database_for_tests().connection.total_changes(),
        changes
    );
    assert_eq!(
        source_writer_state_snapshot(writer.database_for_tests()),
        before
    );
    assert_eq!(
        writer
            .database_for_tests()
            .source_lease_control_total_changes_for_tests()
            .unwrap()
            .unwrap(),
        control
    );
    assert_eq!(writer.current_revision().unwrap(), rev);
}
