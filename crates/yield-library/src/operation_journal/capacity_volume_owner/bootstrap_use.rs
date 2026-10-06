//! Closed original source proofs for protected-reserve bootstrap, not ordinary effect grants.
use crate::sample_sources::db::{
    CurrentReserveUseProof, LegacyReserveUseProof, SourceEffectPermit,
};

pub(crate) enum ReserveBootstrapUseProof<'scope, 'source> {
    Current(&'scope CurrentReserveUseProof<'source>),
    Legacy(&'scope LegacyReserveUseProof<'source>),
}
impl ReserveBootstrapUseProof<'_, '_> {
    pub(crate) fn validate_for_bootstrap(&self) -> Result<(), ()> {
        match self {
            Self::Current(scope) => super::validate_current_source_scope(scope).map_err(|_| ()),
            Self::Legacy(scope) => scope.validate_for_bootstrap().map_err(|_| ()),
        }
    }

    /// Pin original Current source custody while native validation and an effect run.
    /// The gate mutex protects only scalar admission; all native work runs after it is released.
    pub(crate) fn begin_effect(&self) -> Result<Option<SourceEffectPermit>, ()> {
        let pin = match self {
            Self::Current(scope) => Some(scope.begin_bootstrap_effect().map_err(|_| ())?),
            Self::Legacy(_) => None,
        };
        self.validate_for_bootstrap()?;
        Ok(pin)
    }
}
