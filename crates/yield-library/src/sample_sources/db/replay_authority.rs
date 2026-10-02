//! Source-owned durable watcher authority for replay admission.

use serde::de::{self, Deserializer, MapAccess, SeqAccess, Visitor};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::fmt;

use crate::sample_sources::reconciliation::{
    BackendStreamIdentity, BootstrapReplayEvidence, CaptureBoundary, ReplayPriorToken,
    RetainedUncertainty, RetainedUncertaintyBoundary, RootIdentity, UncertaintyReason,
    WatcherContinuityProof, WatcherGeneration,
};
use crate::sample_sources::{SourceDatabase, SourceId};

use super::{META_SOURCE_WATCHER_CHECKPOINT, META_SOURCE_WATCHER_UNCERTAINTY, SourceDbError};

const UNCERTAINTY_FORMAT_VERSION: u8 = 1;
const MAX_UNCERTAINTY_BYTES: usize = 16 * 1024;

/// One bounded, conservative source-wide audit marker retained until a complete audit.
///
/// A single source-wide marker is sufficient to prevent replay from granting authority. Later
/// markers widen its bounded evidence and move its revision watermark forward, so an audit that
/// completed before later uncertainty cannot clear it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DurableReplayUncertainty {
    source_id: SourceId,
    root_identity: RootIdentity,
    lifecycle_generation: u64,
    source_revision: u64,
    watcher_generation: Option<WatcherGeneration>,
    backend_stream_identity: Option<BackendStreamIdentity>,
    capture_boundary: Option<CaptureBoundary>,
    retained_boundary: RetainedUncertaintyBoundary,
    reasons: Vec<UncertaintyReason>,
}

impl DurableReplayUncertainty {
    /// Borrow the source that must be audited.
    pub fn source_id(&self) -> &SourceId {
        &self.source_id
    }
    /// Borrow the physical root that was active when uncertainty was retained.
    pub fn root_identity(&self) -> &RootIdentity {
        &self.root_identity
    }
    /// Return the source lifecycle that retained the marker.
    pub const fn lifecycle_generation(&self) -> u64 {
        self.lifecycle_generation
    }
    /// Return the latest committed source revision at which uncertainty was retained.
    pub const fn source_revision(&self) -> u64 {
        self.source_revision
    }
    /// Return the watcher generation when it was known.
    pub const fn watcher_generation(&self) -> Option<WatcherGeneration> {
        self.watcher_generation
    }
    /// Borrow the captured backend stream identity when it was known.
    pub fn backend_stream_identity(&self) -> Option<&BackendStreamIdentity> {
        self.backend_stream_identity.as_ref()
    }
    /// Return the original capture boundary when it was known.
    pub const fn capture_boundary(&self) -> Option<CaptureBoundary> {
        self.capture_boundary
    }
    /// Return the supervisor's retained uncertainty watermark.
    pub const fn retained_boundary(&self) -> RetainedUncertaintyBoundary {
        self.retained_boundary
    }
    /// Borrow the bounded reasons retained from unresolved markers.
    pub fn reasons(&self) -> &[UncertaintyReason] {
        &self.reasons
    }

    pub(super) fn from_marker(
        marker: &RetainedUncertainty,
        source_id: &SourceId,
        root_identity: &RootIdentity,
        lifecycle_generation: u64,
        source_revision: u64,
    ) -> Option<Self> {
        if marker.source_id() != Some(source_id)
            || marker
                .root_identity()
                .is_some_and(|observed| observed != root_identity)
            || lifecycle_generation == 0
            || source_revision == 0
            || marker.generation() == Some(WatcherGeneration::new(0))
            || marker.reasons().is_empty()
            || marker.retained_boundary().first() > marker.retained_boundary().through()
        {
            return None;
        }
        Some(Self {
            source_id: source_id.clone(),
            root_identity: root_identity.clone(),
            lifecycle_generation,
            source_revision,
            watcher_generation: marker.generation(),
            backend_stream_identity: marker.backend_stream_identity().cloned(),
            capture_boundary: marker.capture_boundary(),
            retained_boundary: marker.retained_boundary(),
            reasons: marker.reasons().to_vec(),
        })
    }

    pub(super) fn merged_with(mut self, newer: &Self) -> Option<Self> {
        if self.source_id != newer.source_id || self.root_identity != newer.root_identity {
            return None;
        }
        self.lifecycle_generation = newer.lifecycle_generation;
        self.source_revision = self.source_revision.max(newer.source_revision);
        if self.watcher_generation != newer.watcher_generation {
            self.watcher_generation = None;
        }
        if self.backend_stream_identity != newer.backend_stream_identity {
            self.backend_stream_identity = None;
        }
        if self.capture_boundary != newer.capture_boundary {
            self.capture_boundary = None;
        }
        self.retained_boundary = RetainedUncertaintyBoundary::new(
            self.retained_boundary
                .first()
                .min(newer.retained_boundary.first()),
            self.retained_boundary
                .through()
                .max(newer.retained_boundary.through()),
        );
        self.reasons.extend_from_slice(&newer.reasons);
        self.reasons.sort_unstable();
        self.reasons.dedup();
        Some(self)
    }

    pub(super) fn encode(&self) -> Option<String> {
        if self.source_id.as_str().len() > 256
            || self.root_identity.as_bytes().len() > 2048
            || self
                .backend_stream_identity
                .as_ref()
                .is_some_and(|stream| stream.as_bytes().len() > 128)
            || self.reasons.len() > 32
        {
            return None;
        }
        let capture = self.capture_boundary.map(|boundary| StoredCaptureBoundary {
            captured_at: boundary.captured_at(),
            first_sequence: boundary.first_sequence(),
            last_sequence: boundary.last_sequence(),
        });
        let stored = StoredReplayUncertainty {
            format_version: UNCERTAINTY_FORMAT_VERSION,
            source_id: self.source_id.as_str().to_owned(),
            root_identity: self.root_identity.as_bytes().to_vec(),
            lifecycle_generation: self.lifecycle_generation,
            source_revision: self.source_revision,
            watcher_generation: self.watcher_generation.map(WatcherGeneration::get),
            backend_stream_identity: self
                .backend_stream_identity
                .as_ref()
                .map(|stream| stream.as_bytes().to_vec()),
            capture_boundary: capture,
            retained_first: self.retained_boundary.first(),
            retained_through: self.retained_boundary.through(),
            scope: "source_audit".to_owned(),
            reasons: self.reasons.clone(),
        };
        let encoded = serde_json::to_string(&stored).ok()?;
        (encoded.len() <= MAX_UNCERTAINTY_BYTES).then_some(encoded)
    }

    pub(super) fn decode(value: &str) -> Option<Self> {
        if value.len() > MAX_UNCERTAINTY_BYTES {
            return None;
        }
        let stored: StoredReplayUncertainty = serde_json::from_str(value).ok()?;
        if stored.format_version != UNCERTAINTY_FORMAT_VERSION
            || stored.source_id.is_empty()
            || stored.source_id.len() > 256
            || stored.root_identity.is_empty()
            || stored.root_identity.len() > 2048
            || stored.lifecycle_generation == 0
            || stored.source_revision == 0
            || stored.watcher_generation == Some(0)
            || stored
                .backend_stream_identity
                .as_ref()
                .is_some_and(|stream| stream.len() > 128)
            || stored.retained_first > stored.retained_through
            || stored.scope != "source_audit"
            || stored.reasons.is_empty()
            || stored.reasons.len() > 32
        {
            return None;
        }
        let capture_boundary = match stored.capture_boundary {
            Some(boundary) => Some(
                CaptureBoundary::try_new(
                    boundary.captured_at,
                    boundary.first_sequence,
                    boundary.last_sequence,
                )
                .ok()?,
            ),
            None => None,
        };
        Some(Self {
            source_id: SourceId::from_string(stored.source_id),
            root_identity: RootIdentity::from_bytes(stored.root_identity),
            lifecycle_generation: stored.lifecycle_generation,
            source_revision: stored.source_revision,
            watcher_generation: stored.watcher_generation.map(WatcherGeneration::new),
            backend_stream_identity: stored
                .backend_stream_identity
                .map(BackendStreamIdentity::from_bytes),
            capture_boundary,
            retained_boundary: RetainedUncertaintyBoundary::new(
                stored.retained_first,
                stored.retained_through,
            ),
            reasons: stored.reasons,
        })
    }
}

/// Read outcome for a source-owned replay uncertainty marker.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DurableReplayUncertaintyState {
    /// No unresolved source-owned marker is stored.
    Absent,
    /// A valid source-owned marker was reconstructed.
    Retained(DurableReplayUncertainty),
    /// Unknown or malformed durable state still forbids replay authority.
    Invalid,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredCaptureBoundary {
    captured_at: u64,
    first_sequence: Option<u64>,
    last_sequence: Option<u64>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredReplayUncertainty {
    format_version: u8,
    source_id: String,
    root_identity: Vec<u8>,
    lifecycle_generation: u64,
    source_revision: u64,
    watcher_generation: Option<u64>,
    backend_stream_identity: Option<Vec<u8>>,
    capture_boundary: Option<StoredCaptureBoundary>,
    retained_first: u64,
    retained_through: u64,
    scope: String,
    reasons: Vec<UncertaintyReason>,
}

const CHECKPOINT_FORMAT_VERSION: u64 = 4;
const CHECKPOINT_FIELDS: [&str; 8] = [
    "root_identity",
    "event_id",
    "format_version",
    "source_id",
    "lifecycle_generation",
    "source_revision",
    "cause",
    "continuity_proof",
];
const CONTINUITY_PROOF_FIELDS: [&str; 8] = [
    "root_identity",
    "backend",
    "backend_device",
    "backend_volume_uuid",
    "watcher_generation",
    "replay_coverage_start_event_id",
    "replay_coverage_end_event_id",
    "acknowledged_end_event_id",
];
const BOOTSTRAP_PROOF_FIELDS: [&str; 10] = [
    "root_identity",
    "backend",
    "backend_device",
    "backend_volume_uuid",
    "watcher_generation",
    "stream_start_event_id",
    "stream_end_event_id",
    "source_audit_revision",
    "history_complete",
    "changed_source_paths",
];

/// A construction capability that can only be created after this module validates DB metadata.
pub(crate) struct ValidatedReplayAuthority {
    source_id: SourceId,
    root_identity: RootIdentity,
    backend_stream_identity: BackendStreamIdentity,
    watcher_generation: WatcherGeneration,
    acknowledged_sequence: u64,
}

impl ValidatedReplayAuthority {
    fn new(
        source_id: SourceId,
        root_identity: RootIdentity,
        backend_stream_identity: BackendStreamIdentity,
        watcher_generation: WatcherGeneration,
        acknowledged_sequence: u64,
    ) -> Self {
        Self {
            source_id,
            root_identity,
            backend_stream_identity,
            watcher_generation,
            acknowledged_sequence,
        }
    }

    pub(crate) fn into_parts(
        self,
    ) -> (
        SourceId,
        RootIdentity,
        BackendStreamIdentity,
        WatcherGeneration,
        u64,
    ) {
        (
            self.source_id,
            self.root_identity,
            self.backend_stream_identity,
            self.watcher_generation,
            self.acknowledged_sequence,
        )
    }
}

/// Opaque persisted replay outcome, read under source/root/lifecycle validation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DurableReplayReceipt {
    prior: ReplayPriorToken,
    source_revision: u64,
    replay_first: Option<u64>,
}
impl DurableReplayReceipt {
    /// Validated durable checkpoint authority; not native or filesystem-effect permission.
    pub fn prior(&self) -> &ReplayPriorToken {
        &self.prior
    }
    /// Exact source revision covered by this checkpoint.
    pub const fn source_revision(&self) -> u64 {
        self.source_revision
    }
    /// Match an exact replay interval, or an exact-terminal bootstrap audit coverage.
    /// Changed-path evidence is not reconstructed from a cursor or returned as a new delta.
    pub fn matches_interval(&self, first: u64, last: u64) -> bool {
        first != 0
            && first <= last
            && last == self.prior.acknowledged_sequence()
            && self.replay_first.is_none_or(|stored| stored == first)
    }
}

/// Read durable replay authority from the source database's existing watcher checkpoint.
impl SourceDatabase {
    /// Read the bounded source-owned uncertainty that requires an authoritative audit.
    /// Malformed state remains an explicit blocking outcome instead of disappearing.
    pub fn read_durable_replay_uncertainty(
        &self,
    ) -> Result<DurableReplayUncertaintyState, SourceDbError> {
        let Some(value) = self.get_metadata(META_SOURCE_WATCHER_UNCERTAINTY)? else {
            return Ok(DurableReplayUncertaintyState::Absent);
        };
        let Some(marker) = DurableReplayUncertainty::decode(&value) else {
            return Ok(DurableReplayUncertaintyState::Invalid);
        };
        if marker.source_revision() > self.get_revision()? {
            return Ok(DurableReplayUncertaintyState::Invalid);
        }
        Ok(DurableReplayUncertaintyState::Retained(marker))
    }

    /// Return opaque replay authority only when the stored checkpoint matches this source lane.
    ///
    /// The checkpoint parser is intentionally strict and fail-closed. In particular, legacy,
    /// proofless, malformed, unknown, duplicated, or identity-mismatched metadata returns
    /// `Ok(None)`, while database access failures retain their original `SourceDbError`.
    /// `expected_source_lifecycle_generation` belongs to the persisted source lifecycle; the
    /// separate `owner_watcher_generation` is the current in-memory reconciliation-owner lane
    /// generation that the returned opaque authority must bind to.
    pub fn read_durable_replay_prior(
        &self,
        expected_source_id: &SourceId,
        expected_root_identity: &RootIdentity,
        expected_source_lifecycle_generation: WatcherGeneration,
        owner_watcher_generation: WatcherGeneration,
    ) -> Result<Option<ReplayPriorToken>, SourceDbError> {
        Ok(self
            .read_durable_replay_receipt(
                expected_source_id,
                expected_root_identity,
                expected_source_lifecycle_generation,
                owner_watcher_generation,
            )?
            .map(|receipt| receipt.prior))
    }

    /// Read a validated, bounded receipt for retry after a lost acknowledgement or restart.
    /// Uncertainty/malformed metadata and unknown or future source revisions fail closed.
    pub fn read_durable_replay_receipt(
        &self,
        expected_source_id: &SourceId,
        expected_root_identity: &RootIdentity,
        expected_source_lifecycle_generation: WatcherGeneration,
        owner_watcher_generation: WatcherGeneration,
    ) -> Result<Option<DurableReplayReceipt>, SourceDbError> {
        // Any unresolved or malformed marker blocks replay, including after a restart.
        if self
            .get_metadata(META_SOURCE_WATCHER_UNCERTAINTY)?
            .is_some()
        {
            return Ok(None);
        }
        let Some(value) = self.get_metadata(META_SOURCE_WATCHER_CHECKPOINT)? else {
            return Ok(None);
        };
        if value.len() > 16 * 1024 {
            return Ok(None);
        }
        // A checkpoint cannot claim coverage of a source revision that has not committed.
        // Later unrelated source writes may advance the revision without invalidating the
        // watcher cursor, so require an existing revision rather than exact equality.
        let committed_revision = self.get_revision()?;
        let checkpoint_revision = parse_object(&value)
            .ok()
            .and_then(|object| u64_field(&object, "source_revision"));
        if !checkpoint_revision
            .is_some_and(|revision| revision != 0 && revision <= committed_revision)
        {
            return Ok(None);
        }
        let Some(prior) = parse_durable_replay_prior(
            &value,
            expected_source_id,
            expected_root_identity,
            expected_source_lifecycle_generation,
            owner_watcher_generation,
        ) else {
            return Ok(None);
        };
        let object = parse_object(&value).map_err(|_| SourceDbError::Unexpected)?;
        let replay_first = if string_field(&object, "cause") == Some("targeted_replay") {
            object
                .get("continuity_proof")
                .and_then(Value::as_object)
                .and_then(|proof| u64_field(proof, "replay_coverage_start_event_id"))
        } else {
            None
        };
        Ok(Some(DurableReplayReceipt {
            prior,
            source_revision: checkpoint_revision.expect("validated committed revision"),
            replay_first,
        }))
    }
}

pub(super) fn parse_durable_replay_prior(
    value: &str,
    expected_source_id: &SourceId,
    expected_root_identity: &RootIdentity,
    expected_source_lifecycle_generation: WatcherGeneration,
    owner_watcher_generation: WatcherGeneration,
) -> Option<ReplayPriorToken> {
    let object = parse_object(value).ok()?;
    if !has_exact_fields(&object, &CHECKPOINT_FIELDS) {
        return None;
    }

    let root_identity = string_field(&object, "root_identity")?;
    let event_id = u64_field(&object, "event_id")?;
    if u64_field(&object, "format_version")? != CHECKPOINT_FORMAT_VERSION
        || string_field(&object, "source_id")? != expected_source_id.as_str()
        || root_identity.as_bytes() != expected_root_identity.as_bytes()
        || u64_field(&object, "lifecycle_generation")? != expected_source_lifecycle_generation.get()
        || u64_field(&object, "source_revision").is_none()
    {
        return None;
    }

    let proof = object.get("continuity_proof")?.as_object()?;
    let cause = string_field(&object, "cause")?;
    match cause {
        "targeted_replay" if has_exact_fields(proof, &CONTINUITY_PROOF_FIELDS) => {}
        "bootstrap_audit" if has_exact_fields(proof, &BOOTSTRAP_PROOF_FIELDS) => {
            if string_field(proof, "root_identity")? != root_identity
                || u64_field(proof, "stream_start_event_id")?
                    > u64_field(proof, "stream_end_event_id")?
                || u64_field(proof, "stream_end_event_id")? != event_id
                || u64_field(proof, "source_audit_revision")?
                    != u64_field(&object, "source_revision")?
                || proof.get("history_complete")?.as_bool()? != true
                || u64_field(proof, "changed_source_paths")? != 0
            {
                return None;
            }
        }
        _ => return None,
    }
    let proof_root_identity = string_field(proof, "root_identity")?;
    let backend = string_field(proof, "backend")?;
    let backend_device = u64_field(proof, "backend_device")?;
    let volume_uuid = uuid::Uuid::parse_str(string_field(proof, "backend_volume_uuid")?).ok()?;
    let (replay_start, replay_end, acknowledged_end) = if cause == "targeted_replay" {
        (
            u64_field(proof, "replay_coverage_start_event_id")?,
            u64_field(proof, "replay_coverage_end_event_id")?,
            u64_field(proof, "acknowledged_end_event_id")?,
        )
    } else {
        (
            u64_field(proof, "stream_start_event_id")?,
            u64_field(proof, "stream_end_event_id")?,
            event_id,
        )
    };
    if proof_root_identity != root_identity
        || backend != "fsevents"
        || backend_device == 0
        || u64_field(proof, "watcher_generation")? == 0
        || replay_start > replay_end
        || replay_end != acknowledged_end
        || acknowledged_end != event_id
    {
        return None;
    }

    // A numeric macOS device ID is not stable across reboot or volume replacement. The native
    // replay stream must supply the same volume UUID; a stale or absent UUID cannot authorize it.
    let stream_identity =
        BackendStreamIdentity::from_fsevents_volume(backend_device, *volume_uuid.as_bytes())?;
    Some(ReplayPriorToken::from_validated_durable_authority(
        ValidatedReplayAuthority::new(
            expected_source_id.clone(),
            expected_root_identity.clone(),
            stream_identity,
            owner_watcher_generation,
            event_id,
        ),
    ))
}

/// Encode only a continuity proof admitted by reconciliation for the retained source writer.
pub(super) fn encode_replay_checkpoint(
    proof: &WatcherContinuityProof,
    source_revision: u64,
    lifecycle_generation: u64,
) -> Option<String> {
    let stream = proof.backend_stream_identity().as_bytes();
    let payload = stream.strip_prefix(b"fsevents-volume:")?;
    if payload.len() != 24 {
        return None;
    }
    let device = u64::from_be_bytes(payload[..8].try_into().ok()?);
    let uuid = uuid::Uuid::from_slice(&payload[8..]).ok()?;
    let root = std::str::from_utf8(proof.root_identity().as_bytes()).ok()?;
    let coverage = proof.replay_coverage();
    if device == 0 || uuid.is_nil() || lifecycle_generation == 0 {
        return None;
    }
    Some(
        serde_json::json!({
            "root_identity": root,
            "event_id": coverage.through_sequence(),
            "format_version": CHECKPOINT_FORMAT_VERSION,
            "source_id": proof.source_id().as_str(),
            "lifecycle_generation": lifecycle_generation,
            "source_revision": source_revision,
            "cause": "targeted_replay",
            "continuity_proof": {
                "root_identity": root,
                "backend": "fsevents",
                "backend_device": device,
                "backend_volume_uuid": uuid.to_string(),
                "watcher_generation": proof.watcher_generation().get(),
                "replay_coverage_start_event_id": coverage.after_sequence().checked_add(1)?,
                "replay_coverage_end_event_id": coverage.through_sequence(),
                "acknowledged_end_event_id": coverage.through_sequence(),
            }
        })
        .to_string(),
    )
}

/// Encode a point-in-time replay prior only from a complete audit and a certain live capture.
pub(super) fn encode_bootstrap_checkpoint(
    evidence: &BootstrapReplayEvidence,
    source_revision: u64,
) -> Option<String> {
    let stream = evidence.stream_identity().as_bytes();
    let payload = stream.strip_prefix(b"fsevents-volume:")?;
    if payload.len() != 24 || source_revision == 0 {
        return None;
    }
    let device = u64::from_be_bytes(payload[..8].try_into().ok()?);
    let uuid = uuid::Uuid::from_slice(&payload[8..]).ok()?;
    let root = std::str::from_utf8(evidence.root_identity().as_bytes()).ok()?;
    if device == 0 || uuid.is_nil() || evidence.lifecycle_generation() == 0 {
        return None;
    }
    Some(
        serde_json::json!({
            "root_identity": root,
            "event_id": evidence.through_sequence(),
            "format_version": CHECKPOINT_FORMAT_VERSION,
            "source_id": evidence.source_id().as_str(),
            "lifecycle_generation": evidence.lifecycle_generation(),
            "source_revision": source_revision,
            "cause": "bootstrap_audit",
            "continuity_proof": {
                "root_identity": root,
                "backend": "fsevents",
                "backend_device": device,
                "backend_volume_uuid": uuid.to_string(),
                "watcher_generation": evidence.watcher_generation().get(),
                "stream_start_event_id": evidence.started_after(),
                "stream_end_event_id": evidence.through_sequence(),
                "source_audit_revision": source_revision,
                "history_complete": true,
                "changed_source_paths": 0,
            }
        })
        .to_string(),
    )
}

struct StrictJsonValue(Value);

impl<'de> Deserialize<'de> for StrictJsonValue {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserializer.deserialize_any(StrictJsonValueVisitor)
    }
}

struct StrictJsonValueVisitor;

impl<'de> Visitor<'de> for StrictJsonValueVisitor {
    type Value = StrictJsonValue;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("a JSON value without duplicate object fields")
    }

    fn visit_bool<E>(self, value: bool) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        Ok(StrictJsonValue(Value::Bool(value)))
    }

    fn visit_i64<E>(self, value: i64) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        Ok(StrictJsonValue(Value::Number(value.into())))
    }

    fn visit_u64<E>(self, value: u64) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        Ok(StrictJsonValue(Value::Number(value.into())))
    }

    fn visit_f64<E>(self, value: f64) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        let number = serde_json::Number::from_f64(value)
            .ok_or_else(|| E::custom("non-finite JSON number"))?;
        Ok(StrictJsonValue(Value::Number(number)))
    }

    fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        Ok(StrictJsonValue(Value::String(value.to_owned())))
    }

    fn visit_string<E>(self, value: String) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        Ok(StrictJsonValue(Value::String(value)))
    }

    fn visit_none<E>(self) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        Ok(StrictJsonValue(Value::Null))
    }

    fn visit_unit<E>(self) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        Ok(StrictJsonValue(Value::Null))
    }

    fn visit_some<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserializer.deserialize_any(StrictJsonValueVisitor)
    }

    fn visit_seq<A>(self, mut sequence: A) -> Result<Self::Value, A::Error>
    where
        A: SeqAccess<'de>,
    {
        let mut values = Vec::new();
        while let Some(value) = sequence.next_element::<StrictJsonValue>()? {
            values.push(value.0);
        }
        Ok(StrictJsonValue(Value::Array(values)))
    }

    fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
    where
        A: MapAccess<'de>,
    {
        let mut object = Map::new();
        while let Some(key) = map.next_key::<String>()? {
            if object.contains_key(&key) {
                return Err(de::Error::custom("duplicate checkpoint field"));
            }
            object.insert(key, map.next_value::<StrictJsonValue>()?.0);
        }
        Ok(StrictJsonValue(Value::Object(object)))
    }
}

fn parse_object(value: &str) -> Result<Map<String, Value>, String> {
    let StrictJsonValue(value) =
        serde_json::from_str::<StrictJsonValue>(value).map_err(|error| error.to_string())?;
    value
        .as_object()
        .cloned()
        .ok_or_else(|| String::from("checkpoint must be an object"))
}

fn has_exact_fields(object: &Map<String, Value>, fields: &[&str]) -> bool {
    object.len() == fields.len() && fields.iter().all(|field| object.contains_key(*field))
}

fn string_field<'a>(object: &'a Map<String, Value>, field: &str) -> Option<&'a str> {
    object.get(field)?.as_str()
}

fn u64_field(object: &Map<String, Value>, field: &str) -> Option<u64> {
    object.get(field)?.as_u64()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn root() -> RootIdentity {
        RootIdentity::from_bytes(b"root-a".to_vec())
    }

    fn source_id() -> SourceId {
        SourceId::from_string("source-a")
    }

    fn checkpoint() -> String {
        serde_json::json!({
            "root_identity": "root-a",
            "event_id": 17,
            "format_version": 4,
            "source_id": "source-a",
            "lifecycle_generation": 4,
            "source_revision": 12,
            "cause": "targeted_replay",
            "continuity_proof": {
                "root_identity": "root-a",
                "backend": "fsevents",
                "backend_device": 99,
                "backend_volume_uuid": "550e8400-e29b-41d4-a716-446655440000",
                "watcher_generation": 23,
                "replay_coverage_start_event_id": 7,
                "replay_coverage_end_event_id": 17,
                "acknowledged_end_event_id": 17
            }
        })
        .to_string()
    }

    fn bootstrap_checkpoint() -> Value {
        serde_json::json!({
            "root_identity": "root-a",
            "event_id": 17,
            "format_version": 4,
            "source_id": "source-a",
            "lifecycle_generation": 4,
            "source_revision": 12,
            "cause": "bootstrap_audit",
            "continuity_proof": {
                "root_identity": "root-a",
                "backend": "fsevents",
                "backend_device": 99,
                "backend_volume_uuid": "550e8400-e29b-41d4-a716-446655440000",
                "watcher_generation": 23,
                "stream_start_event_id": 7,
                "stream_end_event_id": 17,
                "source_audit_revision": 12,
                "history_complete": true,
                "changed_source_paths": 0
            }
        })
    }

    fn read_with_generations(
        value: &str,
        source_lifecycle_generation: u64,
        owner_watcher_generation: u64,
    ) -> Option<ReplayPriorToken> {
        let directory = tempdir().expect("source root");
        let database = SourceDatabase::open_for_test_fixture_source_write(directory.path())
            .expect("source database");
        database
            .set_metadata("revision", "12")
            .expect("committed fixture revision");
        database
            .set_metadata(META_SOURCE_WATCHER_CHECKPOINT, value)
            .expect("checkpoint metadata");
        database
            .read_durable_replay_prior(
                &source_id(),
                &root(),
                WatcherGeneration::new(source_lifecycle_generation),
                WatcherGeneration::new(owner_watcher_generation),
            )
            .expect("authority read")
    }

    fn read(value: &str) -> Option<ReplayPriorToken> {
        read_with_generations(value, 4, 4)
    }

    #[test]
    fn valid_checkpoint_produces_durable_opaque_authority() {
        let token = read_with_generations(&checkpoint(), 4, 9).expect("valid token");
        assert_eq!(token.source_id(), &source_id());
        assert_eq!(token.root_identity(), &root());
        assert_eq!(token.watcher_generation(), WatcherGeneration::new(9));
        assert_eq!(token.acknowledged_sequence(), 17);
        let uuid =
            uuid::Uuid::parse_str("550e8400-e29b-41d4-a716-446655440000").expect("fixture UUID");
        assert_eq!(
            token.backend_stream_identity(),
            &BackendStreamIdentity::from_fsevents_volume(99, *uuid.as_bytes())
                .expect("volume stream")
        );
    }

    #[test]
    fn reopened_database_reconstructs_the_same_authority() {
        let directory = tempdir().expect("source root");
        {
            let database = SourceDatabase::open_for_test_fixture_source_write(directory.path())
                .expect("source database");
            database
                .set_metadata("revision", "12")
                .expect("committed fixture revision");
            database
                .set_metadata(META_SOURCE_WATCHER_CHECKPOINT, &checkpoint())
                .expect("checkpoint metadata");
        }
        let database = SourceDatabase::open_for_test_fixture_source_write(directory.path())
            .expect("reopened source database");
        let token = database
            .read_durable_replay_prior(
                &source_id(),
                &root(),
                WatcherGeneration::new(4),
                WatcherGeneration::new(4),
            )
            .expect("authority read")
            .expect("valid token");
        assert_eq!(token.acknowledged_sequence(), 17);
    }

    #[test]
    fn checkpoint_cannot_claim_an_uncommitted_source_revision() {
        let directory = tempdir().expect("source root");
        let database = SourceDatabase::open_for_test_fixture_source_write(directory.path())
            .expect("source database");
        let write_metadata = |key: &str, value: &str| {
            let mut batch = database.write_batch().expect("auxiliary batch");
            batch.set_metadata(key, value).expect("metadata write");
            batch.commit_auxiliary_state().expect("auxiliary commit");
        };
        write_metadata("revision", "11");
        write_metadata(META_SOURCE_WATCHER_CHECKPOINT, &checkpoint());
        let read = || {
            database
                .read_durable_replay_prior(
                    &source_id(),
                    &root(),
                    WatcherGeneration::new(4),
                    WatcherGeneration::new(4),
                )
                .expect("authority read")
        };
        assert_eq!(read(), None, "future revision grants no replay authority");
        write_metadata("revision", "12");
        assert!(read().is_some(), "committed revision grants authority");
        write_metadata("revision", "13");
        assert!(read().is_some(), "later writes do not rewind coverage");
        let zero = checkpoint().replace("\"source_revision\":12", "\"source_revision\":0");
        write_metadata(META_SOURCE_WATCHER_CHECKPOINT, &zero);
        assert_eq!(read(), None, "zero revision grants no replay authority");
    }

    #[test]
    fn malformed_uncertainty_still_blocks_a_valid_replay_prior() {
        let directory = tempdir().expect("source root");
        let database = SourceDatabase::open_for_test_fixture_source_write(directory.path())
            .expect("source database");
        database
            .set_metadata("revision", "12")
            .expect("committed fixture revision");
        database
            .set_metadata(META_SOURCE_WATCHER_CHECKPOINT, &checkpoint())
            .expect("checkpoint metadata");
        let read = || {
            database
                .read_durable_replay_prior(
                    &source_id(),
                    &root(),
                    WatcherGeneration::new(4),
                    WatcherGeneration::new(4),
                )
                .expect("authority read")
        };
        assert!(read().is_some(), "valid checkpoint starts usable");
        database
            .set_metadata(META_SOURCE_WATCHER_UNCERTAINTY, "not-json")
            .expect("malformed marker");
        assert_eq!(
            database
                .read_durable_replay_uncertainty()
                .expect("uncertainty read"),
            DurableReplayUncertaintyState::Invalid,
        );
        assert_eq!(read(), None, "malformed marker cannot be ignored");
    }

    #[test]
    fn same_numeric_device_with_another_volume_uuid_is_a_different_stream() {
        let original = read(&checkpoint()).expect("original volume token");
        let replacement = checkpoint().replace(
            "550e8400-e29b-41d4-a716-446655440000",
            "550e8400-e29b-41d4-a716-446655440001",
        );
        let replacement = read(&replacement).expect("replacement volume token");
        assert_ne!(
            original.backend_stream_identity(),
            replacement.backend_stream_identity()
        );
    }

    #[test]
    fn legacy_v3_proofless_and_invalid_checkpoints_fail_closed() {
        let old_proof = checkpoint().replace("\"format_version\":4", "\"format_version\":3");
        let values = [
            r#"{"root_identity":"root-a","event_id":17}"#,
            r#"{"root_identity":"root-a","event_id":17,"format_version":2,"source_id":"source-a","lifecycle_generation":4,"source_revision":12,"cause":"targeted_replay"}"#,
            r#"{"root_identity":"root-a","event_id":17,"format_version":3,"source_id":"source-a","lifecycle_generation":4,"source_revision":12,"cause":"targeted_replay","continuity_proof":null}"#,
            old_proof.as_str(),
            "not-json",
        ];
        for value in values {
            assert_eq!(read(value), None, "value should fail closed: {value}");
        }
    }

    #[test]
    fn duplicate_unknown_and_mismatched_checkpoint_fields_fail_closed() {
        let cases = [
            format!(
                "{}{}",
                checkpoint().trim_end_matches('}'),
                ",\"event_id\":17}"
            ),
            checkpoint().replace(
                "\"cause\":\"targeted_replay\"",
                "\"cause\":\"completed_fallback_audit\"",
            ),
            checkpoint().replace("\"backend\":\"fsevents\"", "\"backend\":\"notify\""),
            checkpoint().replace("\"backend_device\":99", "\"backend_device\":0"),
            checkpoint().replace(
                ",\"backend_volume_uuid\":\"550e8400-e29b-41d4-a716-446655440000\"",
                "",
            ),
            checkpoint().replace(
                "\"backend_volume_uuid\":\"550e8400-e29b-41d4-a716-446655440000\"",
                "\"backend_volume_uuid\":\"00000000-0000-0000-0000-000000000000\"",
            ),
            checkpoint().replace(
                "\"backend_volume_uuid\":\"550e8400-e29b-41d4-a716-446655440000\"",
                "\"backend_volume_uuid\":\"malformed\"",
            ),
            checkpoint().replace(
                "\"replay_coverage_start_event_id\":7",
                "\"replay_coverage_start_event_id\":18",
            ),
            checkpoint().replace("\"source-a\"", "\"other-source\""),
            checkpoint().replace("\"root-a\"", "\"other-root\""),
            checkpoint().replace("\"lifecycle_generation\":4", "\"lifecycle_generation\":5"),
            checkpoint().replace(
                "\"continuity_proof\":{",
                "\"continuity_proof\":{\"unknown\":true,",
            ),
            checkpoint().replace(
                "\"continuity_proof\":{",
                "\"continuity_proof\":{\"backend\":\"notify\",",
            ),
        ];
        for value in cases {
            assert_eq!(read(&value), None, "value should fail closed: {value}");
        }
    }

    #[test]
    fn bootstrap_checkpoint_requires_exact_audit_and_gap_free_coverage_fields() {
        let valid = bootstrap_checkpoint();
        assert_eq!(
            read(&valid.to_string()).unwrap().acknowledged_sequence(),
            17
        );
        let mut cases = Vec::new();
        for (field, value) in [
            ("history_complete", serde_json::json!(false)),
            ("changed_source_paths", serde_json::json!(1)),
            ("stream_start_event_id", serde_json::json!(18)),
            ("stream_end_event_id", serde_json::json!(16)),
            ("source_audit_revision", serde_json::json!(11)),
            ("backend_volume_uuid", serde_json::json!("bad-uuid")),
        ] {
            let mut invalid = valid.clone();
            invalid["continuity_proof"][field] = value;
            cases.push(invalid);
        }
        let mut missing = valid.clone();
        missing["continuity_proof"]
            .as_object_mut()
            .unwrap()
            .remove("history_complete");
        cases.push(missing);
        let mut extra = valid.clone();
        extra["continuity_proof"]["unknown"] = serde_json::json!(true);
        cases.push(extra);
        let mut old = valid;
        old["format_version"] = serde_json::json!(3);
        cases.push(old);
        for value in cases {
            assert_eq!(read(&value.to_string()), None, "invalid bootstrap: {value}");
        }
    }
}
