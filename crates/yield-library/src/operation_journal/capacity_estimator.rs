//! Authority-free requirement and estimator coverage for universal commands.
//!
//! This module describes what a future owner would need to account for.  It never opens a
//! descriptor, reads a path, reserves capacity, or makes a provider available.  A `Known`
//! result is only a checked logical byte bound supplied by the typed command observation; a
//! later descriptor-bound planner still has to derive volume facts, allocation units, and the
//! protected floor before it can build a [`FullCapacityPlan`].

use thiserror::Error;

use super::coordinator::UniversalCommandArgs;
use super::decoded_edit_commands::DecodedEditIntentArgs;
use super::decoded_export_commands::DecodedExportIntentArgs;
use super::full_capacity::FullCapacityAllocationClass;
use super::history_commands::HistoryAction;
use super::model::UniversalCommandKind;
use super::namespace_commands::NamespaceCommandArgs;
use super::removal_commands::RemovalCommandArgs;
use super::render_commands::{
    RenderCommandArgs, RenderCommandValidationError, WaveformRenderOutputSpec,
};
use super::retained_resolution_commands::RetainedResolutionCommandArgs;
use super::selection_output_commands::SelectionOutputCommandArgs;

/// Stable version of the authority-free command/class coverage matrix.
pub const CAPACITY_ESTIMATOR_MATRIX_VERSION: u16 = 1;

/// Every allocation class that a universal estimator must account for or explicitly reject.
pub const FULL_CAPACITY_ALLOCATION_CLASSES: [FullCapacityAllocationClass; 9] = [
    FullCapacityAllocationClass::DestinationStaging,
    FullCapacityAllocationClass::JournalRecord,
    FullCapacityAllocationClass::SourceDatabaseCommit,
    FullCapacityAllocationClass::SourceWalShm,
    FullCapacityAllocationClass::GlobalDatabaseCommit,
    FullCapacityAllocationClass::GlobalWalShm,
    FullCapacityAllocationClass::ExistingDestination,
    FullCapacityAllocationClass::ExistingRecoveryPayload,
    FullCapacityAllocationClass::OperationRecoveryPayload,
];

/// Every command kind in the universal command model, including the argumentless CreateFile
/// kind.  The explicit lists and matches below are deliberate review points for new families.
pub const UNIVERSAL_COMMAND_KINDS: [UniversalCommandKind; 17] = [
    UniversalCommandKind::Extract,
    UniversalCommandKind::Copy,
    UniversalCommandKind::Import,
    UniversalCommandKind::CreateFile,
    UniversalCommandKind::CreateFolder,
    UniversalCommandKind::Duplicate,
    UniversalCommandKind::Export,
    UniversalCommandKind::Rename,
    UniversalCommandKind::Move,
    UniversalCommandKind::Trash,
    UniversalCommandKind::Delete,
    UniversalCommandKind::RestoreRetained,
    UniversalCommandKind::PurgeRetained,
    UniversalCommandKind::DestructiveEdit,
    UniversalCommandKind::Undo,
    UniversalCommandKind::Redo,
    UniversalCommandKind::ExternalReconciliation,
];

/// Why an estimate is not a bounded logical byte value.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CapacityEstimateReason {
    /// A later owner must supply live descriptor-bound facts before this can be planned.
    DescriptorBoundFactsRequired,
    /// The command family has no sound bounded estimator in this milestone.
    FamilyEstimatorNotImplemented,
    /// The command has an output, but its current typed envelope does not expose a safe bound.
    OutputBoundNotExposed,
    /// The command mutates metadata or history without a capacity bound in this matrix.
    MetadataBoundNotDefined,
    /// The command has no new payload allocation in this matrix.
    NoNewPayload,
    /// This command kind has no typed arguments to estimate.
    MissingTypedArguments,
}

/// Typed availability of one logical capacity estimate.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CapacityEstimateDisposition {
    /// A checked, non-zero logical byte bound.  This is not a reservation or authority.
    Known(u64),
    /// A supported shape whose bound is not available without a later authority or contract.
    Unknown(CapacityEstimateReason),
    /// The current milestone does not support estimating this family or shape.
    Unsupported(CapacityEstimateReason),
    /// The class has no allocation for this explicitly understood command shape.
    NotApplicable(CapacityEstimateReason),
}

/// One allocation-class row in the command estimator matrix.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CapacityRequirementEstimate {
    class: FullCapacityAllocationClass,
    disposition: CapacityEstimateDisposition,
}

impl CapacityRequirementEstimate {
    const fn new(
        class: FullCapacityAllocationClass,
        disposition: CapacityEstimateDisposition,
    ) -> Self {
        Self { class, disposition }
    }

    /// Return the allocation class represented by this row.
    pub const fn class(self) -> FullCapacityAllocationClass {
        self.class
    }

    /// Return the typed estimate disposition for this class.
    pub const fn disposition(self) -> CapacityEstimateDisposition {
        self.disposition
    }
}

/// Complete nine-class estimator result for one typed command observation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CapacityRequirementMatrix {
    version: u16,
    command_kind: UniversalCommandKind,
    rows: [CapacityRequirementEstimate; 9],
}

impl CapacityRequirementMatrix {
    /// Return the matrix version.
    pub const fn version(&self) -> u16 {
        self.version
    }

    /// Return the command kind covered by this matrix.
    pub const fn command_kind(&self) -> UniversalCommandKind {
        self.command_kind
    }

    /// Return all nine allocation-class rows in canonical order.
    pub const fn rows(&self) -> &[CapacityRequirementEstimate; 9] {
        &self.rows
    }

    /// Return the row for one allocation class.
    pub fn for_class(&self, class: FullCapacityAllocationClass) -> CapacityRequirementEstimate {
        self.rows
            .iter()
            .copied()
            .find(|row| row.class == class)
            .expect("canonical capacity matrix contains every allocation class")
    }

    /// Extract one positive checked bound, failing closed for every non-`Known` disposition.
    ///
    /// Callers must use this method instead of filtering the rows for known values.  A missing
    /// class, an unknown/unsupported reason, and an explicit non-applicability claim are all
    /// typed failures rather than implicit zero requirements.
    pub fn require_known_bytes(
        &self,
        class: FullCapacityAllocationClass,
    ) -> Result<u64, CapacityRequirementError> {
        let row = self.rows.iter().find(|row| row.class == class).ok_or(
            CapacityRequirementError::MissingClass {
                command_kind: self.command_kind,
                class,
            },
        )?;
        match row.disposition {
            CapacityEstimateDisposition::Known(bytes) if bytes > 0 => Ok(bytes),
            disposition => Err(CapacityRequirementError::Unresolved {
                command_kind: self.command_kind,
                class,
                disposition,
            }),
        }
    }
}

/// A future descriptor-bound planner could not extract an executable byte bound from a matrix.
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum CapacityRequirementError {
    /// The fixed matrix did not contain the requested allocation class.
    #[error("{command_kind:?} matrix is missing allocation class {class:?}")]
    MissingClass {
        /// Command kind covered by the matrix.
        command_kind: UniversalCommandKind,
        /// Allocation class requested by the caller.
        class: FullCapacityAllocationClass,
    },
    /// The row was present but did not provide a positive checked bound.
    #[error("{command_kind:?} allocation class {class:?} is unresolved: {disposition:?}")]
    Unresolved {
        /// Command kind covered by the matrix.
        command_kind: UniversalCommandKind,
        /// Allocation class requested by the caller.
        class: FullCapacityAllocationClass,
        /// The explicit unresolved disposition retained by the matrix.
        disposition: CapacityEstimateDisposition,
    },
}

/// Failure while deriving a typed estimate from malformed command observations.
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum CapacityEstimatorError {
    /// A known byte field was zero and therefore cannot be an executable allocation bound.
    #[error("{command_kind:?} supplied a zero capacity estimate for {field}")]
    ZeroEstimate {
        /// Command kind carrying the malformed value.
        command_kind: UniversalCommandKind,
        /// Bounded field label.
        field: &'static str,
    },
    /// Existing typed arithmetic rejected the command shape or output.
    #[error("{command_kind:?} output arithmetic is invalid: {reason}")]
    InvalidCommandShape {
        /// Command kind carrying the malformed value.
        command_kind: UniversalCommandKind,
        /// Bounded error category.
        reason: &'static str,
    },
    /// Checked output arithmetic exceeded its representable bound.
    #[error("{command_kind:?} output arithmetic overflowed")]
    ArithmeticOverflow {
        /// Command kind whose aggregate output overflowed.
        command_kind: UniversalCommandKind,
    },
}

/// Derive a complete, authority-free class matrix from typed command arguments.
pub fn estimate_capacity(
    args: &UniversalCommandArgs,
) -> Result<CapacityRequirementMatrix, CapacityEstimatorError> {
    match args {
        UniversalCommandArgs::SourceLegacyAdoption(_) => Ok(unsupported_matrix(
            UniversalCommandKind::SourceLegacyAdoption,
        )),
        UniversalCommandArgs::SourceDerivedLayout(_) => Ok(unsupported_matrix(
            UniversalCommandKind::SourceDerivedLayout,
        )),
        UniversalCommandArgs::Recording(_) => Ok(unknown_output_matrix(
            UniversalCommandKind::CreateFile,
            CapacityEstimateReason::MetadataBoundNotDefined,
        )),
        UniversalCommandArgs::Copy(args) => {
            let kind = UniversalCommandKind::Copy;
            let bytes = non_zero(kind, args.expected_source().file_size(), "source file size")?;
            Ok(new_output_matrix(kind, bytes))
        }
        UniversalCommandArgs::Import(args) => {
            let kind = UniversalCommandKind::Import;
            let bytes = non_zero(kind, args.expected_file().file_size(), "external file size")?;
            Ok(new_output_matrix(kind, bytes))
        }
        UniversalCommandArgs::DuplicateSame(args) => {
            let kind = UniversalCommandKind::Duplicate;
            let bytes = non_zero(kind, args.expected_source().file_size(), "source file size")?;
            Ok(new_output_matrix(kind, bytes))
        }
        UniversalCommandArgs::WaveformEdit(_) => Ok(unknown_output_matrix(
            UniversalCommandKind::DestructiveEdit,
            CapacityEstimateReason::OutputBoundNotExposed,
        )),
        UniversalCommandArgs::DecodedEdit(args) => {
            let kind = UniversalCommandKind::DestructiveEdit;
            let bytes = decoded_edit_output_bytes(args)?;
            Ok(new_output_matrix(kind, bytes))
        }
        UniversalCommandArgs::DecodedExport(args) => {
            let kind = UniversalCommandKind::Export;
            let bytes = decoded_export_output_bytes(args)?;
            Ok(new_output_matrix(kind, bytes))
        }
        UniversalCommandArgs::DecodedExportBatch(args) => {
            let kind = UniversalCommandKind::Export;
            args.validate()
                .map_err(|_| CapacityEstimatorError::InvalidCommandShape {
                    command_kind: kind,
                    reason: "decoded-export-batch command shape",
                })?;
            if args.outputs().len() != 1 {
                return Ok(unknown_output_matrix(
                    kind,
                    CapacityEstimateReason::OutputBoundNotExposed,
                ));
            }
            let arithmetic = args.child_output_arithmetic(0).map_err(|_| {
                CapacityEstimatorError::InvalidCommandShape {
                    command_kind: kind,
                    reason: "decoded-export-batch output arithmetic",
                }
            })?;
            let bytes = wav_render_output_bytes(kind, arithmetic.wav())?;
            Ok(new_output_matrix(kind, bytes))
        }
        UniversalCommandArgs::Namespace(args) => Ok(namespace_matrix(args)),
        UniversalCommandArgs::Render(args) => render_matrix(args),
        UniversalCommandArgs::Removal(args) => Ok(removal_matrix(args)),
        UniversalCommandArgs::RetainedResolution(args) => Ok(retained_resolution_matrix(args)),
        UniversalCommandArgs::SelectionNormalize(args) => {
            let kind = UniversalCommandKind::DestructiveEdit;
            let bytes = wav_render_output_bytes(
                kind,
                args.output_arithmetic().map_err(|_| {
                    CapacityEstimatorError::InvalidCommandShape {
                        command_kind: kind,
                        reason: "selection-normalize output arithmetic",
                    }
                })?,
            )?;
            Ok(new_output_matrix(kind, bytes))
        }
        UniversalCommandArgs::SelectionOutput(args) => selection_output_matrix(args),
        UniversalCommandArgs::History(args) => {
            Ok(history_matrix(args.command_kind(), args.actions()))
        }
        UniversalCommandArgs::ExternalReconciliation(_) => Ok(unknown_output_matrix(
            UniversalCommandKind::ExternalReconciliation,
            CapacityEstimateReason::MetadataBoundNotDefined,
        )),
    }
}

/// Return the conservative matrix for an argumentless command kind.
pub fn estimate_command_kind(command_kind: UniversalCommandKind) -> CapacityRequirementMatrix {
    match command_kind {
        UniversalCommandKind::SourceLegacyAdoption => unsupported_matrix(command_kind),
        UniversalCommandKind::SourceDerivedLayout => unsupported_matrix(command_kind),
        UniversalCommandKind::Extract => unsupported_matrix(command_kind),
        UniversalCommandKind::Copy => unsupported_matrix(command_kind),
        UniversalCommandKind::Import => unsupported_matrix(command_kind),
        UniversalCommandKind::CreateFile => matrix_with_reason(
            command_kind,
            CapacityEstimateDisposition::Unsupported(CapacityEstimateReason::MissingTypedArguments),
        ),
        UniversalCommandKind::CreateFolder => unsupported_matrix(command_kind),
        UniversalCommandKind::Duplicate => unsupported_matrix(command_kind),
        UniversalCommandKind::Export => unsupported_matrix(command_kind),
        UniversalCommandKind::Rename => unsupported_matrix(command_kind),
        UniversalCommandKind::Move => unsupported_matrix(command_kind),
        UniversalCommandKind::Trash => unsupported_matrix(command_kind),
        UniversalCommandKind::Delete => unsupported_matrix(command_kind),
        UniversalCommandKind::RestoreRetained => unsupported_matrix(command_kind),
        UniversalCommandKind::PurgeRetained => unsupported_matrix(command_kind),
        UniversalCommandKind::DestructiveEdit => unsupported_matrix(command_kind),
        UniversalCommandKind::Undo => unsupported_matrix(command_kind),
        UniversalCommandKind::Redo => unsupported_matrix(command_kind),
        UniversalCommandKind::ExternalReconciliation => unsupported_matrix(command_kind),
    }
}

fn non_zero(
    command_kind: UniversalCommandKind,
    bytes: u64,
    field: &'static str,
) -> Result<u64, CapacityEstimatorError> {
    if bytes == 0 {
        return Err(CapacityEstimatorError::ZeroEstimate {
            command_kind,
            field,
        });
    }
    Ok(bytes)
}

fn wav_render_output_bytes(
    command_kind: UniversalCommandKind,
    arithmetic: super::render_commands::WaveformRenderOutputArithmetic,
) -> Result<u64, CapacityEstimatorError> {
    arithmetic
        .riff_chunk_size()
        .checked_add(8)
        .filter(|bytes| *bytes > 0)
        .ok_or(CapacityEstimatorError::ArithmeticOverflow { command_kind })
}

fn decoded_edit_output_bytes(args: &DecodedEditIntentArgs) -> Result<u64, CapacityEstimatorError> {
    let kind = UniversalCommandKind::DestructiveEdit;
    let frames = args
        .output_frames()
        .map_err(|_| CapacityEstimatorError::InvalidCommandShape {
            command_kind: kind,
            reason: "decoded-edit output frames",
        })?;
    let arithmetic = WaveformRenderOutputSpec::float32(args.output())
        .checked_arithmetic(frames)
        .map_err(|error| match error {
            RenderCommandValidationError::OutputArithmeticOverflow => {
                CapacityEstimatorError::ArithmeticOverflow { command_kind: kind }
            }
            _ => CapacityEstimatorError::InvalidCommandShape {
                command_kind: kind,
                reason: "decoded-edit output arithmetic",
            },
        })?;
    let bytes = wav_render_output_bytes(kind, arithmetic)?;
    non_zero(kind, bytes, "decoded-edit output")
}

fn decoded_export_output_bytes(
    args: &DecodedExportIntentArgs,
) -> Result<u64, CapacityEstimatorError> {
    let kind = UniversalCommandKind::Export;
    let arithmetic =
        args.output_arithmetic()
            .map_err(|_| CapacityEstimatorError::InvalidCommandShape {
                command_kind: kind,
                reason: "decoded-export output arithmetic",
            })?;
    wav_render_output_bytes(kind, arithmetic.wav())
}

fn render_matrix(
    args: &RenderCommandArgs,
) -> Result<CapacityRequirementMatrix, CapacityEstimatorError> {
    match args {
        RenderCommandArgs::Normalize(args) => {
            let kind = UniversalCommandKind::DestructiveEdit;
            let bytes = wav_render_output_bytes(
                kind,
                args.output_arithmetic().map_err(|_| {
                    CapacityEstimatorError::InvalidCommandShape {
                        command_kind: kind,
                        reason: "render-normalize output arithmetic",
                    }
                })?,
            )?;
            Ok(new_output_matrix(kind, bytes))
        }
        RenderCommandArgs::DuplicateDouble(args) => {
            let kind = UniversalCommandKind::Duplicate;
            let bytes = wav_render_output_bytes(
                kind,
                args.output_arithmetic().map_err(|_| {
                    CapacityEstimatorError::InvalidCommandShape {
                        command_kind: kind,
                        reason: "render-duplicate output arithmetic",
                    }
                })?,
            )?;
            Ok(new_output_matrix(kind, bytes))
        }
    }
}

fn selection_output_matrix(
    args: &SelectionOutputCommandArgs,
) -> Result<CapacityRequirementMatrix, CapacityEstimatorError> {
    match args {
        SelectionOutputCommandArgs::Extract(args) => {
            let kind = UniversalCommandKind::Extract;
            let bytes = args
                .output_arithmetic()
                .map_err(|_| CapacityEstimatorError::InvalidCommandShape {
                    command_kind: kind,
                    reason: "selection-extract output arithmetic",
                })?
                .output_file_bytes();
            Ok(new_output_matrix(
                kind,
                non_zero(kind, bytes, "selection output")?,
            ))
        }
        SelectionOutputCommandArgs::ExtractAndTrim(args) => {
            let kind = UniversalCommandKind::DestructiveEdit;
            args.extraction().output_arithmetic().map_err(|_| {
                CapacityEstimatorError::InvalidCommandShape {
                    command_kind: kind,
                    reason: "extract-and-trim extraction arithmetic",
                }
            })?;
            args.trim_output_arithmetic().map_err(|_| {
                CapacityEstimatorError::InvalidCommandShape {
                    command_kind: kind,
                    reason: "extract-and-trim trim arithmetic",
                }
            })?;
            Ok(unknown_output_matrix(
                kind,
                CapacityEstimateReason::OutputBoundNotExposed,
            ))
        }
    }
}

fn namespace_matrix(args: &NamespaceCommandArgs) -> CapacityRequirementMatrix {
    let kind = args.command_kind();
    match args {
        NamespaceCommandArgs::Rename(_) => metadata_matrix(kind),
        NamespaceCommandArgs::Move(_) => metadata_matrix(kind),
        NamespaceCommandArgs::CreateFolder(_) => metadata_matrix(kind),
    }
}

fn removal_matrix(args: &RemovalCommandArgs) -> CapacityRequirementMatrix {
    let kind = args.command_kind();
    match args {
        RemovalCommandArgs::Trash(_) => metadata_matrix(kind),
        RemovalCommandArgs::StageRetainedFolder(_) => metadata_matrix(kind),
    }
}

fn retained_resolution_matrix(args: &RetainedResolutionCommandArgs) -> CapacityRequirementMatrix {
    let kind = args.command_kind();
    match args {
        RetainedResolutionCommandArgs::Restore(_) => metadata_matrix(kind),
        RetainedResolutionCommandArgs::Purge(_) => metadata_matrix(kind),
    }
}

fn history_matrix(
    command_kind: UniversalCommandKind,
    actions: &[HistoryAction],
) -> CapacityRequirementMatrix {
    for action in actions {
        match action {
            HistoryAction::FolderMove(_) => {}
            HistoryAction::WaveformRestore(_) => {}
        }
    }
    metadata_matrix(command_kind)
}

fn new_output_matrix(
    command_kind: UniversalCommandKind,
    output_bytes: u64,
) -> CapacityRequirementMatrix {
    let mut matrix = matrix_with_reason(
        command_kind,
        CapacityEstimateDisposition::Unknown(CapacityEstimateReason::DescriptorBoundFactsRequired),
    );
    matrix.rows[0] = CapacityRequirementEstimate::new(
        FullCapacityAllocationClass::DestinationStaging,
        CapacityEstimateDisposition::Known(output_bytes),
    );
    matrix
}

fn unknown_output_matrix(
    command_kind: UniversalCommandKind,
    reason: CapacityEstimateReason,
) -> CapacityRequirementMatrix {
    let mut matrix = matrix_with_reason(command_kind, CapacityEstimateDisposition::Unknown(reason));
    matrix.rows[0] = CapacityRequirementEstimate::new(
        FullCapacityAllocationClass::DestinationStaging,
        CapacityEstimateDisposition::Unknown(reason),
    );
    matrix
}

fn metadata_matrix(command_kind: UniversalCommandKind) -> CapacityRequirementMatrix {
    matrix_with_reason(
        command_kind,
        CapacityEstimateDisposition::Unknown(CapacityEstimateReason::MetadataBoundNotDefined),
    )
}

fn unsupported_matrix(command_kind: UniversalCommandKind) -> CapacityRequirementMatrix {
    matrix_with_reason(
        command_kind,
        CapacityEstimateDisposition::Unsupported(
            CapacityEstimateReason::FamilyEstimatorNotImplemented,
        ),
    )
}

fn matrix_with_reason(
    command_kind: UniversalCommandKind,
    disposition: CapacityEstimateDisposition,
) -> CapacityRequirementMatrix {
    CapacityRequirementMatrix {
        version: CAPACITY_ESTIMATOR_MATRIX_VERSION,
        command_kind,
        rows: FULL_CAPACITY_ALLOCATION_CLASSES
            .map(|class| CapacityRequirementEstimate::new(class, disposition)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::operation_journal::{
        CopyCollisionPolicy, CopyEndpoint, CopyIntentArgs, CopySourceExpectation,
        DecodedEditIntentArgs, DecodedEditOperationArgs, DecodedExportBatchIntentArgs,
        DecodedExportBatchOutput, DecodedExportOutputPolicy, ExtractAndTrimIntentArgs,
        FiniteF32Bits, HistoryAction, HistoryDirection, HistoryRecoveryBackupObservation,
        HistoryRecoveryLeafObservation, HistoryRestoreTargetExpectation, HistoryScopeId,
        HistoryUndoRedoIntentArgs, HistoryWaveformRestore, NamespaceCommandArgs,
        NamespaceDirectoryEndpoint, NamespaceDirectoryExpectation, NamespaceDirectoryLocator,
        NamespaceEntryEndpoint, NamespaceEntryExpectation, NamespaceFileExpectation,
        NamespaceMoveIntentArgs, ParticipantId, RawWavExtractionRecipe, SelectionExtractIntentArgs,
        SelectionOutputCommandArgs, SelectionOutputRecipe, SourcePlaybackType, StoredRelativePath,
        WaveformEditFrameRange, WaveformFileRevisionObservation, WaveformOutputSpec,
        WaveformSampleFormat, WaveformSourceSpec,
    };
    use crate::sample_sources::SourceId;
    use uuid::Uuid;

    fn copy_args(file_size: u64) -> UniversalCommandArgs {
        let source = CopyEndpoint::new(
            ParticipantId::from_uuid(Uuid::from_u128(1)),
            SourceId::from_string("source"),
            StoredRelativePath::new("source.wav").expect("source path"),
        )
        .expect("source endpoint");
        let destination = CopyEndpoint::new(
            ParticipantId::from_uuid(Uuid::from_u128(2)),
            SourceId::from_string("destination"),
            StoredRelativePath::new("destination.wav").expect("destination path"),
        )
        .expect("destination endpoint");
        UniversalCommandArgs::Copy(
            CopyIntentArgs::new(
                source,
                destination,
                CopySourceExpectation::new(1, "file", file_size, 1).expect("source facts"),
                CopyCollisionPolicy::NoReplace,
                None,
            )
            .expect("copy args"),
        )
    }

    fn decoded_edit_args() -> UniversalCommandArgs {
        let source = CopyEndpoint::new(
            ParticipantId::from_uuid(Uuid::from_u128(3)),
            SourceId::from_string("decoded-source"),
            StoredRelativePath::new("source.wav").expect("source path"),
        )
        .expect("source endpoint");
        UniversalCommandArgs::DecodedEdit(
            DecodedEditIntentArgs::new(
                source,
                WaveformFileRevisionObservation::new("file", "marker", 100, Some(1))
                    .expect("revision"),
                WaveformSourceSpec::new(2, 48_000, 32, WaveformSampleFormat::Float)
                    .expect("source spec"),
                100,
                DecodedEditOperationArgs::click_repair(
                    WaveformEditFrameRange::new(1, 99).expect("range"),
                ),
                WaveformOutputSpec::float32(2, 48_000),
            )
            .expect("decoded edit"),
        )
    }

    fn batch_args(output_count: usize) -> UniversalCommandArgs {
        let source = CopyEndpoint::new(
            ParticipantId::from_uuid(Uuid::from_u128(4)),
            SourceId::from_string("batch-source"),
            StoredRelativePath::new("folder/input.wav").expect("source path"),
        )
        .expect("source endpoint");
        let outputs = (0..output_count)
            .map(|index| {
                DecodedExportBatchOutput::new(
                    WaveformEditFrameRange::new(10 + (index as u64 * 10), 30 + (index as u64 * 10))
                        .expect("range"),
                    CopyEndpoint::new(
                        ParticipantId::from_uuid(Uuid::from_u128(4)),
                        SourceId::from_string("batch-source"),
                        StoredRelativePath::new(format!("folder/output-{index}.wav"))
                            .expect("destination path"),
                    )
                    .expect("destination endpoint"),
                )
                .expect("batch output")
            })
            .collect();
        UniversalCommandArgs::DecodedExportBatch(
            DecodedExportBatchIntentArgs::from_reference(
                source,
                7,
                WaveformFileRevisionObservation::new("file", "marker", 1024, Some(1))
                    .expect("revision"),
                WaveformSourceSpec::new(2, 48_000, 32, WaveformSampleFormat::Float)
                    .expect("source spec"),
                100,
                outputs,
                DecodedExportOutputPolicy::default(),
                None,
                20,
            )
            .expect("batch args"),
        )
    }

    fn extract_and_trim_args() -> UniversalCommandArgs {
        let source = CopyEndpoint::new(
            ParticipantId::from_uuid(Uuid::from_u128(10)),
            SourceId::from_string("extract-source"),
            StoredRelativePath::new("folder/source.wav").expect("source path"),
        )
        .expect("source endpoint");
        let destination = CopyEndpoint::new(
            ParticipantId::from_uuid(Uuid::from_u128(11)),
            SourceId::from_string("extract-destination"),
            StoredRelativePath::new("folder/clip.wav").expect("destination path"),
        )
        .expect("destination endpoint");
        let source_spec = WaveformSourceSpec::new(2, 48_000, 16, WaveformSampleFormat::Integer)
            .expect("source spec");
        let recipe = SelectionOutputRecipe::RawWav(
            RawWavExtractionRecipe::new(16, FiniteF32Bits::new(1.0).expect("gain"), 20)
                .expect("recipe"),
        );
        let extraction = SelectionExtractIntentArgs::new(
            source,
            7,
            WaveformFileRevisionObservation::new("file", "change", 1_000, Some(11))
                .expect("revision"),
            source_spec,
            100,
            WaveformEditFrameRange::new(10, 50).expect("range"),
            recipe,
            SourcePlaybackType::OneShot,
            destination,
            CopyCollisionPolicy::NoReplace,
        )
        .expect("extraction");
        UniversalCommandArgs::SelectionOutput(SelectionOutputCommandArgs::ExtractAndTrim(
            ExtractAndTrimIntentArgs::new(extraction, WaveformOutputSpec::float32(2, 48_000))
                .expect("extract and trim"),
        ))
    }

    fn namespace_move_args() -> UniversalCommandArgs {
        let participant = ParticipantId::from_uuid(Uuid::from_u128(5));
        let source_id = SourceId::from_string("namespace-source");
        let source = NamespaceEntryEndpoint::new(
            participant,
            source_id.clone(),
            StoredRelativePath::new("old.wav").expect("source path"),
            NamespaceEntryExpectation::RegularFile(
                NamespaceFileExpectation::new(7, "file-a", 12, Some(-2)).expect("file expectation"),
            ),
        )
        .expect("source entry");
        let destination_parent = NamespaceDirectoryEndpoint::new(
            participant,
            source_id,
            NamespaceDirectoryLocator::child(
                StoredRelativePath::new("archive").expect("archive path"),
            )
            .expect("child locator"),
            NamespaceDirectoryExpectation::new(7, "archive", Some(-1))
                .expect("directory expectation"),
        )
        .expect("destination parent");
        UniversalCommandArgs::Namespace(NamespaceCommandArgs::Move(
            NamespaceMoveIntentArgs::new(source, destination_parent).expect("move args"),
        ))
    }

    #[test]
    fn folder_create_cannot_substitute_zero_or_known_staging_for_metadata_bounds() {
        let parent = NamespaceDirectoryEndpoint::new(
            ParticipantId::from_uuid(Uuid::from_u128(5)),
            SourceId::from_string("create-source"),
            NamespaceDirectoryLocator::root(),
            NamespaceDirectoryExpectation::new(7, "retained-parent", None).unwrap(),
        )
        .unwrap();
        let args = UniversalCommandArgs::Namespace(NamespaceCommandArgs::CreateFolder(
            super::super::CreateFolderIntentArgs::new(
                parent,
                super::super::NamespaceLeafName::new("new-child").unwrap(),
            )
            .unwrap(),
        ));
        let matrix = estimate_capacity(&args).unwrap();
        assert_eq!(matrix.command_kind(), UniversalCommandKind::CreateFolder);
        for class in FULL_CAPACITY_ALLOCATION_CLASSES {
            assert!(matches!(
                matrix.require_known_bytes(class),
                Err(CapacityRequirementError::Unresolved {
                    disposition: CapacityEstimateDisposition::Unknown(
                        CapacityEstimateReason::MetadataBoundNotDefined
                    ),
                    ..
                })
            ));
        }
    }

    fn history_args() -> UniversalCommandArgs {
        let participant = ParticipantId::from_uuid(Uuid::from_u128(6));
        let source_id = SourceId::from_string("history-source");
        let target = CopyEndpoint::new(
            participant,
            source_id,
            StoredRelativePath::new("source.wav").expect("target path"),
        )
        .expect("target endpoint");
        let revision = |identity: &str| {
            WaveformFileRevisionObservation::new(identity, "change", 10, Some(11))
                .expect("revision")
        };
        let before =
            HistoryRecoveryLeafObservation::new(revision("before")).expect("before backup");
        let backups = HistoryRecoveryBackupObservation::new(
            ParticipantId::from_uuid(Uuid::from_u128(7)),
            "recovery-root",
            Uuid::from_u128(8),
            "edit-directory",
            Some(before),
            None,
            None,
        )
        .expect("backups");
        let restore = HistoryWaveformRestore::new(
            target,
            7,
            HistoryRestoreTargetExpectation::Existing(revision("target")),
            Some(String::from("before-content")),
            backups,
            None,
        )
        .expect("restore");
        UniversalCommandArgs::History(
            HistoryUndoRedoIntentArgs::new(
                HistoryScopeId::from_uuid(Uuid::from_u128(9)),
                4,
                5,
                Some(6),
                HistoryDirection::Undo,
                vec![HistoryAction::WaveformRestore(restore)],
            )
            .expect("history args"),
        )
    }

    #[test]
    fn every_command_kind_has_an_explicit_matrix() {
        assert_eq!(UNIVERSAL_COMMAND_KINDS.len(), 17);
        let mut command_ids = std::collections::BTreeSet::new();
        for kind in UNIVERSAL_COMMAND_KINDS {
            let matrix = estimate_command_kind(kind);
            assert_eq!(matrix.version(), CAPACITY_ESTIMATOR_MATRIX_VERSION);
            assert_eq!(matrix.command_kind(), kind);
            assert!(command_ids.insert(kind.stable_id()));
            assert_eq!(matrix.rows().len(), FULL_CAPACITY_ALLOCATION_CLASSES.len());
            assert!(
                matrix
                    .rows()
                    .iter()
                    .map(|row| row.class())
                    .eq(FULL_CAPACITY_ALLOCATION_CLASSES)
            );
        }
        let class_ids = FULL_CAPACITY_ALLOCATION_CLASSES
            .into_iter()
            .map(FullCapacityAllocationClass::stable_id)
            .collect::<std::collections::BTreeSet<_>>();
        assert_eq!(
            UNIVERSAL_COMMAND_KINDS
                .into_iter()
                .map(UniversalCommandKind::stable_id)
                .collect::<Vec<_>>(),
            vec![
                "family.extract",
                "family.copy",
                "family.import",
                "family.create_file",
                "family.create_folder",
                "family.duplicate",
                "family.export",
                "family.rename",
                "family.move",
                "family.trash",
                "family.delete",
                "family.restore_retained",
                "family.purge_retained",
                "family.destructive_edit",
                "family.undo",
                "family.redo",
                "family.external_reconciliation",
            ]
        );
        assert_eq!(
            FULL_CAPACITY_ALLOCATION_CLASSES
                .into_iter()
                .map(FullCapacityAllocationClass::stable_id)
                .collect::<Vec<_>>(),
            vec![
                "capacity.destination_staging",
                "capacity.journal_record",
                "capacity.source_database_commit",
                "capacity.source_wal_shm",
                "capacity.global_database_commit",
                "capacity.global_wal_shm",
                "capacity.existing_destination",
                "capacity.existing_recovery_payload",
                "capacity.operation_recovery_payload",
            ]
        );
        assert_eq!(command_ids.len(), UNIVERSAL_COMMAND_KINDS.len());
        assert_eq!(class_ids.len(), FULL_CAPACITY_ALLOCATION_CLASSES.len());
        assert!(matches!(
            estimate_command_kind(UniversalCommandKind::CreateFile)
                .for_class(FullCapacityAllocationClass::DestinationStaging)
                .disposition(),
            CapacityEstimateDisposition::Unsupported(CapacityEstimateReason::MissingTypedArguments)
        ));
    }

    #[test]
    fn canonical_matrix_ids_and_outcomes_are_documented_once() {
        const IO_ARCHITECTURE: &str = include_str!("../../../../docs/IO_ARCHITECTURE_TARGET.md");
        for stable_id in UNIVERSAL_COMMAND_KINDS
            .into_iter()
            .map(UniversalCommandKind::stable_id)
            .chain(
                FULL_CAPACITY_ALLOCATION_CLASSES
                    .into_iter()
                    .map(FullCapacityAllocationClass::stable_id),
            )
        {
            assert_eq!(
                IO_ARCHITECTURE.matches(stable_id).count(),
                1,
                "matrix id {stable_id} must appear once in the canonical table/list",
            );
        }
        for marker in [
            "DecodedExportBatch with multiple outputs",
            "SelectionOutput::ExtractAndTrim",
            "Namespace::Rename/Move/CreateFolder",
            "CreateFile (argumentless kind)",
            "CapacityRequirementMatrix::require_known_bytes",
            "NotApplicable` is reserved",
        ] {
            assert!(
                IO_ARCHITECTURE.contains(marker),
                "canonical matrix docs must retain payload marker {marker:?}"
            );
        }
    }

    #[test]
    fn copy_uses_checked_file_size_and_keeps_other_classes_explicit() {
        let matrix = estimate_capacity(&copy_args(4096)).expect("copy estimate");
        assert_eq!(matrix.command_kind(), UniversalCommandKind::Copy);
        assert_eq!(
            matrix
                .for_class(FullCapacityAllocationClass::DestinationStaging)
                .disposition(),
            CapacityEstimateDisposition::Known(4096)
        );
        assert!(matches!(
            matrix
                .for_class(FullCapacityAllocationClass::JournalRecord)
                .disposition(),
            CapacityEstimateDisposition::Unknown(
                CapacityEstimateReason::DescriptorBoundFactsRequired
            )
        ));
        assert!(matches!(
            matrix
                .for_class(FullCapacityAllocationClass::ExistingDestination)
                .disposition(),
            CapacityEstimateDisposition::Unknown(
                CapacityEstimateReason::DescriptorBoundFactsRequired
            )
        ));
    }

    #[test]
    fn zero_copy_size_is_rejected_before_planning() {
        assert_eq!(
            estimate_capacity(&copy_args(0)),
            Err(CapacityEstimatorError::ZeroEstimate {
                command_kind: UniversalCommandKind::Copy,
                field: "source file size",
            })
        );
    }

    #[test]
    fn matrix_is_not_a_capacity_authority() {
        let matrix = estimate_capacity(&copy_args(4096)).expect("copy estimate");
        assert!(!matches!(
            matrix
                .for_class(FullCapacityAllocationClass::DestinationStaging)
                .disposition(),
            CapacityEstimateDisposition::Unsupported(_)
        ));
        assert_eq!(FULL_CAPACITY_ALLOCATION_CLASSES.len(), 9);
    }

    #[test]
    fn extraction_fails_closed_for_unresolved_and_missing_rows() {
        let matrix = estimate_command_kind(UniversalCommandKind::CreateFile);
        assert!(matches!(
            matrix.require_known_bytes(FullCapacityAllocationClass::DestinationStaging),
            Err(CapacityRequirementError::Unresolved {
                disposition: CapacityEstimateDisposition::Unsupported(
                    CapacityEstimateReason::MissingTypedArguments
                ),
                ..
            })
        ));

        let matrix = estimate_capacity(&copy_args(4096)).expect("copy estimate");
        assert!(matches!(
            matrix.require_known_bytes(FullCapacityAllocationClass::JournalRecord),
            Err(CapacityRequirementError::Unresolved {
                disposition: CapacityEstimateDisposition::Unknown(
                    CapacityEstimateReason::DescriptorBoundFactsRequired
                ),
                ..
            })
        ));

        let mut rows = FULL_CAPACITY_ALLOCATION_CLASSES.map(|class| {
            CapacityRequirementEstimate::new(class, CapacityEstimateDisposition::Known(1))
        });
        rows[0] = CapacityRequirementEstimate::new(
            FullCapacityAllocationClass::JournalRecord,
            CapacityEstimateDisposition::Known(1),
        );
        let malformed = CapacityRequirementMatrix {
            version: CAPACITY_ESTIMATOR_MATRIX_VERSION,
            command_kind: UniversalCommandKind::Copy,
            rows,
        };
        assert_eq!(
            malformed.require_known_bytes(FullCapacityAllocationClass::DestinationStaging),
            Err(CapacityRequirementError::MissingClass {
                command_kind: UniversalCommandKind::Copy,
                class: FullCapacityAllocationClass::DestinationStaging,
            })
        );
    }

    #[test]
    fn actual_nested_payloads_keep_unproven_shapes_explicit() {
        let decoded_edit = estimate_capacity(&decoded_edit_args()).expect("decoded edit matrix");
        assert_eq!(
            decoded_edit
                .for_class(FullCapacityAllocationClass::DestinationStaging)
                .disposition(),
            CapacityEstimateDisposition::Known(868)
        );

        let batch = estimate_capacity(&batch_args(2)).expect("batch matrix");
        assert_eq!(
            batch
                .for_class(FullCapacityAllocationClass::DestinationStaging)
                .disposition(),
            CapacityEstimateDisposition::Unknown(CapacityEstimateReason::OutputBoundNotExposed)
        );
        let single_batch = estimate_capacity(&batch_args(1)).expect("single batch matrix");
        assert!(matches!(
            single_batch
                .for_class(FullCapacityAllocationClass::DestinationStaging)
                .disposition(),
            CapacityEstimateDisposition::Known(_)
        ));

        let extract_and_trim =
            estimate_capacity(&extract_and_trim_args()).expect("extract-and-trim matrix");
        assert_eq!(
            extract_and_trim
                .for_class(FullCapacityAllocationClass::DestinationStaging)
                .disposition(),
            CapacityEstimateDisposition::Unknown(CapacityEstimateReason::OutputBoundNotExposed)
        );

        for args in [namespace_move_args(), history_args()] {
            let matrix = estimate_capacity(&args).expect("nested metadata matrix");
            assert!(matrix.rows().iter().all(|row| matches!(
                row.disposition(),
                CapacityEstimateDisposition::Unknown(
                    CapacityEstimateReason::MetadataBoundNotDefined
                )
            )));
        }
    }

    #[test]
    fn decoded_edit_checked_format_limits_reject_before_matrix() {
        let UniversalCommandArgs::DecodedEdit(valid) = decoded_edit_args() else {
            panic!("decoded edit fixture");
        };
        let mut value = serde_json::to_value(valid).expect("serialize edit");
        value["total_frames"] = serde_json::Value::from(u64::from(u32::MAX));
        let malformed = serde_json::from_value::<DecodedEditIntentArgs>(value)
            .expect("deserialize bounded-shape edit");
        assert_eq!(
            estimate_capacity(&UniversalCommandArgs::DecodedEdit(malformed)),
            Err(CapacityEstimatorError::ArithmeticOverflow {
                command_kind: UniversalCommandKind::DestructiveEdit,
            })
        );
    }
}
