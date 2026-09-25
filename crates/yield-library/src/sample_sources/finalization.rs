//! Renderer-neutral platform policy for staged file finalization.
//!
//! The capability query describes only whether the platform exposes the
//! primitive used by the production finalizer. It does not qualify a complete
//! edit workflow, transaction owner, volume, or durability sequence.

use std::fmt;

/// Stable reason for an unavailable staged-finalization primitive.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StagedFinalizationUnsupportedReason {
    /// The platform has no qualified handle-bound atomic no-replace claim.
    FinalNoReplacePrimitiveUnavailable,
    /// The platform has no independently qualified handle-bound cleanup operation.
    SafeDispositionUnavailable,
}

impl StagedFinalizationUnsupportedReason {
    /// Return the stable diagnostic code for this reason.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::FinalNoReplacePrimitiveUnavailable => "FinalNoReplacePrimitiveUnavailable",
            Self::SafeDispositionUnavailable => "SafeDispositionUnavailable",
        }
    }
}

impl fmt::Display for StagedFinalizationUnsupportedReason {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// Availability of one staged-finalization primitive.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StagedFinalizationAvailability {
    /// The platform exposes the primitive. This does not qualify the complete
    /// workflow, ownership contract, or durability requirements.
    Available,
    /// The primitive is unavailable or cannot qualify the ownership contract.
    Unsupported(StagedFinalizationUnsupportedReason),
}

impl StagedFinalizationAvailability {
    /// Whether the primitive is available for the platform policy.
    pub const fn is_available(self) -> bool {
        matches!(self, Self::Available)
    }

    /// Return the stable unsupported reason, if the primitive is unavailable.
    pub const fn unsupported_reason(self) -> Option<StagedFinalizationUnsupportedReason> {
        match self {
            Self::Available => None,
            Self::Unsupported(reason) => Some(reason),
        }
    }
}

/// Independent platform availability results for staged publication and cleanup.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StagedFileFinalizationCapabilities {
    /// Availability of the exclusive staged-to-target publication primitive.
    pub publication: StagedFinalizationAvailability,
    /// Availability of the staged-file cleanup/disposition primitive.
    pub cleanup: StagedFinalizationAvailability,
}

/// Query the side-effect-free platform policy for staged publication and cleanup.
///
/// `Available` means that the corresponding low-level primitive exists. It is
/// not a certification of every filesystem, transaction, owner, or durability
/// invariant required by a complete workflow.
pub const fn staged_file_finalization_capabilities() -> StagedFileFinalizationCapabilities {
    if cfg!(windows) {
        StagedFileFinalizationCapabilities {
            publication: StagedFinalizationAvailability::Available,
            cleanup: StagedFinalizationAvailability::Available,
        }
    } else {
        StagedFileFinalizationCapabilities {
            publication: StagedFinalizationAvailability::Unsupported(
                StagedFinalizationUnsupportedReason::FinalNoReplacePrimitiveUnavailable,
            ),
            cleanup: StagedFinalizationAvailability::Unsupported(
                StagedFinalizationUnsupportedReason::SafeDispositionUnavailable,
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capability_query_reports_independent_operations() {
        let capabilities = staged_file_finalization_capabilities();

        #[cfg(windows)]
        assert_eq!(
            capabilities,
            StagedFileFinalizationCapabilities {
                publication: StagedFinalizationAvailability::Available,
                cleanup: StagedFinalizationAvailability::Available,
            }
        );

        #[cfg(not(windows))]
        assert_eq!(
            capabilities,
            StagedFileFinalizationCapabilities {
                publication: StagedFinalizationAvailability::Unsupported(
                    StagedFinalizationUnsupportedReason::FinalNoReplacePrimitiveUnavailable,
                ),
                cleanup: StagedFinalizationAvailability::Unsupported(
                    StagedFinalizationUnsupportedReason::SafeDispositionUnavailable,
                ),
            }
        );
    }

    #[test]
    fn unsupported_reasons_have_stable_diagnostic_codes() {
        assert_eq!(
            StagedFinalizationUnsupportedReason::FinalNoReplacePrimitiveUnavailable.as_str(),
            "FinalNoReplacePrimitiveUnavailable"
        );
        assert_eq!(
            StagedFinalizationUnsupportedReason::SafeDispositionUnavailable.as_str(),
            "SafeDispositionUnavailable"
        );
    }
}
