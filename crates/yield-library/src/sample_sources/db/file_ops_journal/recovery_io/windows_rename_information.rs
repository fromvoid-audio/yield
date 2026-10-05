//! Bounded native rename backing, not publication or physical qualification authority.
use std::mem::size_of;

const MAX_BACKING: usize = 64 * 1024;
pub(super) const NATIVE_NAME_OFFSET: usize = 2 * size_of::<usize>() + size_of::<u32>();

#[derive(Debug, Eq, PartialEq)]
pub(super) enum RenameInformationError {
    InvalidLeaf,
    TooLarge,
    InvalidLayout,
}
#[derive(Debug)]
pub(super) struct RenameInformation {
    words: Vec<usize>,
    size: u32,
    name_bytes: u32,
}
impl RenameInformation {
    pub(super) fn new<I>(name: I, name_offset: usize) -> Result<Self, RenameInformationError>
    where
        I: Iterator<Item = u16> + Clone,
    {
        use RenameInformationError::*;
        #[cfg(test)]
        MATERIALIZED_BACKING.with(|count| count.set(0));
        if name_offset != NATIVE_NAME_OFFSET || name_offset % size_of::<u16>() != 0 {
            return Err(InvalidLayout);
        }
        let mut units = 0usize;
        let mut only_dots = true;
        // First pass is borrowed and bounded; no name or native buffer has been allocated.
        for unit in name.clone() {
            if matches!(unit, 0 | 47 | 92 | 58) {
                return Err(InvalidLeaf);
            }
            only_dots &= unit == 46;
            units = units.checked_add(1).ok_or(TooLarge)?;
            let (_, backing) = sizes(name_offset, units)?;
            if backing > MAX_BACKING {
                return Err(TooLarge);
            }
        }
        if units == 0 || (only_dots && units <= 2) {
            return Err(InvalidLeaf);
        }
        let (size, backing) = sizes(name_offset, units)?;
        let size = u32::try_from(size).map_err(|_| TooLarge)?;
        let name_bytes = u32::try_from(units.checked_mul(size_of::<u16>()).ok_or(TooLarge)?)
            .map_err(|_| TooLarge)?;
        let mut words = vec![0usize; backing / size_of::<usize>()];
        #[cfg(test)]
        MATERIALIZED_BACKING.with(|count| count.set(words.capacity() * size_of::<usize>()));
        // usize backing is initialized and aligned for u16, and every u16 bit pattern is valid.
        let wide = unsafe {
            std::slice::from_raw_parts_mut(
                words.as_mut_ptr().cast::<u16>(),
                backing / size_of::<u16>(),
            )
        };
        let payload = &mut wide[name_offset / size_of::<u16>()..][..units];
        let mut copied = 0;
        let mut copied_only_dots = true;
        for unit in name {
            if matches!(unit, 0 | 47 | 92 | 58) {
                return Err(InvalidLeaf);
            }
            copied_only_dots &= unit == 46;
            // Do not trust arbitrary cloneable iterators to reproduce their first pass.
            *payload.get_mut(copied).ok_or(InvalidLeaf)? = unit;
            copied += 1;
        }
        if copied != units || (copied_only_dots && copied <= 2) {
            return Err(InvalidLeaf);
        }
        Ok(Self {
            words,
            size,
            name_bytes,
        })
    }
    #[cfg(windows)]
    pub(super) fn as_mut_ptr(&mut self) -> *mut usize {
        self.words.as_mut_ptr()
    }
    #[cfg(windows)]
    pub(super) fn size(&self) -> u32 {
        self.size
    }
    #[cfg(windows)]
    pub(super) fn name_bytes(&self) -> u32 {
        self.name_bytes
    }
}
fn sizes(offset: usize, units: usize) -> Result<(usize, usize), RenameInformationError> {
    use RenameInformationError::TooLarge;
    let size = units
        .checked_mul(size_of::<u16>())
        .and_then(|n| offset.checked_add(n))
        .ok_or(TooLarge)?;
    let word = size_of::<usize>();
    let backing = size
        .checked_add(word - 1)
        .map(|n| n / word * word)
        .ok_or(TooLarge)?;
    if backing > MAX_BACKING {
        return Err(TooLarge);
    }
    Ok((size, backing))
}

#[cfg(test)]
thread_local! {
    static MATERIALIZED_BACKING: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn original_rename_buffer_refuses_nonleaves_before_backing() {
        for name in ["", ".", "..", "a/b", "a\\b", "a:b", "a\0b"] {
            assert_eq!(
                RenameInformation::new(name.encode_utf16(), NATIVE_NAME_OFFSET).unwrap_err(),
                RenameInformationError::InvalidLeaf
            );
            assert_eq!(MATERIALIZED_BACKING.with(|count| count.get()), 0);
        }
        assert!(RenameInformation::new("...".encode_utf16(), NATIVE_NAME_OFFSET).is_ok());
    }
    #[test]
    fn original_rename_buffer_exact_ceiling_and_next_unit_refusal() {
        let units = (MAX_BACKING - NATIVE_NAME_OFFSET) / 2;
        let buffer =
            RenameInformation::new(std::iter::repeat_n(97, units), NATIVE_NAME_OFFSET).unwrap();
        assert_eq!(buffer.words.len() * size_of::<usize>(), MAX_BACKING);
        assert_eq!(buffer.name_bytes as usize, units * 2);
        assert_eq!(buffer.size as usize, NATIVE_NAME_OFFSET + units * 2);
        assert_eq!(
            RenameInformation::new(std::iter::repeat_n(97, units + 1), NATIVE_NAME_OFFSET)
                .unwrap_err(),
            RenameInformationError::TooLarge
        );
        assert_eq!(MATERIALIZED_BACKING.with(|count| count.get()), 0);
    }
    #[test]
    fn original_rename_buffer_copies_exact_unicode_and_native_surrogate_units() {
        let name = [0x61, 0x20ac, 0xd83d, 0xde00, 0xd800];
        let buffer = RenameInformation::new(name.into_iter(), NATIVE_NAME_OFFSET).unwrap();
        let bytes = unsafe {
            std::slice::from_raw_parts(
                buffer.words.as_ptr().cast::<u8>(),
                buffer.words.len() * size_of::<usize>(),
            )
        };
        assert!(bytes[..NATIVE_NAME_OFFSET].iter().all(|b| *b == 0));
        let expected: Vec<u8> = name.into_iter().flat_map(u16::to_ne_bytes).collect();
        assert_eq!(&bytes[NATIVE_NAME_OFFSET..buffer.size as usize], expected);
        assert!(bytes[buffer.size as usize..].iter().all(|b| *b == 0));
    }
    #[test]
    fn original_rename_buffer_refuses_changed_second_pass_grammar_and_length() {
        struct Changed(std::vec::IntoIter<u16>);
        impl Clone for Changed {
            fn clone(&self) -> Self {
                Self(vec![97, 97].into_iter())
            }
        }
        impl Iterator for Changed {
            type Item = u16;
            fn next(&mut self) -> Option<u16> {
                self.0.next()
            }
        }
        for units in [
            vec![97],
            vec![97, 97, 97],
            vec![97, 0],
            vec![97, 47],
            vec![97, 92],
            vec![97, 58],
            vec![46, 46],
        ] {
            assert_eq!(
                RenameInformation::new(Changed(units.into_iter()), NATIVE_NAME_OFFSET).unwrap_err(),
                RenameInformationError::InvalidLeaf
            );
            assert!(MATERIALIZED_BACKING.with(|count| count.get()) <= MAX_BACKING);
        }
    }
    #[test]
    fn original_rename_buffer_checked_layout_and_size_overflow() {
        for offset in [0, 1, NATIVE_NAME_OFFSET + 2, usize::MAX] {
            assert_eq!(
                RenameInformation::new("x".encode_utf16(), offset).unwrap_err(),
                RenameInformationError::InvalidLayout
            );
        }
        assert_eq!(sizes(usize::MAX, 1), Err(RenameInformationError::TooLarge));
        assert_eq!(sizes(0, usize::MAX), Err(RenameInformationError::TooLarge));
        assert_eq!(
            sizes(usize::MAX - 1, 0),
            Err(RenameInformationError::TooLarge)
        );
    }
}
