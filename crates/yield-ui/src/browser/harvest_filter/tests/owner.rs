//! Genuine retained owner setup; WAV fixtures supply actual audited identities.
#[cfg(test)]
mod fixtures {
    use super::super::*;
    use crate::browser::{SourceSession, catalog::CatalogActivation};
    use std::time::{Duration, Instant};
    use yield_library::app_dirs::{ConfigBaseGuard, PersistenceProfileGuard, WritableProfileGuard};
    pub(in crate::browser::harvest_filter::tests) struct OwnerFixture {
        _guard: WritableProfileGuard,
        _profile: PersistenceProfileGuard,
        _base: ConfigBaseGuard,
        pub(in crate::browser::harvest_filter::tests) root: tempfile::TempDir,
        _config: tempfile::TempDir,
    }
    pub(in crate::browser::harvest_filter::tests) fn owner() -> (
        OwnerFixture,
        yield_catalog::CatalogService,
        yield_catalog::CatalogSnapshot,
        yield_library::sample_sources::library::GlobalLibraryWriter,
    ) {
        let config = tempfile::tempdir().unwrap();
        let base = ConfigBaseGuard::set(config.path().canonicalize().unwrap());
        let profile = PersistenceProfileGuard::named("harvest-gpui-owner");
        let guard = WritableProfileGuard::acquire_current().unwrap();
        let root = tempfile::tempdir().unwrap();
        let frames = 4000u32;
        let bytes = frames * 2;
        let mut wav = Vec::new();
        wav.extend_from_slice(b"RIFF");
        wav.extend_from_slice(&(36 + bytes).to_le_bytes());
        wav.extend_from_slice(b"WAVEfmt ");
        wav.extend_from_slice(&16u32.to_le_bytes());
        wav.extend_from_slice(&1u16.to_le_bytes());
        wav.extend_from_slice(&1u16.to_le_bytes());
        wav.extend_from_slice(&16000u32.to_le_bytes());
        wav.extend_from_slice(&32000u32.to_le_bytes());
        wav.extend_from_slice(&2u16.to_le_bytes());
        wav.extend_from_slice(&16u16.to_le_bytes());
        wav.extend_from_slice(b"data");
        wav.extend_from_slice(&bytes.to_le_bytes());
        for i in 0..frames {
            wav.extend_from_slice(&(((i as f64 / 16.).sin() * 12000.) as i16).to_le_bytes());
        }
        for name in ["anchor.wav", "same.wav", "missing.wav"] {
            std::fs::write(root.path().join(name), &wav).unwrap();
        }
        let global =
            yield_library::sample_sources::library::GlobalLibraryWriter::start(&guard).unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while global.status()
            != yield_library::sample_sources::library::GlobalLibraryWriterStatus::Available
        {
            assert!(Instant::now() < deadline);
            std::thread::yield_now();
        }
        let service = yield_catalog::CatalogService::start_with_profile_owners(
            &guard,
            None,
            global.harvest_facts_client(),
        )
        .unwrap();
        let id = yield_library::sample_sources::SourceId::from_string("harvest-gpui-owner");
        service
            .submit(CatalogCommand::Register {
                source_id: id.clone(),
                canonical_root: root.path().into(),
                lifecycle_generation: 1,
            })
            .unwrap();
        next_owner(&service, |e| matches!(e, CatalogEvent::Registered { .. }));
        service
            .submit(CatalogCommand::RequestAudit {
                source_id: id.clone(),
                lifecycle_generation: 1,
            })
            .unwrap();
        let CatalogEvent::AuditCommitted { snapshot } = next_owner(&service, |e| {
            matches!(e, CatalogEvent::AuditCommitted { .. })
        }) else {
            panic!()
        };
        (
            OwnerFixture {
                _guard: guard,
                _profile: profile,
                _base: base,
                root,
                _config: config,
            },
            service,
            snapshot,
            global,
        )
    }
    fn next_owner(
        service: &yield_catalog::CatalogService,
        matches: impl Fn(&CatalogEvent) -> bool,
    ) -> CatalogEvent {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if let Ok(event) = service.try_recv_event() {
                if matches(&event) {
                    return event;
                }
            }
            assert!(Instant::now() < deadline, "owner completion timeout");
            std::thread::yield_now();
        }
    }
    pub(in crate::browser::harvest_filter::tests) fn install_owned(
        view: &mut Browser,
        fixture: &OwnerFixture,
        snapshot: &yield_catalog::CatalogSnapshot,
    ) {
        let root = fixture.root.path().to_path_buf();
        let mut session = SourceSession::new(root.clone());
        session.source_id = Some(snapshot.source_id.clone());
        view.source_sessions = vec![session];
        view.active_source = Some(0);
        view.root = Some(root.clone());
        view.source_registry_loaded = true;
        view.catalog_activation = Some(CatalogActivation {
            source_id: snapshot.source_id.clone(),
            root: root.clone(),
            lifecycle_generation: snapshot.lifecycle_generation,
            registered: true,
            registration_admitted: true,
            root_identity: Some(snapshot.root_identity.clone()),
            audit_finished: true,
            audit_error: None,
            audit_retryable: false,
            force_reload: false,
            fallback_to_scan: false,
        });
        view.entries = snapshot
            .manifest
            .iter()
            .map(|entry| yield_core::AudioFile {
                path: root.join(&entry.relative_path),
                relative_path: entry.relative_path.clone(),
                byte_len: entry.file_size,
                duration_seconds: None,
                sample_rate: None,
                playback_type: None,
                curation: None,
            })
            .collect();
        view.view_model = yield_core::BrowserViewModel::new(&view.entries);
        let key = crate::browser::curation::CurationLifecycleKey {
            source_id: snapshot.source_id.clone(),
            lifecycle_generation: snapshot.lifecycle_generation,
        };
        view.curation_state.install_projection(
            key,
            snapshot.root_identity.clone(),
            snapshot.directory_generation,
            snapshot.committed_revision,
            &view.entries,
        );
        view.catalog_metadata
            .install_projection(CatalogMetadataContext {
                source_id: snapshot.source_id.clone(),
                lifecycle_generation: snapshot.lifecycle_generation,
                root,
                root_identity: snapshot.root_identity.clone(),
                directory_generation: snapshot.directory_generation,
                revision: snapshot.committed_revision,
                entry_indices: Arc::new(
                    view.entries
                        .iter()
                        .enumerate()
                        .map(|(i, e)| (e.relative_path.clone(), i))
                        .collect(),
                ),
            });
    }
}
#[cfg(test)]
pub(super) use fixtures::{install_owned, owner};
