//! Original-worker read-only target preparation; no namespace, capacity or journal grant.
use super::*;
#[cfg(windows)]
mod windows_staging_share;
const MAX_BINDINGS: usize = 32;
const BACKING: usize = 64 * 1024;
const HEADER: usize = 16 * 1024;
const REGISTRY_BYTES: usize = 32 * 1024;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum BindingError {
    Invalid,
    Missing,
    Conflict,
    Full,
    Closed,
    Deadline,
    Cancelled,
    Unknown,
    Occupied,
}
pub(super) struct BindingRequest {
    validation: Arc<FileOperationResult>,
    target: RootRegistrationRef,
    leaf: BoundedRelativeLocator,
    effect: EffectId,
    deadline: Instant,
    cancelled: Arc<AtomicBool>,
    input: InputPermit,
}
/// Original live binding, not a live namespace authorization. No Clone or decoder.
#[allow(dead_code)] // Consumed by the complete journal issuer, OPT-2215.
pub(crate) struct PreparedPublicationTarget {
    data: Arc<BindingData>,
    _result: ResultPermit,
}
#[allow(dead_code)] // Private producer interface consumed by complete OPT-2215.
pub(super) struct BindingData {
    validation: Arc<FileOperationResult>,
    target: RootRoleBinding,
    effect: EffectId,
    fingerprint: RequestFingerprint,
    deadline: Instant,
    #[allow(dead_code)]
    // Retained read-only original handle for the future backend, not effect rights.
    staged_file: std::fs::File,
    volume: crate::durable_capacity::VolumeIdentity,
    lives: Vec<Arc<AtomicBool>>,
    shared: Weak<OwnerShared>,
    _input: InputPermit,
}
#[allow(dead_code)] // Private producer interface consumed by complete OPT-2215.
pub(crate) struct PublicationTargetPending {
    receiver: Receiver<Result<Arc<BindingData>, BindingError>>,
    cancelled: Arc<AtomicBool>,
    result: Option<ResultPermit>,
    received: bool,
}
#[allow(dead_code)] // Private producer interface consumed by complete OPT-2215.
impl PublicationTargetPending {
    pub(crate) fn try_target(&mut self) -> Result<Option<PreparedPublicationTarget>, BindingError> {
        if self.received {
            return Err(BindingError::Closed);
        }
        match self.receiver.try_recv() {
            Ok(Ok(data)) => {
                self.received = true;
                Ok(Some(PreparedPublicationTarget {
                    data,
                    _result: self.result.take().ok_or(BindingError::Closed)?,
                }))
            }
            Ok(Err(error)) => {
                self.received = true;
                self.result.take();
                Err(error)
            }
            Err(mpsc::TryRecvError::Empty) => Ok(None),
            Err(mpsc::TryRecvError::Disconnected) => {
                self.received = true;
                self.result.take();
                Err(BindingError::Closed)
            }
        }
    }
}
impl Drop for PublicationTargetPending {
    fn drop(&mut self) {
        self.cancelled.store(true, Ordering::Release);
    }
}
#[allow(dead_code)] // Private producer interface consumed by complete OPT-2215.
impl PreparedPublicationTarget {
    /// Build only the authority-free bytes selected by this original binding.
    pub(crate) fn publication_command(
        &self,
    ) -> Result<FileOperationCommand, FileOperationValidationError> {
        let validation = self.validation();
        let EffectOutcome::Applied { evidence } = validation.outcome() else {
            return Err(FileOperationValidationError::new(
                "original staging validation is absent",
            ));
        };
        let proof = evidence
            .validated_staging()
            .ok_or_else(|| FileOperationValidationError::new("original staging proof is absent"))?;
        let [staging] = validation.root_bindings() else {
            return Err(FileOperationValidationError::new(
                "original staging role is absent",
            ));
        };
        let object = evidence
            .observed_identity
            .ok_or_else(|| FileOperationValidationError::new("original staged object is absent"))?;
        let command = FileOperationCommand {
            operation_id: validation.operation_id(),
            participant_id: validation.participant_id(),
            effect_id: self.publication_effect(),
            source_fences: validation.source_fences().to_vec(),
            target_registration: self.target().registration,
            expected: ExpectedObject::Absent,
            effect: EffectRequest::PublishNoReplace {
                staging: StagingRoot {
                    binding: RootRoleBinding {
                        registration: staging.registration,
                        authority: staging.authority.clone(),
                        locator: staging.locator.clone(),
                        expected: ExpectedObject::Exact(object),
                    },
                },
                target: DestinationRoot {
                    binding: self.target().clone(),
                },
                staged: CopyValidatedEvidence {
                    object,
                    bytes: proof.bytes(),
                },
                expected_absent: AbsenceProof { marker: 1 },
            },
            authorization: EffectAuthorization::JournalGrant(JournalGrantAuthorization {
                _sealed: (),
            }),
        };
        command.validate_shape()?;
        Ok(command)
    }
    /// Scalar liveness only; actual roots/native objects require worker revalidation at entry.
    pub(crate) fn is_current(&self) -> bool {
        Instant::now() < self.data.deadline
            && self.data.shared.upgrade().is_some_and(|shared| {
                OwnerLifecycle::from_u8(shared.lifecycle.load(Ordering::Acquire))
                    == OwnerLifecycle::Running
            })
            && self
                .data
                .lives
                .iter()
                .all(|alive| alive.load(Ordering::Acquire))
    }
    pub(crate) fn validation(&self) -> &FileOperationResult {
        &self.data.validation
    }
    pub(crate) fn target(&self) -> &RootRoleBinding {
        &self.data.target
    }
    pub(crate) fn publication_effect(&self) -> EffectId {
        self.data.effect
    }
    pub(crate) fn fingerprint(&self) -> RequestFingerprint {
        self.data.fingerprint
    }
    fn revalidate_held_staging(&self) -> Result<(), BindingError> {
        self.data.revalidate_held_staging()
    }
    pub(crate) fn volume(&self) -> &crate::durable_capacity::VolumeIdentity {
        &self.data.volume
    }
}
impl BindingData {
    /// Fresh descriptor observations only; this never establishes creation provenance.
    fn revalidate_held_staging(&self) -> Result<(), BindingError> {
        let check = || {
            if Instant::now() < self.deadline
                && self.shared.upgrade().is_some_and(|shared| {
                    OwnerLifecycle::from_u8(shared.lifecycle.load(Ordering::Acquire))
                        == OwnerLifecycle::Running
                })
                && self.lives.iter().all(|life| life.load(Ordering::Acquire))
            {
                Ok(())
            } else {
                Err(BindingError::Closed)
            }
        };
        check()?;
        let EffectOutcome::Applied { evidence } = self.validation.outcome() else {
            return Err(BindingError::Invalid);
        };
        let proof = evidence.validated_staging().ok_or(BindingError::Invalid)?;
        let file = &self.staged_file;
        let metadata_matches = || {
            let metadata = file.metadata().map_err(|_| BindingError::Unknown)?;
            if !metadata.is_file()
                || metadata.len() != proof.bytes
                || stable_filesystem_identity_from_open_file(file).as_deref()
                    != Some(proof.stable_identity.as_str())
            {
                return Err(BindingError::Conflict);
            }
            #[cfg(windows)]
            validate_windows_held_staging(file)?;
            #[cfg(unix)]
            if std::os::unix::fs::MetadataExt::nlink(&metadata) != 1 {
                return Err(BindingError::Conflict);
            }
            Ok(())
        };
        metadata_matches()?;
        // Windows seek_read may move its supplied handle's cursor. ReOpenFile derives
        // one independent read handle from this exact held object, never a pathname.
        #[cfg(windows)]
        let reader = reopen_held_staging_reader(file).map_err(|_| BindingError::Unknown)?;
        #[cfg(windows)]
        let reader = &reader;
        #[cfg(not(windows))]
        let reader = file;
        let mut hasher = blake3::Hasher::new();
        let mut scratch = [0u8; 8192];
        let mut offset = 0u64;
        loop {
            check()?;
            let count = read_held_staging_at(reader, &mut scratch, offset)
                .map_err(|_| BindingError::Unknown)?;
            if count == 0 {
                break;
            }
            offset = offset
                .checked_add(count as u64)
                .filter(|bytes| *bytes <= proof.bytes)
                .ok_or(BindingError::Conflict)?;
            hasher.update(&scratch[..count]);
        }
        if offset != proof.bytes || *hasher.finalize().as_bytes() != proof.content_hash {
            return Err(BindingError::Conflict);
        }
        metadata_matches()?;
        if crate::durable_capacity::descriptor_capacity_facts(file)
            .map_err(|_| BindingError::Unknown)?
            .identity
            != self.volume
        {
            return Err(BindingError::Conflict);
        }
        check()
    }
}
#[cfg(any(windows, test))]
fn validate_windows_staging_shape(
    kind: u32,
    attributes: u32,
    serial: u32,
    index: u64,
    links: u32,
) -> Result<(), BindingError> {
    // FILE_TYPE_DISK, no directory/device/reparse, exact singleton namespace link.
    if kind != 1
        || attributes & (0x10 | 0x40 | 0x400) != 0
        || serial == 0
        || index == 0
        || links != 1
    {
        return Err(BindingError::Conflict);
    }
    Ok(())
}
#[cfg(windows)]
fn validate_windows_held_staging(file: &std::fs::File) -> Result<(), BindingError> {
    use std::os::windows::io::AsRawHandle;
    use windows::Win32::{
        Foundation::HANDLE,
        Storage::FileSystem::{
            BY_HANDLE_FILE_INFORMATION, GetFileInformationByHandle, GetFileType,
        },
    };
    let handle = HANDLE(file.as_raw_handle());
    let mut info = BY_HANDLE_FILE_INFORMATION::default();
    // SAFETY: live borrowed file handle and initialized output with its exact native ABI.
    unsafe { GetFileInformationByHandle(handle, &mut info) }.map_err(|_| BindingError::Unknown)?;
    // SAFETY: the same live borrowed handle remains owned throughout inspection.
    let kind = unsafe { GetFileType(handle) }.0;
    validate_windows_staging_shape(
        kind,
        info.dwFileAttributes,
        info.dwVolumeSerialNumber,
        (u64::from(info.nFileIndexHigh) << 32) | u64::from(info.nFileIndexLow),
        info.nNumberOfLinks,
    )
}
#[cfg(windows)]
fn reopen_held_staging_reader(original: &std::fs::File) -> std::io::Result<std::fs::File> {
    use std::os::windows::io::{AsRawHandle, FromRawHandle};
    use windows::Win32::{
        Foundation::HANDLE,
        Storage::FileSystem::{
            FILE_FLAG_OPEN_REPARSE_POINT, FILE_GENERIC_READ, FILE_SHARE_DELETE, FILE_SHARE_READ,
            FILE_SHARE_WRITE, ReOpenFile,
        },
    };
    let expected = stable_filesystem_identity_from_open_file(original)
        .ok_or_else(|| std::io::Error::other("unknown original staged identity"))?;
    // SAFETY: caller verified the held regular file; this requests only readonly
    // access to that same object. No path, create, write or namespace right is used.
    let handle = unsafe {
        ReOpenFile(
            HANDLE(original.as_raw_handle()),
            FILE_GENERIC_READ.0,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            FILE_FLAG_OPEN_REPARSE_POINT,
        )
    }
    .map_err(std::io::Error::other)?;
    // SAFETY: the returned valid owned handle is closed on every subsequent path.
    let reader = unsafe { std::fs::File::from_raw_handle(handle.0) };
    if stable_filesystem_identity_from_open_file(&reader).as_deref() != Some(expected.as_str())
        || stable_filesystem_identity_from_open_file(original).as_deref() != Some(expected.as_str())
    {
        return Err(std::io::Error::other(
            "original staged reader identity changed",
        ));
    }
    Ok(reader)
}
fn read_held_staging_at(
    file: &std::fs::File,
    bytes: &mut [u8],
    offset: u64,
) -> std::io::Result<usize> {
    #[cfg(unix)]
    {
        std::os::unix::fs::FileExt::read_at(file, bytes, offset)
    }
    #[cfg(windows)]
    {
        std::os::windows::fs::FileExt::seek_read(file, bytes, offset)
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = (file, bytes, offset);
        Err(std::io::ErrorKind::Unsupported.into())
    }
}
impl FileOperationRequest {
    /// Preserve this original worker reply for a private producer. Ordinary result clones stay
    /// observations and cannot pass the original-receipt pointer check.
    #[allow(dead_code)] // Bound publication producer consumes this route in OPT-2215.
    pub(crate) fn recv_original_staging_validation(
        &mut self,
        timeout: Duration,
    ) -> Result<Arc<FileOperationResult>, FileOperationRequestError> {
        if self.received {
            return Err(FileOperationRequestError::AlreadyConsumed);
        }
        match self.receiver.recv_timeout(timeout) {
            Ok(result) => {
                self.received = true;
                Ok(result)
            }
            Err(RecvTimeoutError::Timeout) => Err(FileOperationRequestError::Timeout),
            Err(RecvTimeoutError::Disconnected) => Err(FileOperationRequestError::Disconnected),
        }
    }
}
// Covers fixed weak-tree nodes and Arc allocation headers, including expired observations.
const _: () = assert!(
    MAX_BINDINGS
        * (std::mem::size_of::<BindingData>()
            + std::mem::size_of::<EffectKey>()
            + std::mem::size_of::<Weak<BindingData>>()
            + 8 * std::mem::size_of::<usize>())
        <= REGISTRY_BYTES
);
impl FileOperationOwner {
    /// Prepare only a direct absent destination on the original root worker. Observation inputs
    /// cannot mint the returned original binding without that worker's retained receipt/roots.
    #[allow(dead_code)] // Actual journal composition is OPT-2215.
    pub(crate) fn prepare_publication_target(
        &self,
        validation: Arc<FileOperationResult>,
        target: RootRegistrationRef,
        leaf: BoundedRelativeLocator,
        effect: EffectId,
        deadline: Instant,
    ) -> Result<PublicationTargetPending, FileOperationOwnerQueueError> {
        self.shared.ensure_accepting()?;
        if deadline <= Instant::now()
            || deadline > Instant::now() + Duration::from_secs(2)
            || target.owner != self.shared.owner
            || effect == validation.effect_id
            || leaf.as_str().contains(['/', '\\'])
            || retained_validation_bytes(&validation).is_none_or(|n| n > HEADER)
        {
            return Err(FileOperationOwnerQueueError::InvalidInput(
                FileOperationValidationError::new("unsupported bounded publication target input"),
            ));
        }
        let input = self.shared.reserve_input(BACKING)?;
        let result = self.shared.reserve_result()?;
        let cancelled = Arc::new(AtomicBool::new(false));
        let (tx, receiver) = mpsc::sync_channel(1);
        let request = BindingRequest {
            validation,
            target,
            leaf: BoundedRelativeLocator::new(leaf.as_str())?,
            effect,
            deadline,
            cancelled: cancelled.clone(),
            input,
        };
        self.shared
            .try_control(OwnerCommand::PreparePublicationTarget {
                request,
                result: tx,
            })?;
        Ok(PublicationTargetPending {
            receiver,
            cancelled,
            result: Some(result),
            received: false,
        })
    }
}
fn retained_validation_bytes(result: &FileOperationResult) -> Option<usize> {
    if result.source_fences.len() > 2 || result.root_bindings.len() != 1 {
        return None;
    }
    let EffectOutcome::Applied { evidence } = &result.outcome else {
        return None;
    };
    let proof = evidence.validated_staging.as_ref()?;
    let mut bytes = std::mem::size_of::<FileOperationResult>()
        .checked_add(proof.stable_identity.capacity())?
        .checked_add(
            result
                .source_fences
                .capacity()
                .checked_mul(std::mem::size_of::<SourceFence>())?,
        )?
        .checked_add(
            result
                .root_bindings
                .capacity()
                .checked_mul(std::mem::size_of::<RootBindingObservation>())?,
        )?;
    for fence in &result.source_fences {
        bytes = bytes.checked_add(fence.dynamic_backing_bytes()?)?;
    }
    for root in &result.root_bindings {
        bytes = bytes.checked_add(root.locator.0.capacity())?;
        if let AuthorityFence::Source(fence) = &root.authority {
            bytes = bytes.checked_add(fence.dynamic_backing_bytes()?)?;
        }
    }
    Some(bytes)
}
impl WorkerState {
    /// Check the original weak registry and retained capabilities on their owning worker.
    /// This is pre-effect validation; it never qualifies a staged object for mutation.
    pub(super) fn validate_original_publication_target(
        &self,
        target: &PreparedPublicationTarget,
        shared: &Arc<OwnerShared>,
    ) -> Result<(), BindingError> {
        if !target.is_current() || !Weak::ptr_eq(&target.data.shared, &Arc::downgrade(shared)) {
            return Err(BindingError::Closed);
        }
        let key = EffectKey {
            operation_id: target.validation().operation_id(),
            participant_id: target.validation().participant_id(),
            effect_id: target.validation().effect_id(),
        };
        let original = self
            .publication_bindings
            .get(&key)
            .and_then(Weak::upgrade)
            .ok_or(BindingError::Missing)?;
        if !Arc::ptr_eq(&original, &target.data) {
            return Err(BindingError::Conflict);
        }
        let command = target
            .publication_command()
            .map_err(|_| BindingError::Invalid)?;
        for (_, binding) in command.effect.role_bindings() {
            let root = self
                .registrations
                .get(&binding.registration)
                .ok_or(BindingError::Missing)?;
            if root.authority != binding.authority || !root.alive.load(Ordering::Acquire) {
                return Err(BindingError::Closed);
            }
            root.capability
                .as_ref()
                .ok_or(BindingError::Unknown)?
                .validate()
                .map_err(|_| BindingError::Closed)?;
        }
        let EffectOutcome::Applied { evidence } = target.validation().outcome() else {
            return Err(BindingError::Invalid);
        };
        if let Some(source) = evidence
            .validated_staging()
            .and_then(|proof| proof.source_root())
        {
            self.registrations
                .get(&source)
                .and_then(|root| root.capability.as_ref())
                .ok_or(BindingError::Missing)?
                .validate()
                .map_err(|_| BindingError::Closed)?;
        }
        let root = self
            .registrations
            .get(&target.target().registration)
            .and_then(|root| root.capability.as_ref())
            .ok_or(BindingError::Missing)?;
        target.revalidate_held_staging()?;
        let target_directory = root
            .root
            .try_clone_descriptor()
            .map_err(|_| BindingError::Unknown)?;
        if crate::durable_capacity::descriptor_capacity_facts(&target_directory)
            .map_err(|_| BindingError::Unknown)?
            .identity
            != target.data.volume
        {
            return Err(BindingError::Conflict);
        }
        match root
            .root
            .symlink_metadata(Path::new(target.target().locator.as_str()))
        {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Ok(_) => Err(BindingError::Occupied),
            Err(_) => Err(BindingError::Unknown),
        }
    }
    pub(super) fn prepare_publication_target(
        &mut self,
        request: BindingRequest,
        shared: &Arc<OwnerShared>,
    ) -> Result<Arc<BindingData>, BindingError> {
        let check = || {
            if request.cancelled.load(Ordering::Acquire) {
                return Err(BindingError::Cancelled);
            }
            if Instant::now() >= request.deadline {
                return Err(BindingError::Deadline);
            }
            shared.ensure_accepting().map_err(|_| BindingError::Closed)
        };
        check()?;
        let key = EffectKey {
            operation_id: request.validation.operation_id,
            participant_id: request.validation.participant_id,
            effect_id: request.validation.effect_id,
        };
        let (canonical, original) = self
            .receipts
            .get(&key)
            .map(|r| (&r.canonical, &r.result))
            .or_else(|| self.tombstones.get(&key).map(|r| (&r.canonical, &r.result)))
            .ok_or(BindingError::Missing)?;
        // A copied/decoded receipt cannot manufacture original reply custody.
        if !Arc::ptr_eq(original, &request.validation) || canonical.len() > HEADER {
            return Err(BindingError::Invalid);
        }
        let staging = &request.validation.root_bindings[0];
        if staging.role != RootRole::Staging {
            return Err(BindingError::Invalid);
        }
        let EffectOutcome::Applied { evidence } = &request.validation.outcome else {
            return Err(BindingError::Invalid);
        };
        let proof = evidence
            .validated_staging
            .as_ref()
            .ok_or(BindingError::Invalid)?;
        let stage_root = self
            .registrations
            .get(&staging.registration)
            .ok_or(BindingError::Missing)?;
        let target_root = self
            .registrations
            .get(&request.target)
            .ok_or(BindingError::Missing)?;
        let stage_cap = stage_root
            .capability
            .as_ref()
            .ok_or(BindingError::Unknown)?;
        let target_cap = target_root
            .capability
            .as_ref()
            .ok_or(BindingError::Unknown)?;
        let target = RootRoleBinding {
            registration: request.target,
            authority: target_root.authority.clone(),
            locator: BoundedRelativeLocator::new(request.leaf.as_str())
                .map_err(|_| BindingError::Invalid)?,
            expected: ExpectedObject::Absent,
        };
        if let AuthorityFence::Source(fence) = &target.authority
            && !request.validation.source_fences.contains(fence)
        {
            return Err(BindingError::Invalid);
        }
        let canonical_binding = serde_json::to_vec(&(
            request.validation.operation_id,
            request.validation.participant_id,
            request.validation.effect_id,
            request.validation.fingerprint.as_bytes(),
            request.validation.source_fences.as_slice(),
            staging.registration,
            staging.authority.clone(),
            staging.locator.as_str(),
            proof.output,
            proof.stable_identity.as_str(),
            proof.bytes,
            proof.content_hash,
            &target,
            request.effect,
            "same-volume/no-replace/v1",
        ))
        .map_err(|_| BindingError::Invalid)?;
        if canonical_binding.len() > HEADER {
            return Err(BindingError::Full);
        }
        let fingerprint = RequestFingerprint(*blake3::hash(&canonical_binding).as_bytes());
        self.publication_bindings
            .retain(|_, data| data.strong_count() != 0);
        let existing = self.publication_bindings.get(&key).and_then(Weak::upgrade);
        if let Some(existing) = existing.as_ref() {
            if Instant::now() >= existing.deadline {
                return Err(BindingError::Deadline);
            }
            if existing.fingerprint != fingerprint {
                return Err(BindingError::Conflict);
            }
            if !existing
                .lives
                .iter()
                .all(|life| life.load(Ordering::Acquire))
            {
                return Err(BindingError::Closed);
            }
        } else {
            if self.publication_bindings.len() >= MAX_BINDINGS {
                return Err(BindingError::Full);
            }
            if self.publication_binding_header.is_none() {
                self.publication_binding_header = Some(
                    shared
                        .reserve_input(REGISTRY_BYTES)
                        .map_err(|_| BindingError::Full)?,
                );
            }
        }
        if let Some(data) = existing.as_ref() {
            data.revalidate_held_staging()?;
        }
        let deadline = existing
            .as_ref()
            .map_or(request.deadline, |data| data.deadline.min(request.deadline));
        let check = || {
            if request.cancelled.load(Ordering::Acquire) {
                return Err(BindingError::Cancelled);
            }
            if Instant::now() >= deadline {
                return Err(BindingError::Deadline);
            }
            shared.ensure_accepting().map_err(|_| BindingError::Closed)
        };
        let mut lives = vec![stage_root.alive.clone(), target_root.alive.clone()];
        if let Some(source) = proof.source_root {
            let root = self
                .registrations
                .get(&source)
                .ok_or(BindingError::Missing)?;
            root.capability
                .as_ref()
                .ok_or(BindingError::Unknown)?
                .validate()
                .map_err(|_| BindingError::Closed)?;
            lives.push(root.alive.clone());
        }
        stage_cap.validate().map_err(|_| BindingError::Closed)?;
        target_cap.validate().map_err(|_| BindingError::Closed)?;
        match target_cap
            .root
            .symlink_metadata(Path::new(target.locator.as_str()))
        {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Ok(_) => return Err(BindingError::Occupied),
            Err(_) => return Err(BindingError::Unknown),
        }
        // Nonblocking no-follow regular-file observation, including FIFO substitution refusal.
        let file = stage_cap
            .root
            .open_observation_file_nofollow(Path::new(staging.locator.as_str()))
            .map_err(|_| BindingError::Unknown)?;
        // Keep the original-object read/share lease through hashing and every live binding.
        // This refuses file-handle write/delete aliases, not surviving writable mappings.
        #[cfg(windows)]
        let mut file = windows_staging_share::retain(file).map_err(|_| BindingError::Unknown)?;
        #[cfg(not(windows))]
        let mut file = file;
        let metadata = file.metadata().map_err(|_| BindingError::Unknown)?;
        if !metadata.is_file()
            || metadata.len() != proof.bytes
            || stable_filesystem_identity_from_open_file(&file).as_deref()
                != Some(proof.stable_identity.as_str())
        {
            return Err(BindingError::Conflict);
        }
        #[cfg(unix)]
        if std::os::unix::fs::MetadataExt::nlink(&metadata) != 1 {
            return Err(BindingError::Conflict);
        }
        let mut hasher = blake3::Hasher::new();
        let mut scratch = [0u8; 8192];
        let mut bytes = 0u64;
        loop {
            check()?;
            let count = file.read(&mut scratch).map_err(|_| BindingError::Unknown)?;
            if count == 0 {
                break;
            }
            bytes = bytes
                .checked_add(count as u64)
                .filter(|n| *n <= proof.bytes)
                .ok_or(BindingError::Conflict)?;
            hasher.update(&scratch[..count]);
        }
        if bytes != proof.bytes || *hasher.finalize().as_bytes() != proof.content_hash {
            return Err(BindingError::Conflict);
        }
        let volume = crate::durable_capacity::descriptor_capacity_facts(&file)
            .map_err(|_| BindingError::Unknown)?
            .identity;
        let target_directory = target_cap
            .root
            .try_clone_descriptor()
            .map_err(|_| BindingError::Unknown)?;
        let target_volume = crate::durable_capacity::descriptor_capacity_facts(&target_directory)
            .map_err(|_| BindingError::Unknown)?
            .identity;
        if target_volume != volume {
            return Err(BindingError::Invalid);
        }
        if file.metadata().map_err(|_| BindingError::Unknown)?.len() != proof.bytes
            || stable_filesystem_identity_from_open_file(&file).as_deref()
                != Some(proof.stable_identity.as_str())
        {
            return Err(BindingError::Conflict);
        }
        stage_cap.validate().map_err(|_| BindingError::Closed)?;
        target_cap.validate().map_err(|_| BindingError::Closed)?;
        if let Some(source) = proof.source_root {
            self.registrations
                .get(&source)
                .and_then(|root| root.capability.as_ref())
                .ok_or(BindingError::Missing)?
                .validate()
                .map_err(|_| BindingError::Closed)?;
        }
        #[cfg(unix)]
        if std::os::unix::fs::MetadataExt::nlink(
            &file.metadata().map_err(|_| BindingError::Unknown)?,
        ) != 1
        {
            return Err(BindingError::Conflict);
        }
        match target_cap
            .root
            .symlink_metadata(Path::new(target.locator.as_str()))
        {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Ok(_) => return Err(BindingError::Occupied),
            Err(_) => return Err(BindingError::Unknown),
        }
        check()?;
        if let Some(existing) = existing {
            return Ok(existing);
        }
        let data = Arc::new(BindingData {
            validation: request.validation,
            target,
            effect: request.effect,
            fingerprint,
            deadline: request.deadline,
            staged_file: file,
            volume,
            lives,
            shared: Arc::downgrade(shared),
            _input: request.input,
        });
        self.publication_bindings.insert(key, Arc::downgrade(&data));
        Ok(data)
    }
}
#[cfg(test)]
mod tests;
