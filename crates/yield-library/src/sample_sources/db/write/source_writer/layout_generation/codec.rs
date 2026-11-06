//! Fixed-order, length-delimited v1 format. No optional extension/unknown field lane.
use super::{
    SOURCE_LAYOUT_CANONICAL_MAX_BYTES, SourceLayoutComputedView,
    SourceLayoutGenerationError as Error,
};
use crate::sample_sources::db::write::{SOURCE_LAYOUT_ALGORITHM_V1, SourceLayoutCapture};
use std::path::Path;
const MAGIC: &[u8] = b"YSLG\x01\0\0\0";
const MAX_ROWS: usize = 256;
const MAX_MANIFEST: usize = 128 * 1024;

/// One borrowed, strictly decoded historical row; no current target/key authority.
#[derive(Debug)]
pub struct SourceLayoutGenerationRow<'a> {
    path: &'a str,
    file_identity: &'a str,
    content_hash: &'a str,
    feature_reference: &'a str,
    size: u64,
    modified_ns: i64,
    coordinates: [f32; 2],
    cluster_id: Option<i32>,
}
impl<'a> SourceLayoutGenerationRow<'a> {
    /// Exact source-relative manifest spelling.
    pub fn path(&self) -> &'a str {
        self.path
    }
    /// Historical file identity, not a live filesystem capability.
    pub fn file_identity(&self) -> &'a str {
        self.file_identity
    }
    /// Complete observed content hash.
    pub fn content_hash(&self) -> &'a str {
        self.content_hash
    }
    /// Opaque historical features-v1 reference, never reconstructed key authority.
    pub fn feature_reference(&self) -> &'a str {
        self.feature_reference
    }
    /// Historical manifest byte length.
    pub fn size(&self) -> u64 {
        self.size
    }
    /// Historical timestamp.
    pub fn modified_ns(&self) -> i64 {
        self.modified_ns
    }
    /// Raw finite coordinates; presentation normalization is separate.
    pub fn coordinates(&self) -> [f32; 2] {
        self.coordinates
    }
    /// Optional existing-engine label.
    pub fn cluster_id(&self) -> Option<i32> {
        self.cluster_id
    }
}
/// Strict bounded historical observation borrowing canonical bytes.
/// Row capacity is checked before allocation. Strings/root borrow the <=512KiB
/// input; path normalization uses bounded <=1024-byte temporaries. No recursion
/// or attacker-sized string allocation exists.
#[derive(Debug)]
pub struct SourceLayoutGeneration<'a> {
    source_id: &'a str,
    root: &'a [u8],
    root_binding: &'a str,
    database: &'a str,
    origin_revision: u64,
    input_digest: [u8; 32],
    cpu_digest: [u8; 32],
    point_digest: [u8; 32],
    rows: Vec<SourceLayoutGenerationRow<'a>>,
}
impl<'a> SourceLayoutGeneration<'a> {
    /// Decode exact v1 canonical bytes with bounded borrowed fields and digest checks.
    /// Successful decode does not validate a lease, current membership or owner.
    pub fn decode(bytes: &'a [u8]) -> Result<Self, Error> {
        if bytes.len() > SOURCE_LAYOUT_CANONICAL_MAX_BYTES {
            return Err(Error::Limit("encoded bytes"));
        }
        let mut reader = Reader { bytes, at: 0 };
        if reader.take(MAGIC.len())? != MAGIC {
            return Err(Error::Invalid("format version"));
        }
        let source_id = reader.text(256)?;
        let root = reader.field(4096)?;
        let root_binding = reader.text(512)?;
        let database = reader.text(512)?;
        if source_id.trim().is_empty()
            || root.is_empty()
            || root_binding.trim().is_empty()
            || database.trim().is_empty()
        {
            return Err(Error::Invalid("physical source identity"));
        }
        let origin_revision = reader.u64()?;
        if origin_revision > i64::MAX as u64 {
            return Err(Error::Invalid("historical origin revision"));
        }
        if reader.text(128)? != SOURCE_LAYOUT_ALGORITHM_V1 || reader.text(32)? != "features-v1" {
            return Err(Error::Invalid("model/version/parameters"));
        }
        let input_digest = reader.digest()?;
        let cpu_digest = reader.digest()?;
        let point_digest = reader.digest()?;
        let count = reader.u32()? as usize;
        if count == 0 || count > MAX_ROWS {
            return Err(Error::Limit("complete row count"));
        }
        // Reject truncated row blocks before allocating even the bounded row vector.
        if reader.bytes.len() - reader.at < count * 41 + 32 {
            return Err(Error::Invalid("truncated rows"));
        }
        let mut rows: Vec<SourceLayoutGenerationRow<'a>> = Vec::with_capacity(count);
        let mut aggregate = 0usize;
        for _ in 0..count {
            let path = reader.text(1024)?;
            let file_identity = reader.text(512)?;
            let content_hash = reader.text(64)?;
            let feature_reference = reader.text(160)?;
            let size = reader.u64()?;
            let modified_ns = i64::from_le_bytes(reader.take(8)?.try_into().unwrap());
            let coordinates = [reader.f32()?, reader.f32()?];
            let cluster_id = match reader.take(1)?[0] {
                0 => None,
                1 => Some(i32::from_le_bytes(reader.take(4)?.try_into().unwrap())),
                _ => return Err(Error::Invalid("cluster tag")),
            };
            validate_point(coordinates, cluster_id, count)?;
            if size > i64::MAX as u64
                || file_identity.trim().is_empty()
                || !full_hash(content_hash)
                || !valid_reference(feature_reference, source_id, file_identity, content_hash)
                || path.contains(['\\', '\0'])
                || crate::sample_sources::db::util::normalize_relative_path(Path::new(path))
                    .ok()
                    .as_deref()
                    != Some(path)
                || !crate::sample_sources::is_supported_audio(Path::new(path))
                || rows.last().is_some_and(|row| row.path >= path)
                || rows.iter().any(|row| row.file_identity == file_identity)
            {
                return Err(Error::Invalid("ordered manifest facts"));
            }
            aggregate = aggregate
                .checked_add(path.len() + file_identity.len() + content_hash.len())
                .filter(|v| *v <= MAX_MANIFEST)
                .ok_or(Error::Limit("manifest identity bytes"))?;
            rows.push(SourceLayoutGenerationRow {
                path,
                file_identity,
                content_hash,
                feature_reference,
                size,
                modified_ns,
                coordinates,
                cluster_id,
            });
        }
        let checksum_offset = reader.at;
        let checksum = reader.digest()?;
        if reader.at != bytes.len() {
            return Err(Error::Invalid("trailing or unknown fields"));
        }
        if blake3::hash(&bytes[..checksum_offset]).as_bytes() != &checksum {
            return Err(Error::Invalid("encoding checksum"));
        }
        let value = Self {
            source_id,
            root,
            root_binding,
            database,
            origin_revision,
            input_digest,
            cpu_digest,
            point_digest,
            rows,
        };
        if value.manifest_digest() != input_digest {
            return Err(Error::Invalid("manifest digest"));
        }
        if value.computed_point_digest() != point_digest {
            return Err(Error::Invalid("point digest"));
        }
        Ok(value)
    }
    /// Historical source ID; cannot select/acquire an owner.
    pub fn source_id(&self) -> &'a str {
        self.source_id
    }
    /// Exact raw physical root identity bytes.
    pub fn root_identity(&self) -> &'a [u8] {
        self.root
    }
    /// Historical root-binding observation.
    pub fn root_binding_identity(&self) -> &'a str {
        self.root_binding
    }
    /// Historical database-file identity.
    pub fn database_identity(&self) -> &'a str {
        self.database
    }
    /// Historical originating semantic revision, never current liveness.
    pub fn origin_revision(&self) -> u64 {
        self.origin_revision
    }
    /// Complete ordered historical rows.
    pub fn rows(&self) -> &[SourceLayoutGenerationRow<'a>] {
        &self.rows
    }
    /// Fixed model/projection/cluster policy.
    pub fn algorithm_version(&self) -> &'static str {
        SOURCE_LAYOUT_ALGORITHM_V1
    }
    /// Full physical/membership input digest recomputed by decode.
    pub fn input_digest(&self) -> &[u8; 32] {
        &self.input_digest
    }
    /// CPU feature fingerprint validated by the live preparation producer.
    /// Archived decode cannot authenticate payloads without current feature lookup.
    pub fn feature_input_digest(&self) -> &[u8; 32] {
        &self.cpu_digest
    }
    /// Exact point/cluster digest recomputed by decode.
    pub fn point_digest(&self) -> &[u8; 32] {
        &self.point_digest
    }
    fn manifest_digest(&self) -> [u8; 32] {
        let mut hash = super::super::layout_capture::layout_input_hasher(
            [
                self.source_id.as_bytes(),
                self.root,
                self.root_binding.as_bytes(),
                self.database.as_bytes(),
            ],
            self.rows.len(),
        );
        for row in &self.rows {
            for field in [
                row.path.as_bytes(),
                row.file_identity.as_bytes(),
                row.content_hash.as_bytes(),
                b"features-v1".as_slice(),
                row.feature_reference.as_bytes(),
                &row.size.to_le_bytes(),
                &row.modified_ns.to_le_bytes(),
            ] {
                hash_field(&mut hash, field);
            }
        }
        *hash.finalize().as_bytes()
    }
    fn computed_point_digest(&self) -> [u8; 32] {
        point_digest(
            &self.input_digest,
            &self.cpu_digest,
            self.rows.len(),
            self.rows
                .iter()
                .map(|r| (r.path, r.coordinates, r.cluster_id)),
        )
    }
}
pub(super) fn validate_point(
    coordinates: [f32; 2],
    cluster: Option<i32>,
    count: usize,
) -> Result<(), Error> {
    if !coordinates.iter().all(|v| v.is_finite()) {
        return Err(Error::Invalid("finite coordinates"));
    }
    if (count < 5 && cluster.is_some())
        || (count >= 5 && cluster.is_none())
        || cluster.is_some_and(|v| v < 0)
    {
        return Err(Error::Invalid("cluster policy"));
    }
    Ok(())
}
fn full_hash(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|v| v.is_ascii_digit() || (b'a'..=b'f').contains(&v))
}
fn valid_reference(value: &str, source: &str, file: &str, content: &str) -> bool {
    let Some((target, payload)) = value
        .strip_prefix("source-feature-v1:")
        .and_then(|v| v.split_once(':'))
    else {
        return false;
    };
    if !full_hash(target) || !full_hash(payload) {
        return false;
    }
    // Same stable target framing as FeatureArtifactKey, but only an archived consistency
    // check: neither this digest nor the persisted payload hash creates key authority.
    let mut hash = blake3::Hasher::new();
    for field in [source, file, content, "features-v1"] {
        hash.update(&(field.len() as u32).to_le_bytes());
        hash.update(field.as_bytes());
    }
    hash.finalize().to_hex().as_str() == target
}

fn hash_field(hash: &mut blake3::Hasher, bytes: &[u8]) {
    hash.update(&(bytes.len() as u64).to_le_bytes());
    hash.update(bytes);
}
fn point_digest<'a>(
    input: &[u8; 32],
    cpu: &[u8; 32],
    count: usize,
    rows: impl Iterator<Item = (&'a str, [f32; 2], Option<i32>)>,
) -> [u8; 32] {
    let mut hash = blake3::Hasher::new();
    for field in [
        b"yield-source-layout-points-v1".as_slice(),
        SOURCE_LAYOUT_ALGORITHM_V1.as_bytes(),
        input.as_slice(),
        cpu.as_slice(),
        &(count as u64).to_le_bytes(),
    ] {
        hash_field(&mut hash, field);
    }
    for (path, coordinates, cluster) in rows {
        hash_field(&mut hash, path.as_bytes());
        for value in coordinates {
            hash.update(&value.to_bits().to_le_bytes());
        }
        match cluster {
            None => {
                hash.update(&[0]);
            }
            Some(value) => {
                hash.update(&[1]);
                hash.update(&value.to_le_bytes());
            }
        }
    }
    *hash.finalize().as_bytes()
}
pub(super) fn encode(
    capture: &SourceLayoutCapture,
    computed: &SourceLayoutComputedView<'_>,
    cpu: &[u8; 32],
) -> Result<Vec<u8>, Error> {
    let scope = capture.revision();
    let fields = [
        scope.source_id().as_str().as_bytes(),
        scope.root_identity().as_bytes(),
        scope.root_binding_identity().as_bytes(),
        scope.database_identity().as_bytes(),
        SOURCE_LAYOUT_ALGORITHM_V1.as_bytes(),
        b"features-v1".as_slice(),
    ];
    for (field, max) in fields.iter().zip([256, 4096, 512, 512, 128, 32]) {
        if field.len() > max {
            return Err(Error::Limit("scope field bytes"));
        }
    }
    let mut bound = MAGIC.len() + 8 + 96 + 4 + 32;
    for field in fields {
        bound = add(bound, 4 + field.len())?;
    }
    for row in capture.features() {
        bound = add(
            bound,
            4 * 4
                + row.path().len()
                + row.file_identity().len()
                + row.content_hash().len()
                + row.key().reference().len()
                + 16
                + 8
                + 5,
        )?;
    }
    // Conservative optional cluster size checked before any encoded allocation.
    let points = point_digest(
        capture.input_digest(),
        cpu,
        capture.features().len(),
        capture
            .features()
            .iter()
            .zip(computed.points)
            .map(|(r, p)| (r.path(), p.coordinates, p.cluster_id)),
    );
    let mut bytes = Vec::with_capacity(bound);
    bytes.extend_from_slice(MAGIC);
    for field in &fields[..4] {
        put_field(&mut bytes, field);
    }
    bytes.extend_from_slice(&scope.revision().to_le_bytes());
    for field in &fields[4..] {
        put_field(&mut bytes, field);
    }
    bytes.extend_from_slice(capture.input_digest());
    bytes.extend_from_slice(cpu);
    bytes.extend_from_slice(&points);
    bytes.extend_from_slice(&(capture.features().len() as u32).to_le_bytes());
    for (row, point) in capture.features().iter().zip(computed.points) {
        for field in [
            row.path(),
            row.file_identity(),
            row.content_hash(),
            row.key().reference(),
        ] {
            put_field(&mut bytes, field.as_bytes());
        }
        bytes.extend_from_slice(&row.size().to_le_bytes());
        bytes.extend_from_slice(&row.modified_ns().to_le_bytes());
        for value in point.coordinates {
            bytes.extend_from_slice(&value.to_bits().to_le_bytes());
        }
        match point.cluster_id {
            None => bytes.push(0),
            Some(value) => {
                bytes.push(1);
                bytes.extend_from_slice(&value.to_le_bytes());
            }
        }
    }
    let checksum = *blake3::hash(&bytes).as_bytes();
    bytes.extend_from_slice(&checksum);
    debug_assert!(bytes.len() <= bound);
    Ok(bytes)
}
fn add(total: usize, extra: usize) -> Result<usize, Error> {
    total
        .checked_add(extra)
        .filter(|v| *v <= SOURCE_LAYOUT_CANONICAL_MAX_BYTES)
        .ok_or(Error::Limit("encoded bytes"))
}
fn put_field(bytes: &mut Vec<u8>, field: &[u8]) {
    bytes.extend_from_slice(&(field.len() as u32).to_le_bytes());
    bytes.extend_from_slice(field);
}
struct Reader<'a> {
    bytes: &'a [u8],
    at: usize,
}
impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8], Error> {
        let end = self
            .at
            .checked_add(n)
            .filter(|v| *v <= self.bytes.len())
            .ok_or(Error::Invalid("truncated bytes"))?;
        let result = &self.bytes[self.at..end];
        self.at = end;
        Ok(result)
    }
    fn u32(&mut self) -> Result<u32, Error> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into().unwrap()))
    }
    fn u64(&mut self) -> Result<u64, Error> {
        Ok(u64::from_le_bytes(self.take(8)?.try_into().unwrap()))
    }
    fn f32(&mut self) -> Result<f32, Error> {
        Ok(f32::from_bits(self.u32()?))
    }
    fn digest(&mut self) -> Result<[u8; 32], Error> {
        Ok(self.take(32)?.try_into().unwrap())
    }
    fn field(&mut self, max: usize) -> Result<&'a [u8], Error> {
        let n = self.u32()? as usize;
        if n > max {
            return Err(Error::Limit("field bytes"));
        }
        self.take(n)
    }
    fn text(&mut self, max: usize) -> Result<&'a str, Error> {
        std::str::from_utf8(self.field(max)?).map_err(|_| Error::Invalid("UTF8 field"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn layout_generation_codec_bounds_before_allocation_and_preserves_exact_escaped_utf8() {
        assert!(add(SOURCE_LAYOUT_CANONICAL_MAX_BYTES, 1).is_err());
        assert!(add(usize::MAX, 1).is_err());
        let value = "quoted-\"-\\-\n-\t-é";
        let mut bytes = Vec::new();
        put_field(&mut bytes, value.as_bytes());
        let mut reader = Reader {
            bytes: &bytes,
            at: 0,
        };
        assert_eq!(reader.text(value.len()).unwrap(), value);
        let mut reader = Reader {
            bytes: &bytes,
            at: 0,
        };
        assert!(reader.text(value.len() - 1).is_err());
        let prefix = u32::MAX.to_le_bytes();
        let mut reader = Reader {
            bytes: &prefix,
            at: 0,
        };
        assert!(matches!(
            reader.field(1024),
            Err(Error::Limit("field bytes"))
        ));
        let malformed = [1, 0, 0, 0, 255];
        let mut reader = Reader {
            bytes: &malformed,
            at: 0,
        };
        assert!(reader.text(1).is_err());
    }
    #[test]
    fn layout_generation_codec_frozen_clusters_and_finite_bits_fail_closed() {
        for count in [1, 2, 4] {
            assert!(validate_point([0.0, -0.0], None, count).is_ok());
            assert!(validate_point([0.0, 1.0], Some(0), count).is_err());
        }
        for count in [5, 256] {
            assert!(validate_point([0.0, 1.0], Some(0), count).is_ok());
            assert!(validate_point([0.0, 1.0], None, count).is_err());
            assert!(validate_point([0.0, 1.0], Some(-1), count).is_err());
        }
        for v in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
            assert!(validate_point([v, 0.0], None, 2).is_err());
        }
    }
}
