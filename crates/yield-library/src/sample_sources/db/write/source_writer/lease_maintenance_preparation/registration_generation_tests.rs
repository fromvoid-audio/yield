//! Original Source registration generations remain separate from lease/lifecycle observations.
use super::*;

#[test]
fn preaudit_original_generation_is_exact_and_foreign_generation_refuses_without_effects() {
    let (fixture, writer) = fixture();
    let other_db = SourceDatabase::open_for_catalog_with_profile_guard(
        &fixture.media,
        &fixture.profile,
        Path::new("other-original-generation"),
    )
    .unwrap();
    let other_writer = SourceWriter::new_for_scan(
        other_db,
        SourceId::from_string("other-original-generation"),
        1,
    )
    .unwrap();
    let original_settings = settings(&writer);
    let other_settings = settings(&other_writer);
    let external = external(&writer);
    let original_lease = lease(&external);
    let original_changes = writer.database.connection.total_changes();
    let (authority, _proof, request) = registered(&writer.database);
    let (other_authority, _other_proof, other_request) = registered(&other_writer.database);
    let original_generation = request.registration_generation();
    assert_ne!(request.registration_id(), other_request.registration_id());
    assert_ne!(original_generation, other_request.registration_generation());
    assert_eq!(
        request.lease_identity().epoch(),
        other_request.lease_identity().epoch()
    );
    let mut held = prepare(writer);
    let mut binding = retain(&mut held, &authority);
    let mut other_held = prepare(other_writer);
    let other_binding = retain(&mut other_held, &other_authority);
    assert_eq!(binding.stored_fence().subject().lifecycle_generation(), 1);
    assert_eq!(
        other_binding
            .stored_fence()
            .subject()
            .lifecycle_generation(),
        1
    );
    assert_eq!(binding.registration_generation(), original_generation);
    assert_eq!(
        other_binding.registration_generation(),
        other_request.registration_generation()
    );
    let retained = binding.retained_memory_bytes().unwrap();
    assert!(retained >= std::mem::size_of::<SourceLeaseMaintenanceBinding>());
    for wrong in [0, other_request.registration_generation(), u64::MAX] {
        binding.set_registration_generation_for_tests(wrong);
        assert!(held.validate_lease_binding(&binding, &authority).is_err());
        assert!(
            held.validate_registered_lease_binding(&binding, &authority)
                .is_err()
        );
        assert_eq!(count(&request), 1);
        busy(&external);
    }
    binding.set_registration_generation_for_tests(original_generation);
    held.validate_registered_lease_binding(&binding, &authority)
        .unwrap();
    assert!(
        held.validate_registered_lease_binding(&binding, &other_authority)
            .is_err()
    );
    let writer = held.abort().unwrap();
    let other_writer = other_held.abort().unwrap();
    assert_eq!((count(&request), count(&other_request)), (0, 0));
    assert_eq!(settings(&writer), original_settings);
    assert_eq!(settings(&other_writer), other_settings);
    assert_eq!(writer.database.connection.total_changes(), original_changes);
    assert_eq!(lease(&external), original_lease);
    external.execute_batch("BEGIN IMMEDIATE; ROLLBACK").unwrap();
    drop(authority);
    drop(other_authority);
    writer.close().unwrap();
    other_writer.close().unwrap();
}
