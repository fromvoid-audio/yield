//! Validated decimal tempo for an audited Source playmark grid.

const BPM_SCALE: u32 = 1_000;

/// A valid user-entered grid tempo stored in thousandths of a BPM.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SourcePlaymarkGridTempo(u32);

impl SourcePlaymarkGridTempo {
    /// Parse 1–1000 BPM with up to three decimal places.
    pub fn parse(text: &str) -> Option<Self> {
        let text = text.trim();
        let (whole, fractional) = text.split_once('.').unwrap_or((text, ""));
        if whole.is_empty()
            || fractional.len() > 3
            || !whole
                .bytes()
                .chain(fractional.bytes())
                .all(|byte| byte.is_ascii_digit())
        {
            return None;
        }
        let whole = whole.parse::<u32>().ok()?.checked_mul(BPM_SCALE)?;
        let fractional = if fractional.is_empty() {
            0
        } else {
            fractional.parse::<u32>().ok()? * 10_u32.pow(3 - fractional.len() as u32)
        };
        let tempo = whole.checked_add(fractional)?;
        Self::from_milli_bpm(tempo)
    }

    pub(super) fn from_milli_bpm(value: u32) -> Option<Self> {
        (BPM_SCALE..=1_000 * BPM_SCALE)
            .contains(&value)
            .then_some(Self(value))
    }

    pub(super) fn milli_bpm(self) -> u32 {
        self.0
    }

    /// Convert the fixed decimal value to BPM.
    pub fn bpm(self) -> f64 {
        f64::from(self.0) / f64::from(BPM_SCALE)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn grid_tempo_accepts_exact_decimal_input_and_rejects_other_notation() {
        for (text, bpm) in [
            (" 120.5 ", 120.5),
            ("123.145", 123.145),
            ("1", 1.0),
            ("1000.000", 1000.0),
        ] {
            assert_eq!(
                SourcePlaymarkGridTempo::parse(text).map(SourcePlaymarkGridTempo::bpm),
                Some(bpm)
            );
        }
        for text in [
            "", "0", "0.999", "-2", "+120", "1000.001", "121.0001", "1e2", "NaN", "120.4.5", "١٢٠",
        ] {
            assert!(SourcePlaymarkGridTempo::parse(text).is_none(), "{text}");
        }
    }
}
