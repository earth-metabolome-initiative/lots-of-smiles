//! Zero-allocation byte-level pre-parse scans.
//!
//! These run on the raw SMILES bytes before the (much more expensive) parser
//! tier, so molecules that would be rejected anyway never pay parse cost. Each
//! check here is chosen to be *definitive* in the reject direction: a positive
//! result is always a true reject, never a false one.

/// Returns `true` if the SMILES encodes more than one connected component.
///
/// In SMILES the `.` character is exclusively the fragment/disconnection
/// separator; it never appears inside bracket atoms or as part of any other
/// token. So a single byte scan for `.` is an exact multi-component test.
pub fn is_multi_component(smiles: &[u8]) -> bool {
    smiles.contains(&b'.')
}

/// Returns `true` if the SMILES contains an explicit isotope specification.
///
/// Isotopes are written as a mass number immediately after the opening bracket
/// of a bracket atom, e.g. `[13C]`, `[2H]`. The only tokens that can follow `[`
/// are an isotope mass number (digits), an element symbol (letter), `*`, or
/// `H`. Ring-closure digits never follow `[`. Therefore `[` directly followed
/// by an ASCII digit is an exact isotope test.
pub fn has_isotope(smiles: &[u8]) -> bool {
    let mut iter = smiles.iter().peekable();
    while let Some(&b) = iter.next() {
        if b == b'['
            && let Some(&&next) = iter.peek()
            && next.is_ascii_digit()
        {
            return true;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_multi_component() {
        assert!(is_multi_component(b"CC.O"));
        assert!(is_multi_component(b"[Na+].[Cl-]"));
        assert!(!is_multi_component(b"CCO"));
        assert!(!is_multi_component(b"c1ccccc1"));
    }

    #[test]
    fn detects_isotopes() {
        assert!(has_isotope(b"[13C]CC"));
        assert!(has_isotope(b"[2H]O[2H]"));
        assert!(has_isotope(b"CC[18O]"));
        // Ring closures and plain brackets are not isotopes.
        assert!(!has_isotope(b"C1CCCCC1"));
        assert!(!has_isotope(b"[NH4+]"));
        assert!(!has_isotope(b"[O-]"));
        assert!(!has_isotope(b"c1cc[nH]c1"));
    }
}
