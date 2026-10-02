//! Read-only prerequisite diagnostics using the existing exact feature validators.
use super::*;

pub(in crate::sample_sources::readiness::store) fn lookup_layout_target(
    connection: &Connection,
    ownership: &ReadinessOwnership<'_>,
    target: &super::super::super::ReadinessTarget,
) -> Result<(FeatureArtifactKey, FeatureArtifactPayload), ReadinessError> {
    ownership.validate_artifact_read_only(connection)?;
    let probe = FeatureArtifactKey::for_payload(
        target,
        &FeatureArtifactPayload::from_v1_vector(&[0.0; 183])?,
    )?;
    if !current_target(connection, &probe)? {
        return Err(ReadinessError::InvalidFeatureArtifact(
            "layout feature target unavailable or unsupported",
        ));
    }
    let result = match health(connection, target)? {
        Health::Healthy(key, payload) if marker_matches(connection, &key)? => Ok((key, payload)),
        Health::Healthy(..) => Err(ReadinessError::InvalidFeatureArtifact(
            "layout feature completion missing",
        )),
        Health::Missing => Err(ReadinessError::InvalidFeatureArtifact(
            "layout feature payload missing",
        )),
        Health::CorruptOwned => Err(ReadinessError::InvalidFeatureArtifact(
            "layout feature payload corrupt",
        )),
        Health::Ambiguous => Err(ReadinessError::InvalidFeatureArtifact(
            "layout feature provenance ambiguous",
        )),
    };
    ownership.validate_artifact_read_only(connection)?;
    result
}
