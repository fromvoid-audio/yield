//! Bounded UTF-16 path-unit accumulation shared by the Windows adapter and
//! portable tests.

/// The validation failure produced while accumulating one or more UTF-16
/// paths.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Utf16PathError {
    /// A path contained an embedded NUL at the given zero-based path index.
    EmbeddedNul {
        /// Zero-based path index containing the NUL.
        index: usize,
    },
    /// The accumulated path units exceeded the supplied byte budget.
    TooLarge {
        /// The next encoded byte count that crossed the budget.
        actual: usize,
    },
}

/// Collects exact UTF-16 path units, rejecting before retaining more than
/// `max_bytes` of path content. Vector capacity may include bounded spare space.
///
/// Each input item is one path. The iterator is intentionally generic so the
/// Windows adapter can pass `OsStr::encode_wide()` directly, preserving valid
/// native WTF-16 values without a UTF-8 conversion.
pub(crate) fn collect_bounded_utf16_paths<I, P>(
    paths: I,
    max_bytes: usize,
) -> Result<Vec<Vec<u16>>, Utf16PathError>
where
    I: IntoIterator<Item = P>,
    P: IntoIterator<Item = u16>,
{
    let mut total_bytes = 0usize;
    let mut encoded_paths = Vec::new();
    for (index, path) in paths.into_iter().enumerate() {
        let mut encoded = Vec::new();
        for unit in path {
            if unit == 0 {
                return Err(Utf16PathError::EmbeddedNul { index });
            }
            let next_bytes = total_bytes
                .checked_add(std::mem::size_of::<u16>())
                .ok_or(Utf16PathError::TooLarge { actual: usize::MAX })?;
            if next_bytes > max_bytes {
                return Err(Utf16PathError::TooLarge { actual: next_bytes });
            }
            total_bytes = next_bytes;
            encoded.push(unit);
        }
        encoded_paths.push(encoded);
    }
    Ok(encoded_paths)
}

#[cfg(test)]
mod tests {
    use super::{Utf16PathError, collect_bounded_utf16_paths};

    const BYTE_LIMIT: usize = crate::MAX_FILE_DRAG_PATH_BYTES;

    #[test]
    fn accepts_exact_utf16_byte_cap() {
        let paths =
            collect_bounded_utf16_paths([std::iter::repeat_n(0x0041, BYTE_LIMIT / 2)], BYTE_LIMIT)
                .expect("exact cap should be accepted");
        assert_eq!(paths[0].len() * std::mem::size_of::<u16>(), BYTE_LIMIT);
    }

    #[test]
    fn rejects_first_unit_over_remaining_multi_path_budget() {
        let result = collect_bounded_utf16_paths(
            [
                std::iter::repeat_n(0x0041, BYTE_LIMIT / 2),
                std::iter::repeat_n(0x0042, 1),
            ],
            BYTE_LIMIT,
        );
        assert_eq!(
            result,
            Err(Utf16PathError::TooLarge {
                actual: BYTE_LIMIT + std::mem::size_of::<u16>(),
            })
        );
    }

    #[test]
    fn rejects_embedded_nul_with_path_index() {
        assert_eq!(
            collect_bounded_utf16_paths([vec![0x0041, 0, 0x0042]], BYTE_LIMIT),
            Err(Utf16PathError::EmbeddedNul { index: 0 })
        );
    }

    #[test]
    fn preserves_unpaired_surrogate_units() {
        let paths = collect_bounded_utf16_paths([vec![0xD800]], BYTE_LIMIT)
            .expect("native WTF-16 unit should be preserved");
        assert_eq!(paths, vec![vec![0xD800]]);
    }

    #[test]
    fn rejects_excess_after_exact_cap() {
        let result = collect_bounded_utf16_paths(
            [std::iter::repeat_n(0x0041, BYTE_LIMIT / 2 + 1)],
            BYTE_LIMIT,
        );
        assert_eq!(
            result,
            Err(Utf16PathError::TooLarge {
                actual: BYTE_LIMIT + std::mem::size_of::<u16>(),
            })
        );
    }
}
