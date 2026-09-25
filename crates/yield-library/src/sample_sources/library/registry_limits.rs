use std::io::{self, Write};
use std::path::Path;

use serde::Serialize;

use crate::sample_sources::SampleSource;

/// Maximum number of active or retained source descriptors handled by one owner operation.
pub const SOURCE_REGISTRY_MAX_DESCRIPTORS: usize = 4096;
/// Maximum encoded source-registry representation retained by one owner operation.
pub const SOURCE_REGISTRY_MAX_BYTES: usize = 16 * 1024 * 1024;
/// Maximum encoded size of one source descriptor or path field.
pub const SOURCE_REGISTRY_MAX_DESCRIPTOR_BYTES: usize = 64 * 1024;
/// Maximum source identity length shared with the operation-journal identity contract.
pub const SOURCE_REGISTRY_MAX_SOURCE_ID_BYTES: usize =
    crate::operation_journal::MAX_SOURCE_IDENTITY_BYTES;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum RegistryBoundError {
    TooManyDescriptors {
        actual: usize,
    },
    AggregateTooLarge {
        actual: usize,
    },
    DescriptorTooLarge {
        index: usize,
        actual: usize,
    },
    FieldTooLarge {
        index: usize,
        field: &'static str,
        actual: usize,
    },
    SourceIdTooLarge {
        index: usize,
        actual: usize,
    },
    ArithmeticOverflow,
    Serialization,
}

/// Validate caller-owned source descriptors before they cross the owner queue.
pub(super) fn validate_sources(sources: &[SampleSource]) -> Result<usize, RegistryBoundError> {
    if sources.len() > SOURCE_REGISTRY_MAX_DESCRIPTORS {
        return Err(RegistryBoundError::TooManyDescriptors {
            actual: sources.len(),
        });
    }

    let mut aggregate = 0usize;
    for (index, source) in sources.iter().enumerate() {
        let source_id = source.id.as_str().len();
        if source_id > SOURCE_REGISTRY_MAX_SOURCE_ID_BYTES {
            return Err(RegistryBoundError::SourceIdTooLarge {
                index,
                actual: source_id,
            });
        }
        let root = source.root.to_string_lossy();
        let primary_import_folder = source.primary_import_folder.to_string_lossy();
        for (field, value) in [
            ("root", root.as_bytes()),
            ("primary_import_folder", primary_import_folder.as_bytes()),
        ] {
            if value.len() > SOURCE_REGISTRY_MAX_DESCRIPTOR_BYTES {
                return Err(RegistryBoundError::FieldTooLarge {
                    index,
                    field,
                    actual: value.len(),
                });
            }
        }
        let descriptor = source_id
            .checked_add(root.len())
            .and_then(|value| value.checked_add(primary_import_folder.len()))
            .and_then(|value| value.checked_add(source.role.as_str().len()))
            .and_then(|value| value.checked_add(source.metadata_storage.as_str().len()))
            .and_then(|value| value.checked_add(128))
            .ok_or(RegistryBoundError::ArithmeticOverflow)?;
        if descriptor > SOURCE_REGISTRY_MAX_DESCRIPTOR_BYTES {
            return Err(RegistryBoundError::DescriptorTooLarge {
                index,
                actual: descriptor,
            });
        }
        aggregate = aggregate
            .checked_add(descriptor)
            .ok_or(RegistryBoundError::ArithmeticOverflow)?;
        if aggregate > SOURCE_REGISTRY_MAX_BYTES {
            return Err(RegistryBoundError::AggregateTooLarge { actual: aggregate });
        }
    }
    Ok(aggregate)
}

/// Validate the actual JSON representation used by owner commands, including escaping overhead.
pub(super) fn validate_sources_encoded(
    sources: &[SampleSource],
) -> Result<usize, RegistryBoundError> {
    validate_sources(sources)?;
    let mut aggregate = 0usize;
    for (index, source) in sources.iter().enumerate() {
        let encoded = bounded_json_with_limit(source, SOURCE_REGISTRY_MAX_DESCRIPTOR_BYTES)
            .map_err(|error| match error {
                RegistryBoundError::AggregateTooLarge { actual } => {
                    RegistryBoundError::DescriptorTooLarge { index, actual }
                }
                other => other,
            })?;
        aggregate = aggregate
            .checked_add(encoded.len())
            .ok_or(RegistryBoundError::ArithmeticOverflow)?;
        if aggregate > SOURCE_REGISTRY_MAX_BYTES {
            return Err(RegistryBoundError::AggregateTooLarge { actual: aggregate });
        }
    }
    let encoded = bounded_json(sources)?;
    Ok(encoded.len())
}

/// Serialize a bounded owner payload without first constructing an unbounded JSON buffer.
pub(super) fn bounded_json<T: Serialize + ?Sized>(
    value: &T,
) -> Result<Vec<u8>, RegistryBoundError> {
    bounded_json_with_limit(value, SOURCE_REGISTRY_MAX_BYTES)
}

pub(super) fn bounded_json_at_limit<T: Serialize + ?Sized>(
    value: &T,
    limit: usize,
) -> Result<Vec<u8>, RegistryBoundError> {
    bounded_json_with_limit(value, limit)
}

fn bounded_json_with_limit<T: Serialize + ?Sized>(
    value: &T,
    limit: usize,
) -> Result<Vec<u8>, RegistryBoundError> {
    let mut writer = BoundedJsonWriter {
        bytes: Vec::new(),
        overflowed: false,
        limit,
    };
    if serde_json::to_writer(&mut writer, value).is_err() {
        if writer.overflowed {
            return Err(RegistryBoundError::AggregateTooLarge {
                actual: limit.saturating_add(1),
            });
        }
        return Err(RegistryBoundError::Serialization);
    }
    Ok(writer.bytes)
}

struct BoundedJsonWriter {
    bytes: Vec<u8>,
    overflowed: bool,
    limit: usize,
}

impl Write for BoundedJsonWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let next = self.bytes.len().checked_add(bytes.len()).ok_or_else(|| {
            self.overflowed = true;
            io::Error::new(io::ErrorKind::WriteZero, "bounded JSON payload overflow")
        })?;
        if next > self.limit {
            self.overflowed = true;
            return Err(io::Error::new(
                io::ErrorKind::WriteZero,
                "bounded JSON payload exceeds source-registry limit",
            ));
        }
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

pub(super) fn path_bytes(path: &Path) -> usize {
    path.to_string_lossy().as_bytes().len()
}
