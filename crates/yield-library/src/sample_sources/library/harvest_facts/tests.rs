//! Real retained owners and SQLite metadata fixtures, not physical file qualification.
use super::super::{HarvestAuthoredRequest, HarvestAuthoredResult, HarvestAuthoredSnapshotRequest};
use super::*;
use crate::app_dirs::{ConfigBaseGuard, PersistenceProfileGuard};
use crate::operation_journal::OperationId;
use crate::sample_sources::db::SourceContentIdentityObservation;
use crate::sample_sources::library::{GlobalLibraryWriter, GlobalLibraryWriterStatus};
use crate::sample_sources::{SourceDatabase, SourceId, SourceWriter};
use crate::test_runtime::TestRuntimeGuard;
use rusqlite::Connection;
use std::{
    path::Path,
    time::{Duration, Instant},
};
mod bounds;
mod failures;
mod publication;
struct Fixture {
    global: Option<GlobalLibraryWriter>,
    source: Option<SourceWriter>,
    guard: Option<WritableProfileGuard>,
    _root: tempfile::TempDir,
    _profile: PersistenceProfileGuard,
    _config: ConfigBaseGuard,
    _base: tempfile::TempDir,
    _runtime: TestRuntimeGuard,
}
fn wait_global(w: &GlobalLibraryWriter) {
    let end = Instant::now() + Duration::from_secs(3);
    while w.status() != GlobalLibraryWriterStatus::Available {
        assert!(Instant::now() < end, "{:?}", w.status());
        std::thread::sleep(Duration::from_millis(5));
    }
}
fn source(
    root: &tempfile::TempDir,
    guard: &WritableProfileGuard,
    id: &str,
    paths: &[String],
) -> SourceWriter {
    let db = SourceDatabase::open_for_catalog_with_profile_guard(root.path(), guard, Path::new(id))
        .unwrap();
    let mut batch = db.write_batch().unwrap();
    for path in paths {
        batch
            .upsert_file_with_hash(Path::new(path), 10, 1, &"a".repeat(64))
            .unwrap();
        batch
            .set_file_identity(Path::new(path), Some(&format!("object-{id}")))
            .unwrap();
    }
    batch.commit().unwrap();
    db.begin_source_directory_truth_generation(1, 0).unwrap();
    let writer = SourceWriter::new_for_scan(db, SourceId::from_string(id), 1).unwrap();
    let fence = writer.current_scan_fence().unwrap();
    let revision = fence.revision();
    let root = fence.root_identity().clone();
    writer
        .catalog_database()
        .write_batch()
        .unwrap()
        .commit_manifest_audit_with_staged_directory_truth(1, revision, 10, root, None)
        .unwrap();
    writer
}
fn content(source: &SourceWriter, path: &str) -> SourceContentIdentityObservation {
    source
        .observe_file_identity(StoredRelativePath::new(path).unwrap())
        .unwrap()
        .into_content_identity()
        .unwrap()
}

impl Fixture {
    fn new() -> Self {
        Self::with_paths(vec!["one.wav".into()])
    }
    fn with_paths(paths: Vec<String>) -> Self {
        let runtime = TestRuntimeGuard::acquire();
        let base = tempfile::tempdir().unwrap();
        let config = ConfigBaseGuard::set(base.path().into());
        let profile = PersistenceProfileGuard::named("harvest-authored-owner");
        let guard = WritableProfileGuard::acquire_current().unwrap();
        let global = GlobalLibraryWriter::start(&guard).unwrap();
        wait_global(&global);
        let root = tempfile::tempdir().unwrap();
        let source = source(&root, &guard, "origin", &paths);
        Self {
            global: Some(global),
            source: Some(source),
            guard: Some(guard),
            _root: root,
            _profile: profile,
            _config: config,
            _base: base,
            _runtime: runtime,
        }
    }
    fn guard(&self) -> &WritableProfileGuard {
        self.guard.as_ref().unwrap()
    }
    fn global(&self) -> &GlobalLibraryWriter {
        self.global.as_ref().unwrap()
    }
    fn source(&self) -> &SourceWriter {
        self.source.as_ref().unwrap()
    }
    fn db(&self) -> Connection {
        Connection::open(
            self.guard()
                .profile_root()
                .join(super::super::LIBRARY_DB_FILE_NAME),
        )
        .unwrap()
    }
    fn observations(&self, paths: &[&str]) -> Vec<SourceFileObservation> {
        paths
            .iter()
            .map(|path| {
                self.source()
                    .observe_file_identity(StoredRelativePath::new(*path).unwrap())
                    .unwrap()
            })
            .collect()
    }
    fn facts_request(
        &self,
        paths: &[&str],
        token: HarvestFactsCancellation,
    ) -> Result<HarvestFactsRequest, LibraryError> {
        HarvestFactsRequest::new(self.guard(), self.observations(paths), token)
    }
    fn read(&self, request: HarvestFactsRequest) -> Result<HarvestFactsBatch, LibraryError> {
        self.global()
            .read_harvest_facts(request)
            .unwrap()
            .recv_timeout(Duration::from_secs(5))
            .unwrap()
    }
    fn facts(&self) -> Result<HarvestFactsBatch, LibraryError> {
        self.read(
            self.facts_request(&["one.wav"], HarvestFactsCancellation::new())
                .unwrap(),
        )
    }
    fn source_db(&self) -> Connection {
        Connection::open(
            self.guard()
                .profile_root()
                .join("origin")
                .join(crate::sample_sources::db::DB_FILE_NAME),
        )
        .unwrap()
    }
    fn authored_request(&self, state: HarvestState) -> HarvestAuthoredRequest {
        let source = content(self.source(), "one.wav");
        let snapshot = self
            .global()
            .harvest_authored_snapshot(
                HarvestAuthoredSnapshotRequest::new(self.guard(), source.clone()).unwrap(),
            )
            .unwrap()
            .recv_timeout(Duration::from_secs(3))
            .unwrap()
            .unwrap();
        HarvestAuthoredRequest::new(self.guard(), OperationId::new(), snapshot, source, state)
            .unwrap()
    }
    fn apply(
        &self,
        request: HarvestAuthoredRequest,
    ) -> Result<HarvestAuthoredResult, LibraryError> {
        self.global()
            .set_harvest_authored_state(request)
            .unwrap()
            .recv_timeout(Duration::from_secs(5))
            .unwrap()
    }
    fn count(&self) -> i64 {
        self.db()
            .query_row("SELECT COUNT(*) FROM harvest_authored_receipts", [], |r| {
                r.get(0)
            })
            .unwrap()
    }
    fn restart(&mut self) {
        let mut g = self.global.take().unwrap();
        g.shutdown();
        drop(g);
        let g = GlobalLibraryWriter::start(self.guard()).unwrap();
        wait_global(&g);
        self.global = Some(g);
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        if let Some(mut g) = self.global.take() {
            g.shutdown();
        }
        if let Some(s) = self.source.take() {
            s.into_catalog_database().close().unwrap();
        }
        self.guard.take();
    }
}
