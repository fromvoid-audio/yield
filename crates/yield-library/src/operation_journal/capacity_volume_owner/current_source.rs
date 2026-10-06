//! Correlate only genuine original Current descriptors before any acquisition effect.
use super::*;
use crate::sample_sources::db::CurrentReserveUseProof;

pub(in super::super) fn validate_current_source_scope(
    scope: &CurrentReserveUseProof<'_>,
) -> Result<(), VolumeOwnershipError> {
    scope
        .validate_for_bootstrap()
        .map_err(|_| VolumeOwnershipError::VolumeIdentityUnavailable)?;
    #[cfg(target_os = "macos")]
    {
        let files = scope.native_volume_descriptors();
        let observe = |file| {
            observe_mac_volume(file).map(|record| NativeScopeRecord {
                device: record.device,
                uuid: record.uuid,
            })
        };
        validate_native_scope([
            observe(files[0])?,
            observe(files[1])?,
            observe(files[2])?,
            observe(files[3])?,
        ])?;
    }
    // Linux/Windows obtain no qualification from this validation. Their original ownership
    // acquisition and reserve physical gates remain unchanged and fail closed.
    scope
        .validate_for_bootstrap()
        .map_err(|_| VolumeOwnershipError::VolumeIdentityUnavailable)
}

#[cfg(any(target_os = "macos", test))]
#[derive(Clone, Copy, Eq, PartialEq)]
struct NativeScopeRecord {
    device: u64,
    uuid: [u8; 16],
}

#[cfg(any(target_os = "macos", test))]
fn validate_native_scope(records: [NativeScopeRecord; 4]) -> Result<(), VolumeOwnershipError> {
    if records.iter().any(|r| r.device == 0 || r.uuid == [0; 16])
        || records[0] != records[1]
        || records.iter().any(|a| {
            records
                .iter()
                .any(|b| (a.device == b.device) != (a.uuid == b.uuid))
        })
    {
        return Err(VolumeOwnershipError::VolumeIdentityUnavailable);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn current_source_native_scope_requires_main_root_and_exact_alias_correlation() {
        let a = NativeScopeRecord {
            device: 1,
            uuid: [1; 16],
        };
        let b = NativeScopeRecord {
            device: 2,
            uuid: [2; 16],
        };
        assert!(validate_native_scope([a, a, b, b]).is_ok());
        assert!(validate_native_scope([a; 4]).is_ok());
        assert!(validate_native_scope([b, a, b, a]).is_err());
        let foreign_alias = NativeScopeRecord {
            device: 1,
            uuid: [3; 16],
        };
        assert!(validate_native_scope([a, a, foreign_alias, a]).is_err());
        assert!(validate_native_scope([a, a, NativeScopeRecord { device: 2, ..a }, a]).is_err());
        assert!(validate_native_scope([a, a, NativeScopeRecord { device: 0, ..b }, a]).is_err());
        assert!(
            validate_native_scope([a, a, NativeScopeRecord { uuid: [0; 16], ..b }, a]).is_err()
        );
    }
}
