use super::{Check, Ledger, SelectionError, zip::Entry};
use unicode_casefold::UnicodeCaseFold;
use unicode_normalization::UnicodeNormalization;

pub(super) fn canonical(name: &str, directory: bool) -> Result<(), SelectionError> {
    let invalid = SelectionError::Invalid;
    if name.is_empty()
        || name.len() > 1024
        || name.contains(['\\', ':'])
        || name.chars().any(|c| c.is_control() || ignorable(c))
    {
        return Err(invalid);
    }
    let path = if directory {
        name.strip_suffix('/').ok_or(invalid)?
    } else {
        name
    };
    let mut count = 0;
    for component in path.split('/') {
        count += 1;
        if count > 32
            || component.len() > 255
            || component.is_empty()
            || component == "."
            || component == ".."
            || component.ends_with([' ', '.'])
            || component.contains(['<', '>', '"', '|', '?', '*'])
        {
            return Err(invalid);
        }
        let stem = component.split('.').next().ok_or(invalid)?;
        if stem.eq_ignore_ascii_case("con")
            || stem.eq_ignore_ascii_case("prn")
            || stem.eq_ignore_ascii_case("aux")
            || stem.eq_ignore_ascii_case("nul")
            || stem.eq_ignore_ascii_case("conin$")
            || stem.eq_ignore_ascii_case("conout$")
            || ((stem.get(..3).is_some_and(|s| s.eq_ignore_ascii_case("com"))
                || stem.get(..3).is_some_and(|s| s.eq_ignore_ascii_case("lpt")))
                && stem.get(3..).is_some_and(|s| {
                    matches!(
                        s,
                        "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9" | "¹" | "²" | "³"
                    )
                }))
        {
            return Err(invalid);
        }
    }
    Ok(())
}
// Reject default-ignorable spellings rather than normalizing them into another target filename.
fn ignorable(c: char) -> bool {
    matches!(c as u32, 0xad|0x34f|0x61c|0x115f..=0x1160|0x17b4..=0x17b5|0x180b..=0x180f|
        0x200b..=0x200f|0x202a..=0x202e|0x2060..=0x206f|0x3164|0xfe00..=0xfe0f|0xfeff|
        0xffa0|0xfff0..=0xfff8|0x1bca0..=0x1bca3|0x1d173..=0x1d17a|0xe0000..=0xe0fff)
}
struct Name<'a> {
    key: String,
    original: &'a str,
    directory: bool,
}
pub(super) fn validate(
    entries: &[Entry<'_>],
    check: &Check<'_>,
    ledger: &mut Ledger,
) -> Result<(), SelectionError> {
    let invalid = SelectionError::Invalid;
    let capacity = entries
        .len()
        .checked_mul(std::mem::size_of::<Name<'_>>())
        .ok_or(invalid)?;
    ledger.charge(capacity)?;
    let mut names = Vec::new();
    names
        .try_reserve_exact(entries.len())
        .map_err(|_| invalid)?;
    ledger.charge(
        names
            .capacity()
            .checked_mul(std::mem::size_of::<Name<'_>>())
            .ok_or(invalid)?
            .checked_sub(capacity)
            .ok_or(invalid)?,
    )?;
    for entry in entries {
        check.live()?;
        canonical(entry.name, entry.directory)?;
        let original = entry.name.trim_end_matches('/');
        if original != "yield" && !original.starts_with("yield/") {
            return Err(invalid);
        }
        if original == "yield" && !entry.directory {
            return Err(invalid);
        }
        let mut key = String::new();
        // Fold/decompose before comparisons. Upper/lower mappings also cover newer Unicode
        // simple-case pairs; full folding detects multi-character aliases such as sharp-s.
        for c in original
            .nfd()
            .flat_map(char::to_uppercase)
            .case_fold()
            .flat_map(char::to_lowercase)
            .nfd()
        {
            let n = c.len_utf8();
            if key.len().checked_add(n).is_none_or(|n| n > 8192) {
                return Err(invalid);
            }
            if key.capacity() - key.len() < n {
                ledger.charge(n)?;
                let old = key.capacity();
                key.try_reserve_exact(n).map_err(|_| invalid)?;
                let actual = key.capacity() - old;
                if actual > n {
                    ledger.charge(actual - n)?;
                }
            }
            key.push(c);
        }
        names.push(Name {
            key,
            original,
            directory: entry.directory,
        });
    }
    names.sort_unstable_by(|a, b| a.key.cmp(&b.key));
    for pair in names.windows(2) {
        check.live()?;
        if pair[0].key == pair[1].key {
            return Err(invalid);
        }
        // Implicit directories must have one original spelling too; distinct aliases cannot
        // merge trees merely because the ZIP omits explicit directory records.
        let a = &pair[0];
        let b = &pair[1];
        for (depth, index) in a
            .key
            .match_indices('/')
            .map(|(i, _)| i)
            .chain(std::iter::once(a.key.len()))
            .enumerate()
        {
            let prefix = &a.key[..index];
            if b.key == prefix
                || b.key
                    .strip_prefix(prefix)
                    .is_some_and(|s| s.starts_with('/'))
            {
                if !a
                    .original
                    .split('/')
                    .take(depth + 1)
                    .eq(b.original.split('/').take(depth + 1))
                {
                    return Err(invalid);
                }
            }
        }
    }
    for name in &names {
        check.live()?;
        for (index, _) in name.key.match_indices('/') {
            if let Ok(parent) = names.binary_search_by(|n| n.key.as_str().cmp(&name.key[..index])) {
                if !names[parent].directory {
                    return Err(invalid);
                }
            }
        }
    }
    Ok(())
}
