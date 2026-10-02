//! Proposed OPT-1996 contract fixture, not an OS adapter, service, or capability.
use std::collections::BTreeMap;
use std::path::PathBuf;
use yield_catalog::{CatalogError, CatalogWavMetadata};
use yield_library::sample_sources::{
    SourceId,
    reconciliation::{RootIdentity, RootRelativePath},
};
use yield_session::{CanonicalSourceRoot, CatalogCommand, CatalogEvent, RegistryRevision};

#[derive(Clone, Debug, Eq, PartialEq)]
struct CapturedOwner {
    profile: String,
    incarnation: u64,
    source: SourceId,
    root: CanonicalSourceRoot,
    physical_root: RootIdentity,
    registry_revision: u64,
    lifecycle: u64,
    catalog_revision: u64,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Startup {
    CurrentReady,
    Loading,
    Unavailable,
    MigrationRequired,
    Closed,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Outcome {
    MetadataAvailable,
    NotAvailable,
    ReadFailed,
    Full,
    Invalid,
    Superseded,
    Unavailable,
    Closed,
}
#[derive(Clone)]
struct Pending {
    id: u64,
    owner: CapturedOwner,
    path: PathBuf,
}
struct HandoffFixture {
    next: u64,
    pending: Option<Pending>,
}
impl HandoffFixture {
    fn new() -> Self {
        Self {
            next: 1,
            pending: None,
        }
    }
    fn propose(
        &mut self,
        startup: Startup,
        current: &CapturedOwner,
        captured: CapturedOwner,
        path: PathBuf,
    ) -> Result<CatalogCommand, Outcome> {
        match startup {
            Startup::Closed => return Err(Outcome::Closed),
            Startup::CurrentReady => {}
            _ => return Err(Outcome::Unavailable),
        }
        if &captured != current || captured.lifecycle == 0 || captured.incarnation == 0 {
            return Err(Outcome::Superseded);
        }
        if path.as_os_str().as_encoded_bytes().len() > 4096
            || RootRelativePath::try_from_path(path.clone()).is_err()
            || captured.root.as_path().as_os_str().as_encoded_bytes().len() > 4096
            || captured.profile.is_empty()
            || captured.profile.len() > 256
            || captured.source.to_string().is_empty()
            || captured.source.to_string().len() > 256
            || captured.physical_root.as_bytes().is_empty()
            || captured.physical_root.as_bytes().len() > 256
        {
            return Err(Outcome::Invalid);
        }
        if self.pending.is_some() {
            return Err(Outcome::Full);
        }
        let id = self.next;
        self.next = self.next.checked_add(1).ok_or(Outcome::Closed)?;
        self.pending = Some(Pending {
            id,
            owner: captured.clone(),
            path: path.clone(),
        });
        Ok(CatalogCommand::RequestWavMetadata {
            source_id: captured.source,
            lifecycle_generation: captured.lifecycle,
            expected_revision: captured.catalog_revision,
            request_id: id,
            relative_paths: vec![path],
        })
    }
    fn cancel(&mut self) {
        self.pending = None;
    }
    fn submission_failed(&mut self, id: u64) {
        if self
            .pending
            .as_ref()
            .is_some_and(|pending| pending.id == id)
        {
            self.cancel();
        }
    }
    fn publish(
        &mut self,
        startup: Startup,
        current: &CapturedOwner,
        event: CatalogEvent,
    ) -> Option<Outcome> {
        let pending = self.pending.as_ref()?;
        match event {
            CatalogEvent::WavMetadataReady {
                source_id,
                lifecycle_generation,
                root_identity,
                revision,
                request_id,
                requested_paths,
                metadata,
            } => {
                if request_id != pending.id || source_id != pending.owner.source {
                    return None;
                }
                let matches = startup == Startup::CurrentReady
                    && &pending.owner == current
                    && source_id == pending.owner.source
                    && lifecycle_generation == pending.owner.lifecycle
                    && root_identity == pending.owner.physical_root
                    && revision == pending.owner.catalog_revision
                    && requested_paths == [pending.path.clone()]
                    && metadata.keys().all(|path| path == &pending.path);
                let present = metadata.contains_key(&pending.path);
                self.cancel();
                Some(if !matches {
                    Outcome::Superseded
                } else if present {
                    Outcome::MetadataAvailable
                } else {
                    Outcome::NotAvailable
                })
            }
            CatalogEvent::WavMetadataFailed {
                source_id,
                lifecycle_generation,
                expected_revision,
                request_id,
                ..
            } => {
                if request_id != pending.id || source_id != pending.owner.source {
                    return None;
                }
                let matches = startup == Startup::CurrentReady
                    && &pending.owner == current
                    && source_id == pending.owner.source
                    && lifecycle_generation == pending.owner.lifecycle
                    && expected_revision == pending.owner.catalog_revision;
                self.cancel();
                Some(if matches {
                    Outcome::ReadFailed
                } else {
                    Outcome::Superseded
                })
            }
            _ => None,
        }
    }
}
fn current() -> CapturedOwner {
    CapturedOwner {
        profile: "fixture-current-profile".into(),
        incarnation: 1,
        source: SourceId::from_string("fixture-source"),
        root: CanonicalSourceRoot::new(PathBuf::from("/fixture/source")).unwrap(),
        physical_root: RootIdentity::from_bytes(vec![1, 2, 3]),
        registry_revision: RegistryRevision::default().get(),
        lifecycle: 7,
        catalog_revision: 11,
    }
}
fn ready(pending: &Pending, present: bool) -> CatalogEvent {
    let mut metadata = BTreeMap::new();
    if present {
        metadata.insert(
            pending.path.clone(),
            CatalogWavMetadata {
                frames: 40,
                sample_rate: 8000,
            },
        );
    }
    CatalogEvent::WavMetadataReady {
        source_id: pending.owner.source.clone(),
        lifecycle_generation: pending.owner.lifecycle,
        root_identity: pending.owner.physical_root.clone(),
        revision: pending.owner.catalog_revision,
        request_id: pending.id,
        requested_paths: vec![pending.path.clone()],
        metadata,
    }
}
fn admit(fixture: &mut HandoffFixture, owner: &CapturedOwner) {
    fixture
        .propose(
            Startup::CurrentReady,
            owner,
            owner.clone(),
            PathBuf::from("音/drum.wav"),
        )
        .unwrap();
}

#[test]
fn current_profile_os_entry_constructs_one_existing_owner_read_and_exact_completion() {
    let owner = current();
    let mut fixture = HandoffFixture::new();
    let command = fixture
        .propose(
            Startup::CurrentReady,
            &owner,
            owner.clone(),
            PathBuf::from("音/drum.wav"),
        )
        .unwrap();
    assert!(
        matches!(command, CatalogCommand::RequestWavMetadata { source_id, lifecycle_generation: 7, expected_revision: 11, request_id: 1, relative_paths }
        if source_id == owner.source && relative_paths == [PathBuf::from("音/drum.wav")])
    );
    assert_eq!(
        fixture.publish(
            Startup::CurrentReady,
            &owner,
            ready(fixture.pending.as_ref().unwrap(), true)
        ),
        Some(Outcome::MetadataAvailable)
    );
    assert!(fixture.pending.is_none());
    admit(&mut fixture, &owner);
    assert_eq!(
        fixture.publish(
            Startup::CurrentReady,
            &owner,
            ready(fixture.pending.as_ref().unwrap(), false)
        ),
        Some(Outcome::NotAvailable)
    );
}
#[test]
fn current_profile_os_entry_rejects_startup_and_identity_changes_without_admission() {
    let owner = current();
    let mut fixture = HandoffFixture::new();
    for startup in [
        Startup::Loading,
        Startup::Unavailable,
        Startup::MigrationRequired,
        Startup::Closed,
    ] {
        assert!(
            fixture
                .propose(startup, &owner, owner.clone(), PathBuf::from("a.wav"))
                .is_err()
        );
        assert!(fixture.pending.is_none());
    }
    for field in 0..8 {
        let mut old = owner.clone();
        match field {
            0 => old.profile = "foreign".into(),
            1 => old.incarnation += 1,
            2 => old.source = SourceId::from_string("replaced-source"),
            3 => old.root = CanonicalSourceRoot::new(PathBuf::from("/other")).unwrap(),
            4 => old.physical_root = RootIdentity::from_bytes(vec![4]),
            5 => old.registry_revision += 1,
            6 => old.lifecycle += 1,
            _ => old.catalog_revision += 1,
        }
        assert!(matches!(
            fixture.propose(Startup::CurrentReady, &owner, old, PathBuf::from("a.wav")),
            Err(Outcome::Superseded)
        ));
        assert!(fixture.pending.is_none());
    }
}
#[test]
fn current_profile_os_entry_bounds_slot_path_and_request_identity() {
    let owner = current();
    let mut fixture = HandoffFixture::new();
    for path in [
        PathBuf::new(),
        PathBuf::from("../escape.wav"),
        PathBuf::from("/absolute.wav"),
        PathBuf::from("x".repeat(4097)),
    ] {
        assert!(matches!(
            fixture.propose(Startup::CurrentReady, &owner, owner.clone(), path),
            Err(Outcome::Invalid)
        ));
    }
    admit(&mut fixture, &owner);
    assert!(matches!(
        fixture.propose(
            Startup::CurrentReady,
            &owner,
            owner.clone(),
            PathBuf::from("a.wav")
        ),
        Err(Outcome::Full)
    ));
    let id = fixture.pending.as_ref().unwrap().id;
    fixture.submission_failed(id + 1);
    assert!(fixture.pending.is_some());
    fixture.submission_failed(id);
    assert!(fixture.pending.is_none());
    fixture.next = u64::MAX;
    assert!(matches!(
        fixture.propose(
            Startup::CurrentReady,
            &owner,
            owner.clone(),
            PathBuf::from("a.wav")
        ),
        Err(Outcome::Closed)
    ));
    assert!(fixture.pending.is_none());
}
#[test]
fn current_profile_os_entry_queued_results_revalidate_every_captured_owner_field() {
    for field in 0..8 {
        let owner = current();
        let mut fixture = HandoffFixture::new();
        admit(&mut fixture, &owner);
        let event = ready(fixture.pending.as_ref().unwrap(), true);
        let mut changed = owner;
        match field {
            0 => changed.profile = "foreign".into(),
            1 => changed.incarnation += 1,
            2 => changed.source = SourceId::from_string("replacement"),
            3 => changed.root = CanonicalSourceRoot::new(PathBuf::from("/other")).unwrap(),
            4 => changed.physical_root = RootIdentity::from_bytes(vec![4]),
            5 => changed.registry_revision += 1,
            6 => changed.lifecycle += 1,
            _ => changed.catalog_revision += 1,
        }
        assert_eq!(
            fixture.publish(Startup::CurrentReady, &changed, event),
            Some(Outcome::Superseded)
        );
        assert!(fixture.pending.is_none());
    }
}
#[test]
fn current_profile_os_entry_cancel_retry_and_foreign_paths_cannot_publish() {
    let owner = current();
    let mut fixture = HandoffFixture::new();
    admit(&mut fixture, &owner);
    let old = fixture.pending.clone().unwrap();
    fixture.cancel();
    admit(&mut fixture, &owner);
    assert_eq!(
        fixture.publish(Startup::CurrentReady, &owner, ready(&old, true)),
        None
    );
    assert!(fixture.pending.is_some());
    let mut event = ready(fixture.pending.as_ref().unwrap(), true);
    if let CatalogEvent::WavMetadataReady { metadata, .. } = &mut event {
        metadata.insert(
            PathBuf::from("foreign.wav"),
            CatalogWavMetadata {
                frames: 1,
                sample_rate: 1,
            },
        );
    }
    assert_eq!(
        fixture.publish(Startup::CurrentReady, &owner, event),
        Some(Outcome::Superseded)
    );
    admit(&mut fixture, &owner);
    let pending = fixture.pending.as_ref().unwrap();
    let failure = CatalogEvent::WavMetadataFailed {
        source_id: owner.source.clone(),
        lifecycle_generation: owner.lifecycle,
        expected_revision: owner.catalog_revision,
        request_id: pending.id,
        error: CatalogError::Busy,
    };
    assert_eq!(
        fixture.publish(Startup::CurrentReady, &owner, failure),
        Some(Outcome::ReadFailed)
    );
}
#[cfg(unix)]
#[test]
fn current_profile_os_entry_preserves_lossless_native_relative_path_bytes() {
    use std::os::unix::ffi::OsStringExt;
    let owner = current();
    let mut fixture = HandoffFixture::new();
    let path = PathBuf::from(std::ffi::OsString::from_vec(b"raw-\xff.wav".to_vec()));
    let command = fixture
        .propose(Startup::CurrentReady, &owner, owner.clone(), path.clone())
        .unwrap();
    assert!(
        matches!(command, CatalogCommand::RequestWavMetadata { relative_paths, .. } if relative_paths == [path.clone()])
    );
    assert_eq!(
        fixture
            .pending
            .as_ref()
            .unwrap()
            .path
            .as_os_str()
            .as_encoded_bytes(),
        path.as_os_str().as_encoded_bytes()
    );
}

#[test]
fn current_profile_os_entry_completion_requires_ready_owner_and_foreign_events_do_not_consume_slot()
{
    for startup in [
        Startup::Loading,
        Startup::Unavailable,
        Startup::MigrationRequired,
        Startup::Closed,
    ] {
        let owner = current();
        let mut fixture = HandoffFixture::new();
        admit(&mut fixture, &owner);
        let event = ready(fixture.pending.as_ref().unwrap(), true);
        assert_eq!(
            fixture.publish(startup, &owner, event),
            Some(Outcome::Superseded)
        );
        assert!(fixture.pending.is_none());
    }
    let owner = current();
    let mut fixture = HandoffFixture::new();
    admit(&mut fixture, &owner);
    let mut event = ready(fixture.pending.as_ref().unwrap(), true);
    if let CatalogEvent::WavMetadataReady { source_id, .. } = &mut event {
        *source_id = SourceId::from_string("another-metadata-producer-source");
    }
    assert_eq!(fixture.publish(Startup::CurrentReady, &owner, event), None);
    assert!(fixture.pending.is_some());
    assert_eq!(
        fixture.publish(
            Startup::CurrentReady,
            &owner,
            ready(fixture.pending.as_ref().unwrap(), true)
        ),
        Some(Outcome::MetadataAvailable)
    );
}

#[test]
fn current_profile_os_entry_bounds_captured_identity_observations() {
    for field in 0..6 {
        let mut owner = current();
        match field {
            0 => owner.profile = "p".repeat(257),
            1 => owner.source = SourceId::from_string("s".repeat(257)),
            2 => owner.physical_root = RootIdentity::from_bytes(vec![1; 257]),
            3 => owner.physical_root = RootIdentity::from_bytes(Vec::new()),
            4 => {
                owner.root =
                    CanonicalSourceRoot::new(PathBuf::from(format!("/{}", "r".repeat(4096))))
                        .unwrap()
            }
            _ => owner.profile.clear(),
        }
        let mut fixture = HandoffFixture::new();
        assert!(matches!(
            fixture.propose(
                Startup::CurrentReady,
                &owner,
                owner.clone(),
                PathBuf::from("a.wav")
            ),
            Err(Outcome::Invalid)
        ));
        assert!(fixture.pending.is_none());
    }
}
